use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Duration,
};

use crate::{
    auth::{InviteLink, Session, decode, decode_bytes, encode},
    identity::{DeviceCertificate, DeviceIdentity, PersonId, fingerprint},
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
    /// Each verified key with its envelope and the person whose device wrapped it.
    keys: HashMap<(Scope, i64), (Key, String, PersonId)>,
    /// What this device knows about each DM channel, pinned once seen so the server
    /// can't relabel it. A DM key is only taken from, and wrapped to, those two people.
    pub(super) dms: HashMap<[u8; 32], DmPin>,
    /// The saved pins were lost: DMs not pinned yet are blocked, not trusted on first use.
    pins_lost: bool,
    /// DMs this session already tried to re-key, so a refusal isn't retried in a loop.
    rekeyed: HashSet<[u8; 32]>,
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
        Self::connect_with_dms(server, identity, session, Some(HashMap::new())).await
    }

    /// Like `connect`, with the DM pins saved earlier, so the first key sync already
    /// applies them. `None` when they couldn't be read: DMs are then blocked.
    pub async fn connect_with_dms(
        server: &'a Server,
        identity: &'a DeviceIdentity,
        session: &Session,
        dms: Option<HashMap<[u8; 32], DmPin>>,
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
            pins_lost: dms.is_none(),
            dms: dms.unwrap_or_default(),
            rekeyed: HashSet::new(),
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
        self.keys.get(&(scope, epoch)).map(|(key, ..)| key)
    }

    /// Every verified epoch key held for a scope.
    #[cfg(feature = "voice")]
    pub(super) fn epoch_keys(&self, scope: Scope) -> impl Iterator<Item = (i64, &Key)> {
        self.keys
            .iter()
            .filter(move |((held, _), _)| *held == scope)
            .map(|((_, epoch), (key, ..))| (*epoch, key))
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
                    .is_some_and(|(_, envelope, _)| *envelope == wrapped.envelope)
                {
                    continue;
                }
                if !self.dm_allows(slot.0, sender.person_id()) {
                    // Not rejected: that would ask the same device to wrap it again.
                    tracing::warn!(
                        device = %fingerprint(sender.signing_key().as_bytes()),
                        "ignored a DM key from someone outside the DM"
                    );
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
                        let person = *sender.person_id();
                        self.keys.insert(slot, (key, wrapped.envelope, person));
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
            refetch |= self.rekey_dms().await?;

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
            if pending.iter().any(|need| need.scope.kind == Kind::Channel) {
                // Learn which channels are DMs before wrapping any channel key.
                self.learn_dms().await?;
            }
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
                    if !self.dm_allows(need.scope, device.person_id()) {
                        tracing::warn!(
                            person = %fingerprint(device.person_id()),
                            device = %fingerprint(device.signing_key().as_bytes()),
                            "refused to wrap a DM key for someone outside the DM"
                        );
                        continue;
                    }
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

    /// Pin a DM channel to the person it is with. `false` when it is pinned to
    /// someone else (the server relabelled it) or blocked: it must not be used.
    pub fn bind_dm(&mut self, channel: [u8; 32], peer: PersonId) -> bool {
        if let Some(pin) = self.dms.get(&channel) {
            return matches!(pin, DmPin::With { peer: bound, .. } if *bound == peer);
        }
        if self.pins_lost {
            self.dms.insert(channel, DmPin::Blocked);
            return false;
        }
        self.dms.insert(channel, DmPin::With { peer, from: None });
        let me = *self.identity.certificate.person_id();
        // Keys taken before this channel was known as a DM must pass the check too.
        self.keys.retain(|(scope, _), (.., person)| {
            scope.kind != Kind::Channel || scope.id != channel || dm_member(&me, &peer, person)
        });
        true
    }

    /// DM channels pinned so far.
    pub fn dms(&self) -> &HashMap<[u8; 32], DmPin> {
        &self.dms
    }

    /// Whether this DM channel can take a message only the two people can read:
    /// pinned to `peer`, re-keyed since, and its current key came from one of them.
    pub fn dm_private(&self, channel: &[u8; 32], peer: &PersonId) -> bool {
        let scope = Scope {
            kind: Kind::Channel,
            id: *channel,
        };
        let me = self.identity.certificate.person_id();
        let Some(DmPin::With {
            peer: bound,
            from: Some(from),
        }) = self.dms.get(channel)
        else {
            return false;
        };
        bound == peer
            && self.current_key(scope).is_ok_and(|(epoch, _)| {
                epoch >= *from
                    && (self.keys.get(&(scope, epoch)))
                        .is_some_and(|(.., person)| dm_member(me, peer, person))
            })
    }

    /// Whether a key of this scope may come from or go to this person: anyone for
    /// a channel or space, only the two people for a pinned DM.
    fn dm_allows(&self, scope: Scope, person: &PersonId) -> bool {
        dm_allows(
            &self.dms,
            self.identity.certificate.person_id(),
            scope,
            person,
        )
    }

    /// Start a new epoch, wrapped only to the pair, on each DM pinned since its last
    /// epoch: an earlier key may have reached others while the channel wasn't known
    /// as a DM. Messages go out only from that epoch on. Returns whether one started.
    async fn rekey_dms(&mut self) -> Result<bool> {
        let mut started = false;
        let readable: Vec<_> = (self.scopes.iter())
            .filter(|state| state.scope.kind == Kind::Channel)
            .map(|state| (state.scope, state.epoch))
            .collect();
        for (scope, epoch) in readable {
            let Some(DmPin::With { from, .. }) = self.dms.get_mut(&scope.id) else {
                continue;
            };
            if from.is_some() || !self.rekeyed.insert(scope.id) {
                continue;
            }
            let next = epoch.checked_add(1).context("epoch overflow")?;
            *from = Some(next);
            // A server without DM re-keying refuses; this DM then stays unused there.
            self.publish_epoch(scope, next).await?;
            started = true;
        }
        Ok(started)
    }

    /// Pin the DMs the catalog lists. One pinned to someone else keeps its pin.
    async fn learn_dms(&mut self) -> Result<()> {
        for channel in self.catalog().await?.channels {
            if channel.kind == "dm"
                && let Some(peer) = channel.peer.as_deref()
                && !self.bind_dm(channel.id, decode(peer)?)
            {
                tracing::warn!("the server relabelled a pinned DM; keeping the pin");
            }
        }
        Ok(())
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
        let me = *self.identity.certificate.person_id();
        self.keys.insert((scope, 1), (key, envelope, me));
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

fn dm_member(me: &PersonId, peer: &PersonId, person: &PersonId) -> bool {
    person == me || person == peer
}

/// What a device knows about a DM channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DmPin {
    /// With this person. Epochs from `from` on were started after the pin, so only
    /// the two people hold them. `None` until it is re-keyed.
    With { peer: PersonId, from: Option<i64> },
    /// Its person is unknown because the saved pins were lost: its keys go to nobody
    /// else and nothing is sent there.
    Blocked,
}

fn dm_allows(
    dms: &HashMap<[u8; 32], DmPin>,
    me: &PersonId,
    scope: Scope,
    person: &PersonId,
) -> bool {
    match dms.get(&scope.id) {
        _ if scope.kind != Kind::Channel => true,
        None => true,
        Some(DmPin::With { peer, .. }) => dm_member(me, peer, person),
        Some(DmPin::Blocked) => person == me,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dm_key_only_comes_from_and_goes_to_the_two_people() {
        let (me, peer, mallory) = ([1; 32], [2; 32], [3; 32]);
        let dm = Scope {
            kind: Kind::Channel,
            id: [9; 32],
        };
        let channel = Scope { id: [8; 32], ..dm };
        let space = Scope {
            kind: Kind::Space,
            id: [9; 32],
        };
        let blocked = Scope { id: [7; 32], ..dm };
        let dms = HashMap::from([
            (dm.id, DmPin::With { peer, from: None }),
            (blocked.id, DmPin::Blocked),
        ]);
        assert!(dm_allows(&dms, &me, dm, &me) && dm_allows(&dms, &me, dm, &peer));
        assert!(!dm_allows(&dms, &me, dm, &mallory));
        // A DM whose pin was lost keeps its keys to this person's own devices.
        assert!(dm_allows(&dms, &me, blocked, &me));
        assert!(!dm_allows(&dms, &me, blocked, &peer));
        // Channels and the space follow the server's membership, as before.
        assert!(dm_allows(&dms, &me, channel, &mallory));
        assert!(dm_allows(&dms, &me, space, &mallory));
    }
}
