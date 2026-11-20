use std::collections::HashMap;

use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use rusttalk_core::{
    auth::{InviteLink, Session, decode, decode_bytes, encode},
    identity::{DeviceCertificate, DeviceIdentity, fingerprint},
    keys::{self, Event, Key, KeyState, Kind, MemberDevice, Metadata, Scope, ScopeState},
    pinned_tls,
};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use crate::membership::Bot;

pub struct KeyClient<'a> {
    bot: &'a Bot,
    pub identity: &'a DeviceIdentity,
    pub session: &'a Session,
    events: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    keys: HashMap<(Scope, i64), Key>,
    devices: HashMap<String, DeviceCertificate>,
    scopes: Vec<ScopeState>,
    pub close_reason: Option<String>,
}

impl<'a> KeyClient<'a> {
    pub async fn connect(
        bot: &'a Bot,
        identity: &'a DeviceIdentity,
        session: &'a Session,
    ) -> Result<Self> {
        let events = pinned_tls::events_socket(&bot.server, &session.token)
            .await
            .map_err(anyhow::Error::from_boxed)?;
        let mut client = Self {
            bot,
            identity,
            session,
            events,
            keys: HashMap::new(),
            devices: HashMap::new(),
            scopes: Vec::new(),
            close_reason: None,
        };
        client.sync().await?;
        Ok(client)
    }

    pub fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.bot
            .request(method, path)
            .bearer_auth(&self.session.token)
    }

    pub fn space(&self) -> Scope {
        Scope {
            kind: Kind::Space,
            id: self.bot.server.server_id,
        }
    }

    pub fn key(&self, scope: Scope, epoch: i64) -> Option<&Key> {
        self.keys.get(&(scope, epoch))
    }

    pub fn encrypt_metadata(&self, kind: Metadata, id: &[u8], plaintext: &[u8]) -> Result<String> {
        let (epoch, key) = self.current_space()?;
        Ok(encode(&kind.seal(id, epoch, key, plaintext)?))
    }

    pub fn decrypt_metadata(&self, kind: Metadata, id: &[u8], encoded: &str) -> Result<Vec<u8>> {
        let bytes = decode_bytes(encoded)?;
        let epoch = kind.epoch(&bytes)?;
        let key = self
            .key(self.space(), epoch)
            .context("missing metadata key")?;
        Ok(kind.open(id, key, &bytes)?)
    }

    fn current_space(&self) -> Result<(i64, &Key)> {
        let current = self
            .scopes
            .iter()
            .find(|state| state.scope == self.space() && !state.rotate)
            .context("space rotation pending")?;
        let key = self
            .key(current.scope, current.epoch)
            .context("missing current space key")?;
        Ok((current.epoch, key))
    }

    /// Reconcile the current directory, missing envelopes and rotations. SQLite decides races.
    pub async fn sync(&mut self) -> Result<()> {
        loop {
            let state: KeyState = self
                .request(Method::GET, "/v1/keys")
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            self.sync_devices().await?;
            for wrapped in state.envelopes {
                let Some(sender) = self.devices.get(&wrapped.sender) else {
                    // A directory change can overtake the key-state request.
                    continue;
                };
                let slot = (wrapped.need.scope, wrapped.need.epoch);
                if let std::collections::hash_map::Entry::Vacant(entry) = self.keys.entry(slot) {
                    match keys::unwrap(
                        slot.0,
                        slot.1,
                        self.identity,
                        sender,
                        &decode_bytes(&wrapped.envelope)?,
                    ) {
                        Ok(key) => {
                            entry.insert(key);
                        }
                        Err(error) => tracing::warn!(reason = %error, "rejected key envelope"),
                    }
                }
            }
            self.scopes = state.scopes;

            let mut refetch = false;
            for scope in &self.scopes {
                if scope.rotate || (scope.epoch == 0 && self.is_owner().await?) {
                    self.publish_epoch(
                        scope.scope,
                        scope.epoch.checked_add(1).context("epoch overflow")?,
                    )
                    .await?;
                    refetch = true;
                }
            }

            for need in state.pending {
                let (Some(key), Some(device)) = (
                    self.key(need.scope, need.epoch),
                    self.devices.get(&need.device),
                ) else {
                    continue;
                };
                let envelope = keys::wrap(need.scope, need.epoch, key, device, self.identity)?;
                tracing::info!(
                    person = %fingerprint(device.person_id()),
                    device = %fingerprint(device.signing_key().as_bytes()),
                    "wrapping key for device"
                );
                let reply = self.request(Method::POST, "/v1/keys")
                    .json(&json!({"scope": need.scope, "epoch": need.epoch, "device": need.device, "envelope": encode(&envelope)}))
                    .send().await?;
                refetch |= key_state_raced(reply).await?;
            }
            if !refetch {
                return Ok(());
            }
        }
    }

    async fn is_owner(&self) -> Result<bool> {
        let me: Value = self
            .request(Method::GET, "/v1/me")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(me["role_ids"]
            .as_array()
            .context("missing roles")?
            .iter()
            .any(|role| role == "owner"))
    }

    async fn publish_epoch(&self, scope: Scope, epoch: i64) -> Result<()> {
        let key = keys::random_key()?;
        let envelope = self.wrap_for_self(scope, epoch, &key)?;
        let reply = self
            .request(Method::POST, "/v1/keys/epochs")
            .json(&json!({"scope": scope, "epoch": epoch, "envelope": envelope}))
            .send()
            .await?;
        key_state_raced(reply).await?;
        Ok(())
    }

    fn wrap_for_self(&self, scope: Scope, epoch: i64, key: &Key) -> Result<String> {
        let cert = &self.identity.certificate;
        tracing::info!(
            person = %fingerprint(cert.person_id()),
            device = %fingerprint(cert.signing_key().as_bytes()),
            "wrapping key for device"
        );
        Ok(encode(&keys::wrap(scope, epoch, key, cert, self.identity)?))
    }

    async fn sync_devices(&mut self) -> Result<()> {
        let listed: Vec<MemberDevice> = self
            .request(Method::GET, "/v1/member-devices")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let mut devices = HashMap::new();
        for device in listed {
            let certificate = DeviceCertificate::verify_public(
                decode(&device.person)?,
                &decode_bytes(&device.certificate)?,
            )?;
            let id = encode(certificate.signing_key().as_bytes());
            if !self.devices.contains_key(&id) {
                tracing::info!(
                    person = %fingerprint(certificate.person_id()),
                    device = %fingerprint(certificate.signing_key().as_bytes()),
                    "member device added"
                );
            }
            devices.insert(id, certificate);
        }
        for (id, certificate) in &self.devices {
            if !devices.contains_key(id) {
                tracing::info!(
                    person = %fingerprint(certificate.person_id()),
                    device = %fingerprint(certificate.signing_key().as_bytes()),
                    "member device removed"
                );
            }
        }
        self.devices = devices;
        Ok(())
    }

    /// Drive the bot by awaiting this method while it is online.
    pub async fn next_event(&mut self) -> Result<Option<Event>> {
        while let Some(frame) = self.events.next().await {
            match frame? {
                Message::Text(text) => {
                    let event = serde_json::from_str(&text)?;
                    if let Event::MemberChanged { person, device } = &event {
                        let person = fingerprint(&decode(person)?);
                        let device = device
                            .as_deref()
                            .map(decode)
                            .transpose()?
                            .as_ref()
                            .map(fingerprint);
                        tracing::info!(%person, device, "member changed");
                    }
                    // A queued notice can precede removal of this session. Let the socket deliver the close reason.
                    if let Err(error) = self.sync().await {
                        if error
                            .downcast_ref::<reqwest::Error>()
                            .and_then(reqwest::Error::status)
                            != Some(StatusCode::UNAUTHORIZED)
                        {
                            return Err(error);
                        }
                        continue;
                    }
                    return Ok(Some(event));
                }
                Message::Close(frame) => {
                    self.close_reason = frame.map(|frame| frame.reason.to_string());
                    return Ok(None);
                }
                _ => {}
            }
        }
        Ok(None)
    }

    pub async fn set_server_name(&self, name: &[u8]) -> Result<()> {
        let name_ct = self.encrypt_metadata(Metadata::ServerName, &self.space().id, name)?;
        self.request(Method::PUT, "/v1/space")
            .json(&json!({"name_ct": name_ct}))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn set_display_name(&self, name: &[u8]) -> Result<()> {
        let name_ct = self.encrypt_metadata(
            Metadata::DisplayName,
            self.identity.certificate.person_id(),
            name,
        )?;
        self.request(Method::PUT, "/v1/me/profile")
            .json(&json!({"display_name_ct": name_ct}))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn create_category(&self, name: &[u8]) -> Result<[u8; 32]> {
        let id = *keys::random_key()?;
        let name_ct = self.encrypt_metadata(Metadata::CategoryName, &id, name)?;
        self.request(Method::POST, "/v1/categories")
            .json(&json!({"id": encode(&id), "position": 0, "name_ct": name_ct}))
            .send()
            .await?
            .error_for_status()?;
        Ok(id)
    }

    pub async fn create_channel(
        &mut self,
        category: &[u8; 32],
        kind: &str,
        name: &[u8],
        topic: &[u8],
    ) -> Result<Scope> {
        let id = *keys::random_key()?;
        let scope = Scope {
            kind: Kind::Channel,
            id,
        };
        let name_ct = self.encrypt_metadata(Metadata::ChannelName, &id, name)?;
        let topic_ct = self.encrypt_metadata(Metadata::ChannelTopic, &id, topic)?;
        let key = keys::random_key()?;
        let envelope = self.wrap_for_self(scope, 1, &key)?;
        self.request(Method::POST, "/v1/channels")
            .json(&json!({
                "id": encode(&id), "kind": kind, "category_id": encode(category), "position": 0,
                "name_ct": name_ct, "topic_ct": topic_ct, "envelope": envelope,
            }))
            .send()
            .await?
            .error_for_status()?;
        self.keys.insert((scope, 1), key);
        Ok(scope)
    }

    pub async fn invite(&mut self, uses: u32, ttl_seconds: u32) -> Result<(String, InviteLink)> {
        self.sync().await?;
        let (epoch, key) = self.current_space()?;
        let link_key = keys::random_key()?;
        let server = &self.bot.server;
        let space_key_ct = encode(&keys::seal_invite(
            &server.server_id,
            epoch,
            key,
            &link_key,
        )?);
        let metadata: Value = self
            .request(Method::GET, "/v1/space")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let name_ct = metadata["name_ct"]
            .as_str()
            .map(|ct| -> Result<_> {
                let name = self.decrypt_metadata(Metadata::ServerName, &server.server_id, ct)?;
                self.encrypt_metadata(Metadata::ServerName, &server.server_id, &name)
            })
            .transpose()?;
        let invite: Value = self.request(Method::POST, "/v1/invites")
            .json(&json!({"uses": uses, "ttl_seconds": ttl_seconds, "space_key_ct": space_key_ct, "name_ct": name_ct}))
            .send().await?.error_for_status()?.json().await?;
        Ok((
            invite["id"]
                .as_str()
                .context("missing invite id")?
                .to_owned(),
            InviteLink {
                server: server.clone(),
                code: decode(invite["code"].as_str().context("missing invite code")?)?,
                link_key: Some(link_key),
            },
        ))
    }
}

async fn key_state_raced(reply: Response) -> Result<bool> {
    let Err(error) = reply.error_for_status_ref() else {
        return Ok(false);
    };
    if reply.status() == StatusCode::CONFLICT
        || (reply.status() == StatusCode::FORBIDDEN
            && matches!(
                reply.json::<Value>().await?["error"].as_str(),
                Some("scope_read_denied" | "recipient_read_denied")
            ))
    {
        tracing::info!("key publication raced another key or an access change");
        return Ok(true);
    }
    Err(error.into())
}

pub async fn invite_name(bot: &Bot, link: &InviteLink) -> Result<Option<Vec<u8>>> {
    ensure!(
        bot.server == link.server,
        "invite pins differ from the established server"
    );
    let key = link
        .link_key
        .as_ref()
        .context("invite has no preview key")?;
    let reply: Value = bot
        .request(Method::POST, "/v1/invite-preview")
        .json(&json!({"code": encode(&link.code)}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let ct = decode_bytes(
        reply["space_key_ct"]
            .as_str()
            .context("missing invite key data")?,
    )?;
    let (epoch, space) = keys::open_invite(&link.server.server_id, key, &ct)?;
    reply["name_ct"]
        .as_str()
        .map(|ct| {
            let ct = decode_bytes(ct)?;
            ensure!(
                Metadata::ServerName.epoch(&ct)? == epoch,
                "invite metadata epoch differs"
            );
            Ok(Metadata::ServerName.open(&link.server.server_id, &space, &ct)?)
        })
        .transpose()
}
