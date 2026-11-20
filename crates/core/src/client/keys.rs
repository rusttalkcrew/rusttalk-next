use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Duration,
};

use crate::{
    auth::{InviteLink, Session, decode, decode_bytes, encode},
    identity::{DeviceCertificate, DeviceIdentity, fingerprint},
    keys::{
        self, EnvelopeContext, Event, Key, KeyState, Kind, MemberDevice, Metadata, Scope,
        ScopeState,
    },
    pinned_tls,
    presence::{Status, Update},
};
use anyhow::{Context, Result, ensure};
use futures_util::{FutureExt, StreamExt};
use rand::{SeedableRng, rngs::StdRng, seq::SliceRandom};
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use super::{Server, chat::Opened};

type HistoricalDevice = ([u8; 32], [u8; 32], [u8; 32]);

/// Wraps between refreshes of the still-missing set.
const WRAP_BATCH: usize = 20;

/// An authenticated client with a live event stream and an in-memory epoch key store.
pub struct KeyClient<'a> {
    pub(super) server: &'a Server,
    /// The authenticated device identity.
    pub identity: &'a DeviceIdentity,
    session: Session,
    events: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    keys: HashMap<(Scope, i64), (Key, String)>,
    pub(super) devices: HashMap<String, DeviceCertificate>,
    pub(super) historical_devices: HashMap<HistoricalDevice, DeviceCertificate>,
    scopes: Vec<ScopeState>,
    pub(super) opened: HashMap<[u8; 32], Opened>,
    /// Received events not yet returned, and whether key state must sync before returning them.
    queued: VecDeque<Update>,
    stale: bool,
    closed: bool,
    /// The server's close reason after the event stream terminates.
    pub close_reason: Option<String>,
}

impl<'a> KeyClient<'a> {
    /// Open the pinned event stream and reconcile envelope and device state.
    pub async fn connect(
        server: &'a Server,
        identity: &'a DeviceIdentity,
        session: &Session,
    ) -> Result<Self> {
        let events = pinned_tls::events_socket(server.known_server(), &session.token)
            .await
            .map_err(anyhow::Error::from_boxed)?;
        let mut client = Self {
            server,
            identity,
            session: session.clone(),
            events,
            keys: HashMap::new(),
            devices: HashMap::new(),
            historical_devices: HashMap::new(),
            scopes: Vec::new(),
            opened: HashMap::new(),
            queued: VecDeque::new(),
            stale: false,
            closed: false,
            close_reason: None,
        };
        client.sync().await?;
        Ok(client)
    }

    /// Return the expiry time of the active bearer token as Unix seconds.
    pub fn expires_at(&self) -> i64 {
        self.session.expires_at
    }

    /// Rotate the bearer token on the current session, keeping the event stream open.
    pub async fn refresh(&mut self) -> Result<()> {
        self.session = self.server.refresh(&self.session).await?;
        Ok(())
    }

    /// Build an authenticated API request for the pinned server.
    pub fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.server
            .request(method, path)
            .bearer_auth(&self.session.token)
    }

    /// Borrow this pinned server and its current session for device sync.
    pub fn sync_peer(&self) -> super::sync::Peer<'_> {
        (self.server, &self.session)
    }

    /// Return this server’s space scope.
    pub fn space(&self) -> Scope {
        Scope {
            kind: Kind::Space,
            id: self.server.known_server().server_id,
        }
    }

    /// Borrow a verified key for any known epoch of a readable scope.
    pub fn key(&self, scope: Scope, epoch: i64) -> Option<&Key> {
        self.keys.get(&(scope, epoch)).map(|(key, _)| key)
    }

    /// Every verified epoch key held for a scope.
    #[cfg(feature = "voice")]
    pub(super) fn epoch_keys(&self, scope: Scope) -> impl Iterator<Item = (i64, &Key)> {
        self.keys
            .iter()
            .filter(move |((held, _), _)| *held == scope)
            .map(|((_, epoch), (key, _))| (*epoch, key))
    }

    /// Seal metadata with the current space key and return base64url ciphertext.
    pub fn encrypt_metadata(&self, kind: Metadata, id: &[u8], plaintext: &[u8]) -> Result<String> {
        let (epoch, key) = self.current_space()?;
        Ok(encode(&kind.seal(
            &self.server.known_server().server_id,
            id,
            epoch,
            key,
            plaintext,
        )?))
    }

    /// Open base64url metadata with the space key for its recorded epoch.
    pub fn decrypt_metadata(&self, kind: Metadata, id: &[u8], encoded: &str) -> Result<Vec<u8>> {
        let bytes = decode_bytes(encoded)?;
        let epoch = kind.epoch(&bytes)?;
        let key = self
            .key(self.space(), epoch)
            .context("missing metadata key")?;
        Ok(kind.open(&self.server.known_server().server_id, id, key, &bytes)?)
    }

    /// Whether the current space key is here, so names can be encrypted.
    pub fn has_space_key(&self) -> bool {
        self.current_space().is_ok()
    }

    fn current_space(&self) -> Result<(i64, &Key)> {
        self.current_key(self.space())
    }

    /// The latest verified key once the scope is ready for use.
    pub fn current_key(&self, scope: Scope) -> Result<(i64, &Key)> {
        let current = self
            .scopes
            .iter()
            .find(|state| state.scope == scope && !state.rotate)
            .context("scope key unavailable or rotation pending")?;
        let key = self
            .key(current.scope, current.epoch)
            .context("missing current scope key")?;
        Ok((current.epoch, key))
    }

    /// Reconcile the current directory, missing envelopes and rotations. SQLite decides races.
    pub async fn sync(&mut self) -> Result<()> {
        loop {
            let state = self.key_state().await?;
            self.sync_devices().await?;
            let mut refetch = false;
            for wrapped in state.envelopes {
                let Some(sender) = self.devices.get(&wrapped.sender) else {
                    // A directory change can overtake the key-state request.
                    continue;
                };
                let slot = (wrapped.need.scope, wrapped.need.epoch);
                if self
                    .keys
                    .get(&slot)
                    .is_some_and(|(_, envelope)| *envelope == wrapped.envelope)
                {
                    continue;
                }
                let context = EnvelopeContext {
                    server_id: self.server.known_server().server_id,
                    scope: slot.0,
                    epoch: slot.1,
                    commitment: wrapped.commitment,
                };
                let opened = decode_bytes(&wrapped.envelope)
                    .map_err(|_| keys::KeyError::Encoding)
                    .and_then(|bytes| context.unwrap(self.identity, sender, &bytes));
                match opened {
                    Ok(key) => {
                        self.keys.insert(slot, (key, wrapped.envelope));
                    }
                    Err(error) => {
                        tracing::warn!(
                            reason = %error,
                            device = %fingerprint(sender.signing_key().as_bytes()),
                            "rejected key envelope"
                        );
                        let reply = self.request(Method::POST, "/v1/keys/reject")
                            .json(&json!({"scope": slot.0, "epoch": slot.1, "sender": wrapped.sender, "envelope": wrapped.envelope}))
                            .send().await?;
                        key_state_raced(reply).await?;
                        refetch = true;
                    }
                }
            }
            let readable: HashSet<_> = state.scopes.iter().map(|state| state.scope).collect();
            self.keys.retain(|(scope, _), _| readable.contains(scope));
            self.historical_devices.retain(|(id, _, _), _| {
                readable.contains(&Scope {
                    kind: Kind::Channel,
                    id: *id,
                })
            });
            self.scopes = state.scopes;

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

            let mut pending = state.pending;
            pending.retain(|need| {
                self.key(need.scope, need.epoch).is_some()
                    && self.devices.contains_key(&need.device)
            });
            if !pending.is_empty() {
                // Every online member sees the same need; a random pause lets one wrap and the rest skip.
                let [a, b, ..] = *keys::random_key()?;
                let pause = u64::from(u16::from_le_bytes([a, b])) % 3001;
                tokio::time::sleep(Duration::from_millis(pause)).await;
                let fresh = self.key_state().await?.pending;
                pending.retain(|need| fresh.contains(need));
            }
            // Shuffled so members that wake together start on different devices, and
            // refreshed every 20 wraps to skip needs another member already met.
            pending.shuffle(&mut StdRng::from_seed(*keys::random_key()?));
            while !pending.is_empty() {
                let rest = pending.split_off(pending.len().min(WRAP_BATCH));
                for need in pending {
                    let (Some(key), Some(device)) = (
                        self.key(need.scope, need.epoch),
                        self.devices.get(&need.device),
                    ) else {
                        continue;
                    };
                    let context = EnvelopeContext::new(
                        self.server.known_server().server_id,
                        need.scope,
                        need.epoch,
                        key,
                    );
                    let envelope = context.wrap(key, device, self.identity)?;
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
                pending = rest;
                if !pending.is_empty() {
                    let fresh = self.key_state().await?.pending;
                    pending.retain(|need| fresh.contains(need));
                }
            }
            if !refetch {
                return Ok(());
            }
        }
    }

    async fn key_state(&self) -> Result<KeyState> {
        Ok(self
            .request(Method::GET, "/v1/keys")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
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
        let context =
            EnvelopeContext::new(self.server.known_server().server_id, scope, epoch, &key);
        let envelope = self.wrap_for_self(context, &key)?;
        let reply = self
            .request(Method::POST, "/v1/keys/epochs")
            .json(&json!({"scope": scope, "epoch": epoch, "commitment": encode(&context.commitment), "envelope": envelope}))
            .send()
            .await?;
        key_state_raced(reply).await?;
        Ok(())
    }

    pub(super) fn wrap_for_self(&self, context: EnvelopeContext, key: &Key) -> Result<String> {
        let cert = &self.identity.certificate;
        tracing::info!(
            person = %fingerprint(cert.person_id()),
            device = %fingerprint(cert.signing_key().as_bytes()),
            "wrapping key for device"
        );
        Ok(encode(&context.wrap(key, cert, self.identity)?))
    }

    /// Iterate current verified member-device certificates for fingerprint display.
    pub fn devices(&self) -> impl Iterator<Item = &DeviceCertificate> {
        self.devices.values()
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

    /// Await the next event and reconcile key work when needed.
    pub async fn next_event(&mut self) -> Result<Option<Event>> {
        while let Some(update) = self.next_update().await? {
            if let Update::Event(event) = update {
                return Ok(Some(event));
            }
        }
        Ok(None)
    }

    /// Await the next API event, including presence changes.
    pub async fn next_update(&mut self) -> Result<Option<Update>> {
        loop {
            if self.stale && !self.queued.is_empty() {
                match self.sync().await {
                    Ok(()) => self.stale = false,
                    // A queued notice can precede removal of this session. Wait for the close frame.
                    Err(error)
                        if error
                            .downcast_ref::<reqwest::Error>()
                            .and_then(reqwest::Error::status)
                            == Some(StatusCode::UNAUTHORIZED) =>
                    {
                        self.queued.clear();
                        self.stale = false;
                    }
                    Err(error) => return Err(error),
                }
            }
            if let Some(event) = self.queued.pop_front() {
                return Ok(Some(event));
            }
            if self.closed {
                return Ok(None);
            }
            let Some(frame) = self.events.next().await else {
                return Ok(None);
            };
            self.receive(frame?)?;
            // A join queues one notice per missing envelope; one sync covers the whole burst.
            while !self.closed
                && let Some(Some(frame)) = self.events.next().now_or_never()
            {
                self.receive(frame?)?;
            }
        }
    }

    fn receive(&mut self, frame: Message) -> Result<()> {
        match frame {
            Message::Text(text) => {
                let update = serde_json::from_str(&text)?;
                if let Update::Event(Event::MemberChanged { person, device }) = &update {
                    let person = fingerprint(&decode(person)?);
                    let device = device
                        .as_deref()
                        .map(decode)
                        .transpose()?
                        .as_ref()
                        .map(fingerprint);
                    tracing::info!(%person, device, "member changed");
                }
                self.stale |= !matches!(
                    update,
                    Update::Presence(_)
                        | Update::Event(
                            Event::MessageCreated { .. }
                                | Event::MessageDeleted { .. }
                                | Event::VoiceChanged { .. }
                                | Event::LiveFileAccepted { .. }
                        )
                );
                self.queued.push_back(update);
            }
            Message::Close(frame) => {
                self.close_reason = frame.map(|frame| frame.reason.to_string());
                self.closed = true;
            }
            _ => {}
        }
        Ok(())
    }

    /// Set the presence choice for this member’s open event sockets.
    pub async fn set_status(&self, status: Status) -> Result<()> {
        self.request(Method::PUT, "/v1/me/status")
            .json(&json!({"status": status}))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Encrypt and replace the server name.
    pub async fn set_server_name(&self, name: &[u8]) -> Result<()> {
        let name_ct = self.encrypt_metadata(Metadata::ServerName, &self.space().id, name)?;
        self.request(Method::PUT, "/v1/space")
            .json(&json!({"name_ct": name_ct}))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Encrypt and replace this member’s display name.
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

    /// Create a category with an encrypted name.
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

    /// Create a text or voice channel and initialize its first key epoch.
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
        let context = EnvelopeContext::new(self.server.known_server().server_id, scope, 1, &key);
        let envelope = self.wrap_for_self(context, &key)?;
        self.request(Method::POST, "/v1/channels")
            .json(&json!({
                "id": encode(&id), "kind": kind, "category_id": encode(category), "position": 0,
                "name_ct": name_ct, "topic_ct": topic_ct, "commitment": encode(&context.commitment), "envelope": envelope,
            }))
            .send()
            .await?
            .error_for_status()?;
        self.keys.insert((scope, 1), (key, envelope));
        self.scopes.push(ScopeState {
            scope,
            epoch: 1,
            rotate: false,
        });
        Ok(scope)
    }

    /// Create an invite with the current space key and optional name snapshot.
    pub async fn invite(&mut self, uses: u32, ttl_seconds: u32) -> Result<(String, InviteLink)> {
        self.sync().await?;
        let (epoch, key) = self.current_space()?;
        let link_key = keys::random_key()?;
        let server = self.server.known_server();
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

/// Decrypt the optional server name carried by a verified invite link.
pub async fn invite_name(server: &Server, link: &InviteLink) -> Result<Option<Vec<u8>>> {
    ensure!(
        server.known_server() == &link.server,
        "invite pins differ from the established server"
    );
    let key = link
        .link_key
        .as_ref()
        .context("invite has no preview key")?;
    let reply: Value = server
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
            Ok(Metadata::ServerName.open(
                &link.server.server_id,
                &link.server.server_id,
                &space,
                &ct,
            )?)
        })
        .transpose()
}
