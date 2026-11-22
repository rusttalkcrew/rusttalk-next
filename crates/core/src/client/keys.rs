mod rotation;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Duration,
};

use crate::{
    auth::{InviteLink, Session, decode, decode_bytes, encode},
    identity::{DeviceCertificate, DeviceIdentity, PersonId, fingerprint},
    keys::{
        self, EnvelopeContext, Event, Excluded, Key, KeyState, Kind, MemberDevice, Metadata, Scope,
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

use super::{MessageBody, Server, chat::Opened};
use crate::messages::KeyLink;

type EventStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

type HistoricalDevice = ([u8; 32], [u8; 32], [u8; 32]);
/// A verified epoch key, its envelope, the person who wrapped it, its commitment,
/// and whether that commitment names the pinned pair (a DM key made for them).
struct HeldKey {
    key: Key,
    envelope: String,
    sender: PersonId,
    commitment: [u8; 32],
    excluded: Excluded,
    pair_bound: bool,
}

/// Wraps between refreshes of the still-missing set.
const WRAP_BATCH: usize = 20;

/// An authenticated client with a live event stream and an in-memory epoch key store.
pub struct KeyClient<'a> {
    pub(super) server: &'a Server,
    /// The authenticated device identity.
    pub identity: &'a DeviceIdentity,
    pub(super) revocations: &'a super::revocations::Revocations,
    changes: tokio::sync::watch::Receiver<()>,
    checked_removals: bool,
    create_protected_epochs: bool,
    update_required: HashSet<Scope>,
    rotation_retry: Option<std::time::Instant>,
    own_revocations_changed: bool,
    session: Session,
    events: EventStream,
    /// Each verified key with its envelope, the person whose device wrapped it, and
    /// its epoch commitment.
    keys: HashMap<(Scope, i64), HeldKey>,
    /// What this device knows about each DM channel, pinned once seen so the server
    /// can't relabel it. A DM key is only taken from, and wrapped to, those two people.
    pub(super) dms: HashMap<[u8; 32], DmPin>,
    /// The saved pins were lost: DMs not pinned yet are blocked, not trusted on first use.
    pins_lost: bool,
    /// DM epochs not yet trusted that this session tried to recover: attempts and
    /// when to try again, so a refusal isn't retried in a loop but a delay is.
    rekeyed: HashMap<([u8; 32], i64), (u32, std::time::Instant)>,
    /// Timer-driven syncs that failed in a row, to back off their retries too.
    rekey_failures: u32,
    /// Key links opened in each pinned DM, sent by one of its two people: the epoch
    /// each was sent in, and the link.
    pub(super) links: HashMap<[u8; 32], Vec<(i64, KeyLink)>>,
    pub(super) devices: HashMap<String, DeviceCertificate>,
    pub(super) historical_devices: HashMap<HistoricalDevice, DeviceCertificate>,
    rotations: rotation::Rotations,
    rotation_save_pending: bool,
    former_devices: HashSet<(PersonId, [u8; 32])>,
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
        let mut client = Self::from_stream(server, identity, session, dms, events)?;
        client.sync().await?;
        Ok(client)
    }

    fn from_stream(
        server: &'a Server,
        identity: &'a DeviceIdentity,
        session: &Session,
        dms: Option<HashMap<[u8; 32], DmPin>>,
        events: EventStream,
    ) -> Result<Self> {
        let revocations = server
            .revocations
            .as_ref()
            .context("key sync needs a connected profile")?;
        let create_protected_epochs = server.protocol >= 3;
        Ok(Self {
            server,
            identity,
            revocations,
            changes: revocations.changes(),
            checked_removals: false,
            create_protected_epochs,
            update_required: Default::default(),
            rotation_retry: None,
            own_revocations_changed: false,
            session: session.clone(),
            events,
            keys: HashMap::new(),
            pins_lost: dms.is_none(),
            dms: dms.unwrap_or_default(),
            rekeyed: HashMap::new(),
            rekey_failures: 0,
            links: HashMap::new(),
            devices: HashMap::new(),
            historical_devices: HashMap::new(),
            rotations: if create_protected_epochs {
                rotation::Rotations::load(
                    &revocations.rotation_file(&server.known_server().server_id),
                )?
            } else {
                Default::default()
            },
            rotation_save_pending: false,
            former_devices: Default::default(),
            scopes: Vec::new(),
            opened: HashMap::new(),
            queued: VecDeque::new(),
            stale: false,
            closed: false,
            close_reason: None,
        })
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
        self.keys.get(&(scope, epoch)).map(|held| &held.key)
    }

    /// Every verified epoch key held for a scope.
    #[cfg(feature = "voice")]
    pub(super) fn epoch_keys(&self, scope: Scope) -> impl Iterator<Item = (i64, &Key)> {
        self.keys
            .iter()
            .filter(move |((held, _), _)| *held == scope)
            .map(|((_, epoch), held)| (*epoch, &held.key))
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
        ensure!(
            !self.create_protected_epochs
                || (self.checked_removals && !self.changes.has_changed()?),
            "checking device removals"
        );
        ensure!(
            !self.update_required.contains(&scope),
            crate::messages::MessageError::ServerUpdateRequired
        );
        let current = self
            .scopes
            .iter()
            .find(|state| state.scope == scope && !state.rotate)
            .context("scope key unavailable or rotation pending")?;
        let held = self
            .keys
            .get(&(scope, current.epoch))
            .context("missing current scope key")?;
        ensure!(
            current.excluded.is_subset(&held.excluded)
                && self.rotations.covered(scope, &held.excluded),
            "waiting for device removal rotation"
        );
        Ok((current.epoch, &held.key))
    }

    /// Reconcile the current directory, missing envelopes and rotations. SQLite decides races.
    pub async fn sync(&mut self) -> Result<()> {
        let mut waited = false;
        let mut attempted = HashSet::new();
        loop {
            let state = self.key_state().await?;
            self.checked_removals = false;
            self.changes.borrow_and_update();
            let sibling =
                self.create_protected_epochs && self.find_rotations(&state.scopes).await?;
            self.checked_removals = true;
            let missing: HashSet<_> = state
                .scopes
                .iter()
                .filter(|s| {
                    !self.rotations.covered(s.scope, &s.excluded)
                        || self
                            .keys
                            .get(&(s.scope, s.epoch))
                            .is_some_and(|held| !self.rotations.covered(s.scope, &held.excluded))
                })
                .map(|s| s.scope)
                .collect();
            if !missing.is_empty() && state.supports_exclusions && !waited {
                // Siblings get a head start. Every eligible online reader can take over.
                waited = true;
                let [a, b, ..] = *keys::random_key()?;
                let jitter = u64::from(u16::from_le_bytes([a, b])) % 1000;
                tokio::time::sleep(Duration::from_millis(if sibling {
                    jitter / 4
                } else {
                    250 + jitter
                }))
                .await;
                continue;
            }

            let mut refetch = false;
            let mut learned = false;
            for wrapped in state.envelopes {
                let Some(sender) = self.device(&wrapped.sender) else {
                    // A directory change can overtake the key-state request.
                    continue;
                };
                if wrapped.excluded.contains(&keys::certificate_id(sender))
                    || wrapped
                        .excluded
                        .contains(&keys::certificate_id(&self.identity.certificate))
                {
                    continue;
                }
                let slot = (wrapped.need.scope, wrapped.need.epoch);
                if self
                    .keys
                    .get(&slot)
                    .is_some_and(|held| held.envelope == wrapped.envelope)
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
                let sender = sender.clone();
                let mut opened = decode_bytes(&wrapped.envelope)
                    .map_err(|_| keys::KeyError::Encoding)
                    .and_then(|bytes| context.open(self.identity, &sender, &bytes))
                    .map(|key| {
                        let bound = self.pair_bound(&context, &key, &wrapped.excluded);
                        (key, bound)
                    });
                // A DM commitment needs its pair: learn the DMs once, then try again.
                if let Ok((key, None)) = &opened
                    && slot.0.kind == Kind::Channel
                    && !learned
                {
                    learned = true;
                    let key = key.clone();
                    self.learn_dms().await?;
                    let bound = self.pair_bound(&context, &key, &wrapped.excluded);
                    opened = Ok((key, bound));
                }
                match opened {
                    Ok((key, Some(bound))) => {
                        let person = *sender.person_id();
                        let entry = HeldKey {
                            key,
                            envelope: wrapped.envelope,
                            sender: person,
                            commitment: wrapped.commitment,
                            excluded: wrapped.excluded,
                            pair_bound: bound,
                        };
                        self.keys.insert(slot, entry);
                    }
                    Ok((_, None)) if matches!(self.dms.get(&slot.0.id), Some(DmPin::Blocked)) => {
                        // A DM key for a pair this device can't name now; not rejected.
                    }
                    Ok((_, None)) => {
                        tracing::warn!(
                            reason = %keys::KeyError::Commitment,
                            device = %fingerprint(sender.signing_key().as_bytes()),
                            "rejected key envelope"
                        );
                        let reply = self.request(Method::POST, "/v1/keys/reject")
                            .json(&json!({"scope": slot.0, "epoch": slot.1, "sender": wrapped.sender, "envelope": wrapped.envelope}))
                            .send().await?;
                        key_state_raced(reply).await?;
                        refetch = true;
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
            self.update_required = if state.supports_exclusions {
                Default::default()
            } else {
                missing.clone()
            };
            for current in &mut self.scopes {
                current.rotate |= missing.contains(&current.scope);
            }
            refetch |= self.rekey_dms().await?;

            let states: Vec<_> = (self.scopes.iter())
                .map(|state| (state.scope, state.epoch, state.rotate))
                .collect();
            let mut retry = false;
            for (scope, epoch, rotate) in states {
                if rotate || (epoch == 0 && self.is_owner().await?) {
                    if self.create_protected_epochs
                        && scope.kind == Kind::Channel
                        && matches!(self.dms.get(&scope.id), Some(DmPin::Blocked))
                    {
                        continue;
                    }
                    if self.update_required.contains(&scope) || !self.may_create_epoch(scope) {
                        continue;
                    }
                    if !attempted.insert(scope) {
                        retry = true;
                        continue;
                    }
                    let next = epoch.checked_add(1).context("epoch overflow")?;
                    let key = keys::random_key()?;
                    let commitment = self.publish_key(scope, next, &key).await?;
                    // A DM epoch this device starts is new; the other person re-keys
                    // from it if they can't tell.
                    if let (Some(commitment), Some(DmPin::With { anchor, .. })) =
                        (commitment, self.dms.get_mut(&scope.id))
                    {
                        *anchor = Some((next, commitment));
                    }
                    refetch = true;
                }
            }

            self.rotation_retry = retry.then(|| std::time::Instant::now() + backoff(1));
            let mut pending = state.pending;
            if pending.iter().any(|need| need.scope.kind == Kind::Channel) {
                // Learn which channels are DMs before wrapping any channel key.
                self.learn_dms().await?;
            }
            pending.retain(|need| {
                self.keys
                    .get(&(need.scope, need.epoch))
                    .zip(self.device(&need.device))
                    .is_some_and(|(held, device)| {
                        !held.excluded.contains(&keys::certificate_id(device))
                    })
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
                    let (Some(held), Some(device)) = (
                        self.keys.get(&(need.scope, need.epoch)),
                        self.device(&need.device),
                    ) else {
                        continue;
                    };
                    let me = self.identity.certificate.person_id();
                    if held.excluded.contains(&keys::certificate_id(device)) {
                        continue;
                    }
                    if !may_wrap(
                        &self.dms,
                        me,
                        need.scope,
                        device.person_id(),
                        held.pair_bound,
                    ) {
                        tracing::warn!(
                            person = %fingerprint(device.person_id()),
                            device = %fingerprint(device.signing_key().as_bytes()),
                            "refused to wrap a DM key: outside the DM, or not made for this pair"
                        );
                        continue;
                    }
                    let key = &held.key;
                    // The epoch's own commitment, which the API checks; a DM's names its pair.
                    let context = EnvelopeContext {
                        server_id: self.server.known_server().server_id,
                        scope: need.scope,
                        epoch: need.epoch,
                        commitment: held.commitment,
                    };
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
            if !refetch && !self.changes.has_changed()? {
                return Ok(());
            }
        }
    }

    async fn key_state(&mut self) -> Result<KeyState> {
        let state: KeyState = self
            .request(Method::GET, "/v1/keys")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            state.revocations.len() <= super::revocations::MAX_PER_REPLY,
            "oversized key-state revocation list"
        );
        for excluded in state
            .scopes
            .iter()
            .map(|s| &s.excluded)
            .chain(state.envelopes.iter().map(|e| &e.excluded))
        {
            ensure!(
                excluded.len() <= super::revocations::MAX_PER_REPLY
                    && excluded
                        .iter()
                        .all(
                            |d| d.split_once('.').is_some_and(|(owner, key)| [owner, key]
                                .iter()
                                .all(|s| s.len() == 43 && decode::<32>(s).is_ok()))
                        ),
                "invalid epoch exclusions"
            );
        }
        self.sync_devices().await?;
        let me = self.identity.certificate.person_id();
        let mut people: HashSet<_> = self
            .devices
            .values()
            .map(|cert| *cert.person_id())
            .collect();
        people.extend(self.dms.values().filter_map(|pin| match pin {
            DmPin::With { peer, .. } => Some(*peer),
            DmPin::Blocked => None,
        }));
        people.insert(*me);
        for (person, record) in &state.revocations {
            let Ok(person) = decode(person) else {
                tracing::warn!("ignored invalid device revocation owner");
                continue;
            };
            if !people.contains(&person) {
                continue;
            }
            let Ok(record) = super::revocations::verify(&person, record) else {
                tracing::warn!("ignored invalid device revocation");
                continue;
            };
            let added = self.revocations.remember(&record)?;
            if &person == me {
                self.server.note_revocation(&record);
                self.own_revocations_changed |= added;
            }
        }
        Ok(state)
    }

    /// Keep a locally issued revocation before contacting any server, so sync can retry it.
    pub async fn revoke_device(&mut self, record: &crate::identity::Revocation) -> Result<()> {
        ensure!(
            record.person_id() == self.identity.certificate.person_id(),
            "wrong revocation owner"
        );
        self.revocations.remember(record)?;
        self.own_revocations_changed = true;
        self.server.publish_revocation(&self.session, record).await
    }

    /// The owner's device sync must relay newly observed records to its other servers.
    pub fn take_own_revocations_changed(&mut self) -> bool {
        std::mem::take(&mut self.own_revocations_changed)
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

    fn may_create_epoch(&self, scope: Scope) -> bool {
        self.create_protected_epochs
            || !self.scopes.iter().any(|s| {
                s.scope == scope
                    && (!s.excluded.is_empty()
                        || self
                            .keys
                            .get(&(scope, s.epoch))
                            .is_some_and(|held| !held.excluded.is_empty()))
            })
    }

    /// Start an epoch with this key. Its commitment, or `None` when another epoch or
    /// an access change won the race (or the server refused it).
    async fn publish_key(&self, scope: Scope, epoch: i64, key: &Key) -> Result<Option<[u8; 32]>> {
        let (excluded, revocations) = self.rotation_proposal(scope);
        if self
            .scopes
            .iter()
            .any(|s| s.scope == scope && !s.excluded.is_subset(&excluded))
        {
            tracing::warn!("waiting for authenticated prior epoch exclusions");
            return Ok(None);
        }
        let context = self.epoch_context(scope, epoch, key).excluding(&excluded);
        ensure!(
            !excluded.contains(&keys::certificate_id(&self.identity.certificate)),
            "device excluded from epoch"
        );
        let envelope = self.wrap_for_self(context, key)?;
        let reply = self
            .request(Method::POST, "/v1/keys/epochs")
            .json(&json!({"scope": scope, "epoch": epoch, "commitment": encode(&context.commitment), "envelope": envelope, "excluded": excluded, "revocations": revocations}))
            .send()
            .await?;
        Ok((!key_state_raced(reply).await?).then_some(context.commitment))
    }

    /// A new epoch's context: a pinned DM's commitment names the two people.
    pub(super) fn epoch_context(&self, scope: Scope, epoch: i64, key: &Key) -> EnvelopeContext {
        let server = self.server.known_server().server_id;
        match self.dms.get(&scope.id) {
            Some(DmPin::With { peer, .. }) if scope.kind == Kind::Channel => {
                let me = *self.identity.certificate.person_id();
                EnvelopeContext::new_dm(server, scope, epoch, key, keys::dm_pair(me, *peer))
            }
            _ => EnvelopeContext::new(server, scope, epoch, key),
        }
    }

    /// Which commitment an opened envelope has: `Some(true)` for a DM key made for
    /// the pinned pair, `Some(false)` for a plain one (an ordinary channel's, or a DM
    /// epoch from before pair binding), `None` for neither, as for a DM key made
    /// for another pair or for a DM not pinned yet.
    fn pair_bound(
        &self,
        context: &EnvelopeContext,
        key: &Key,
        excluded: &Excluded,
    ) -> Option<bool> {
        let server = context.server_id;
        let scope = context.scope;
        let epoch = context.epoch;
        if let Some(DmPin::With { peer, .. }) = self.dms.get(&scope.id)
            && scope.kind == Kind::Channel
            && EnvelopeContext::new_dm(
                server,
                scope,
                epoch,
                key,
                keys::dm_pair(*self.identity.certificate.person_id(), *peer),
            )
            .excluding(excluded)
            .commitment
                == context.commitment
        {
            return Some(true);
        }
        (EnvelopeContext::new(server, scope, epoch, key)
            .excluding(excluded)
            .commitment
            == context.commitment)
            .then_some(false)
    }

    pub(super) fn wrap_for_self(&self, context: EnvelopeContext, key: &Key) -> Result<String> {
        let cert = &self.identity.certificate;
        self.revocations
            .check(cert.person_id(), cert.signing_key().as_bytes())?;
        tracing::info!(
            person = %fingerprint(cert.person_id()),
            device = %fingerprint(cert.signing_key().as_bytes()),
            "wrapping key for device"
        );
        Ok(encode(&context.wrap(key, cert, self.identity)?))
    }

    /// Iterate current verified member-device certificates for fingerprint display.
    pub fn devices(&self) -> impl Iterator<Item = &DeviceCertificate> {
        self.devices
            .values()
            .filter(|cert| self.revocations.allows(cert))
    }

    pub(super) fn device(&self, id: &str) -> Option<&DeviceCertificate> {
        self.devices
            .get(id)
            .filter(|cert| self.revocations.allows(cert))
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
        self.dms.insert(channel, DmPin::With { peer, anchor: None });
        let me = *self.identity.certificate.person_id();
        // Keys taken before this channel was known as a DM must pass the checks too.
        self.keys.retain(|(scope, _), held| {
            scope.kind != Kind::Channel
                || scope.id != channel
                || dm_member(&me, &peer, &held.sender)
        });
        let server_id = self.server.known_server().server_id;
        for ((scope, epoch), held) in &mut self.keys {
            if scope.kind == Kind::Channel && scope.id == channel {
                held.pair_bound = EnvelopeContext::new_dm(
                    server_id,
                    *scope,
                    *epoch,
                    &held.key,
                    keys::dm_pair(me, peer),
                )
                .excluding(&held.excluded)
                .commitment
                    == held.commitment;
            }
        }
        true
    }

    /// Whether this server's DM pins couldn't be read or saved, so its DMs are paused.
    pub fn dms_paused(&self) -> bool {
        self.pins_lost
    }

    /// The pins couldn't be saved: pause this server's DMs, as if they were lost.
    pub fn pause_dms(&mut self) {
        self.pins_lost = true;
    }

    /// DM channels pinned so far.
    pub fn dms(&self) -> &HashMap<[u8; 32], DmPin> {
        &self.dms
    }

    /// Whether this DM channel can take a message only the two people can read:
    /// pinned to `peer`, and its current epoch chains to a key this device knows is
    /// new (see `trusted_epoch`), wrapped to it by one of the two.
    pub fn dm_private(&self, channel: &[u8; 32], peer: &PersonId) -> bool {
        let scope = Scope {
            kind: Kind::Channel,
            id: *channel,
        };
        let me = self.identity.certificate.person_id();
        !self.pins_lost
            && matches!(self.dms.get(channel), Some(DmPin::With { peer: bound, .. }) if bound == peer)
            && self.current_key(scope).is_ok_and(|(epoch, _)| {
                self.trusted_epoch(scope) == Some(epoch)
                    && (self.keys.get(&(scope, epoch)))
                        .is_some_and(|held| dm_member(me, peer, &held.sender))
            })
    }

    /// Historical plaintext may leave this server only if its own epoch key was
    /// committed to the pinned pair. A later DM pin cannot relabel ordinary chat.
    pub(super) fn dm_history_epoch(&self, channel: [u8; 32], epoch: i64) -> Option<bool> {
        let scope = Scope {
            kind: Kind::Channel,
            id: channel,
        };
        self.keys.get(&(scope, epoch)).map(|held| held.pair_bound)
    }

    /// The newest DM epoch this device can tell only the two people hold: its anchor
    /// (an epoch it started itself after pinning, or reached before), then each epoch
    /// a key link from one of the two names, sent in the epoch trusted so far.
    fn trusted_epoch(&self, scope: Scope) -> Option<i64> {
        let Some(DmPin::With {
            anchor: Some(anchor),
            ..
        }) = self.dms.get(&scope.id)
        else {
            return None;
        };
        // Only keys made for this pair count; a plain DM epoch from before is not one.
        let commitment = |epoch| {
            (self.keys.get(&(scope, epoch)))
                .filter(|held| held.pair_bound)
                .map(|held| held.commitment)
        };
        let links = self.links.get(&scope.id).map_or(&[][..], Vec::as_slice);
        chain(*anchor, commitment, links)
    }

    /// Move a DM's anchor up to its newest trusted epoch, so it is saved.
    fn advance_anchor(&mut self, scope: Scope) {
        let Some(epoch) = self.trusted_epoch(scope) else {
            return;
        };
        let commitment = self.keys.get(&(scope, epoch)).map(|held| held.commitment);
        if let (Some(commitment), Some(DmPin::With { anchor, .. })) =
            (commitment, self.dms.get_mut(&scope.id))
        {
            *anchor = Some((epoch, commitment));
        }
    }

    /// Note a key link opened in a pinned DM, if one of its two people sent it.
    pub(super) fn note_link(
        &mut self,
        channel: &[u8; 32],
        epoch: i64,
        person: &PersonId,
        link: KeyLink,
    ) {
        let me = self.identity.certificate.person_id();
        if let Some(DmPin::With { peer, .. }) = self.dms.get(channel)
            && dm_member(me, peer, person)
        {
            let links = self.links.entry(*channel).or_default();
            if !links.contains(&(epoch, link)) {
                links.push((epoch, link));
            }
            self.advance_anchor(Scope {
                kind: Kind::Channel,
                id: *channel,
            });
        }
    }

    /// Whether this person may take a live file offered in this channel: in a DM,
    /// only the pinned other person (not even this person's own other devices).
    #[cfg(feature = "voice")]
    pub(super) fn dm_peer_is(&self, channel: &[u8; 32], person: &PersonId) -> bool {
        dm_peer_is(&self.dms, self.pins_lost, channel, person)
    }

    /// Whether this device may accept a live file `offerer` offered in this channel.
    #[cfg(feature = "voice")]
    pub(super) fn may_accept_offer(&self, channel: &[u8; 32], offerer: &PersonId) -> bool {
        let me = self.identity.certificate.person_id();
        may_accept_offer(&self.dms, self.pins_lost, me, channel, offerer)
    }

    /// Whether a key of this scope may come from or go to this person: anyone for
    /// a channel or space, only the two people for a pinned DM. Live files in a DM
    /// follow the same rule.
    pub(super) fn dm_allows(&self, scope: Scope, person: &PersonId) -> bool {
        dm_allows(
            &self.dms,
            self.identity.certificate.person_id(),
            scope,
            person,
        )
    }

    /// For each pinned DM whose current epoch doesn't chain to a trusted one, look
    /// for its key link in history, then re-key: announce a fresh key in a link in
    /// the current epoch and start the next epoch with it, wrapped only to the pair.
    /// An earlier key may have reached others while the channel wasn't known as a
    /// DM. Returns whether key state changed.
    async fn rekey_dms(&mut self) -> Result<bool> {
        let mut changed = false;
        self.rekey_failures = 0;
        let readable: Vec<_> = (self.scopes.iter())
            .filter(|state| state.scope.kind == Kind::Channel && !state.rotate && state.epoch > 0)
            .map(|state| (state.scope, state.epoch))
            .collect();
        // Forget attempts for epochs that moved on, so no deadline stays in the past.
        self.rekeyed.retain(|(channel, epoch), _| {
            readable.contains(&(
                Scope {
                    kind: Kind::Channel,
                    id: *channel,
                },
                *epoch,
            ))
        });
        for (scope, epoch) in readable {
            if !matches!(self.dms.get(&scope.id), Some(DmPin::With { .. })) {
                continue;
            }
            self.advance_anchor(scope);
            let slot = (scope.id, epoch);
            if self.trusted_epoch(scope) == Some(epoch) {
                self.rekeyed.remove(&slot);
                continue;
            }
            let now = std::time::Instant::now();
            if self.rekeyed.get(&slot).is_some_and(|(_, next)| now < *next) {
                continue;
            }
            // Try again later, waiting longer each time, until this epoch is trusted.
            let tries = self.rekeyed.get(&slot).map_or(0, |(tries, _)| *tries) + 1;
            self.rekeyed.insert(slot, (tries, now + backoff(tries)));
            self.scan_links(scope, epoch).await;
            if self.trusted_epoch(scope) == Some(epoch) {
                self.rekeyed.remove(&slot);
                continue;
            }
            if !self.may_create_epoch(scope) {
                continue;
            }
            let current = self.key(scope, epoch).cloned();
            let linked = match recovery(current.is_some(), tries) {
                Recovery::Wait => continue,
                Recovery::Link => true,
                Recovery::Fresh => false,
            };
            let next = epoch.checked_add(1).context("epoch overflow")?;
            let key = keys::random_key()?;
            let context = self
                .epoch_context(scope, next, &key)
                .excluding(&self.rotation_proposal(scope).0);
            if linked && let Some(current) = current {
                let link = KeyLink {
                    epoch: next,
                    commitment: context.commitment,
                };
                let body = MessageBody {
                    key_link: Some(link),
                    ..MessageBody::default()
                };
                let id = *keys::random_key()?;
                if let Err(error) = self
                    .post_at(&scope.id, id, epoch, &current, &body, None)
                    .await
                {
                    tracing::warn!(error = %format_args!("{error:#}"), "announcing a DM key failed");
                    continue;
                }
            }
            // A server without DM re-keying refuses; this DM then stays unused there.
            if let Some(commitment) = self.publish_key(scope, next, &key).await?
                && let Some(DmPin::With { anchor, .. }) = self.dms.get_mut(&scope.id)
            {
                *anchor = Some((next, commitment));
                self.rekeyed.remove(&slot);
            }
            changed = true;
        }
        Ok(changed)
    }

    /// The earliest retry for an untrusted DM epoch or a lost rotation race,
    /// so the connection can sync then even without an event.
    pub fn next_rekey(&self) -> Option<std::time::Instant> {
        self.rekeyed
            .values()
            .map(|(_, next)| *next)
            .chain(self.rotation_retry)
            .min()
    }

    /// A timer-driven sync failed before it could move the due attempts on: push
    /// them back, waiting longer after each failure in a row, so it can't spin.
    pub fn rekey_failed(&mut self) {
        self.rekey_failures += 1;
        if let Some(next) = &mut self.rotation_retry {
            *next = std::time::Instant::now() + backoff(self.rekey_failures);
        }
        delay_due(
            &mut self.rekeyed,
            std::time::Instant::now(),
            self.rekey_failures,
        );
    }

    /// Something is waiting to go out: try untrusted DM epochs on the next sync.
    pub fn rekey_now(&mut self) {
        let now = std::time::Instant::now();
        for (_, next) in self.rekeyed.values_mut() {
            *next = (*next).min(now);
        }
    }

    /// Open the DM's recent history from before `epoch`, which notes its key links.
    // shortcut: four pages back; a link buried deeper means one more re-key.
    async fn scan_links(&mut self, scope: Scope, epoch: i64) {
        let mut before = None;
        for _ in 0..4 {
            let Ok(records) = self.message_page(&scope.id, before.as_ref(), 50).await else {
                return;
            };
            for record in records.iter().filter(|record| record.epoch < epoch) {
                let _ = self.open_message(&scope.id, record).await;
            }
            match records.last() {
                Some(last) if records.len() == 50 => before = Some(last.id),
                _ => return,
            }
        }
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
                if self.create_protected_epochs {
                    if self.former_devices.len() < super::revocations::MAX_PER_REPLY {
                        self.former_devices.insert((
                            *certificate.person_id(),
                            certificate.signing_key().to_bytes(),
                        ));
                    } else {
                        tracing::warn!("former device limit reached");
                    }
                }
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

    /// Leave this member out of others' member lists while they appear offline.
    pub async fn set_hide_offline(&self, hide_offline: bool) -> Result<()> {
        self.request(Method::PUT, "/v1/me/hide-offline")
            .json(&json!({"hide_offline": hide_offline}))
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

    /// Encrypt and replace this member's bio; an empty bio clears it.
    pub async fn set_bio(&self, bio: &str) -> Result<()> {
        let bio_ct = if bio.is_empty() {
            None
        } else {
            let me = self.identity.certificate.person_id();
            Some(self.encrypt_metadata(Metadata::Bio, me, bio.as_bytes())?)
        };
        self.request(Method::PUT, "/v1/me/profile")
            .json(&json!({"bio_ct": bio_ct}))
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
        self.keys.insert(
            (scope, 1),
            HeldKey {
                key,
                envelope,
                sender: me,
                commitment: context.commitment,
                excluded: Default::default(),
                pair_bound: false,
            },
        );
        self.scopes.push(ScopeState {
            scope,
            epoch: 1,
            rotate: false,
            excluded: Default::default(),
        });
        if !self.rotations.covered(self.space(), &Excluded::new()) {
            // Classify the new channel with its actual permissions before it can send.
            self.sync().await?;
        }
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

/// Follow key links from a trusted anchor: each step is a link sent in the epoch
/// trusted so far, naming a later epoch whose commitment matches. An anchor whose
/// commitment differs from the key held for it is not trusted at all.
fn chain(
    anchor: (i64, [u8; 32]),
    commitment: impl Fn(i64) -> Option<[u8; 32]>,
    links: &[(i64, KeyLink)],
) -> Option<i64> {
    if commitment(anchor.0) != Some(anchor.1) {
        return None;
    }
    let mut trusted = anchor.0;
    loop {
        let next = (links.iter())
            .filter(|(within, link)| {
                *within == trusted
                    && link.epoch > trusted
                    && commitment(link.epoch) == Some(link.commitment)
            })
            .map(|(_, link)| link.epoch)
            .max();
        match next {
            Some(epoch) => trusted = epoch,
            None => return Some(trusted),
        }
    }
}

fn dm_member(me: &PersonId, peer: &PersonId, person: &PersonId) -> bool {
    person == me || person == peer
}

/// What a device knows about a DM channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmPin {
    /// With this person. `anchor` is the newest epoch this device knows only the
    /// two people hold, with its commitment: one it started after pinning, or one
    /// reached from there by key links. `None` until then.
    With {
        peer: PersonId,
        anchor: Option<(i64, [u8; 32])>,
    },
    /// Its person is unknown because the saved pins were lost: its keys go to nobody
    /// else and nothing is sent there.
    Blocked,
}

/// Whether this device may wrap a key of this scope for this person's device: a
/// pinned DM's key only for one of the two, and only if it was made for them.
fn may_wrap(
    dms: &HashMap<[u8; 32], DmPin>,
    me: &PersonId,
    scope: Scope,
    person: &PersonId,
    pair_bound: bool,
) -> bool {
    let dm = scope.kind == Kind::Channel && dms.contains_key(&scope.id);
    dm_allows(dms, me, scope, person) && (!dm || pair_bound)
}

/// How to recover a DM epoch that doesn't chain to a trusted one.
#[derive(Debug, PartialEq)]
enum Recovery {
    /// Announce the next key in a link sealed with the current one, then start it.
    Link,
    /// Start the next epoch without a link. Only this device trusts it at first,
    /// having made it after pinning; the other person can't, and re-keys from it
    /// with a link this device then trusts.
    Fresh,
    /// The current key may still arrive.
    Wait,
}

/// With the current key, link from it. Without it (say, two new devices that never
/// held the old key), give it one wait for a late envelope, then start fresh.
fn recovery(have_current: bool, tries: u32) -> Recovery {
    match (have_current, tries) {
        (true, _) => Recovery::Link,
        (false, 1) => Recovery::Wait,
        (false, _) => Recovery::Fresh,
    }
}

/// Move every attempt due before the failure backoff to the end of it.
fn delay_due(
    rekeyed: &mut HashMap<([u8; 32], i64), (u32, std::time::Instant)>,
    now: std::time::Instant,
    failures: u32,
) {
    let until = now + backoff(failures);
    for (_, next) in rekeyed.values_mut() {
        *next = (*next).max(until);
    }
}

/// The wait before the next recovery attempt: 15 s, doubling up to 16 min.
fn backoff(tries: u32) -> Duration {
    Duration::from_secs(15 << tries.saturating_sub(1).min(6))
}

/// Whether this person may take a live file in this channel: in a DM, only the
/// pinned other person, and nobody while this server's DMs are paused.
/// Whether this device may accept a live file offered here by `offerer`: in a DM,
/// only from one of the two people, and not while this server's DMs are paused.
#[cfg(any(feature = "voice", test))]
fn may_accept_offer(
    dms: &HashMap<[u8; 32], DmPin>,
    paused: bool,
    me: &PersonId,
    channel: &[u8; 32],
    offerer: &PersonId,
) -> bool {
    let scope = Scope {
        kind: Kind::Channel,
        id: *channel,
    };
    dm_allows(dms, me, scope, offerer) && !(paused && dms.contains_key(channel))
}

#[cfg(any(feature = "voice", test))]
fn dm_peer_is(
    dms: &HashMap<[u8; 32], DmPin>,
    paused: bool,
    channel: &[u8; 32],
    person: &PersonId,
) -> bool {
    match dms.get(channel) {
        Some(DmPin::With { peer, .. }) => !paused && peer == person,
        Some(DmPin::Blocked) => false,
        None => true,
    }
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
    use crate::identity::DeviceIdentity;

    #[tokio::test]
    async fn protocol_two_keeps_the_current_key_after_learning_a_removal() -> Result<()> {
        let profile = tempfile::tempdir()?;
        let server = Server::for_test(2, profile.path());
        let (words, identity) = DeviceIdentity::create("reader")?;
        let sibling = DeviceIdentity::add_device(&words, "removed")?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let socket = tokio::net::TcpStream::connect(listener.local_addr()?).await?;
        let events = EventStream::from_raw_socket(
            tokio_tungstenite::MaybeTlsStream::Plain(socket),
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        let session = Session {
            token: String::new(),
            expires_at: 0,
        };
        let mut client = KeyClient::from_stream(&server, &identity, &session, None, events)?;
        let scope = client.space();
        let key = keys::random_key()?;
        let epoch = 7;
        let context = EnvelopeContext::new(server.known_server().server_id, scope, epoch, &key);
        client.keys.insert(
            (scope, epoch),
            HeldKey {
                key: key.clone(),
                envelope: encode(&context.wrap(&key, &identity.certificate, &identity)?),
                sender: *identity.certificate.person_id(),
                commitment: context.commitment,
                excluded: Excluded::new(),
                pair_bound: false,
            },
        );
        client.scopes.push(ScopeState {
            scope,
            epoch,
            rotate: false,
            excluded: Excluded::new(),
        });
        client.checked_removals = true;
        assert_eq!(client.current_key(scope)?, (epoch, &key));

        let owner = identity.certificate.person_id();
        let removal = crate::identity::revoke_device(
            &words,
            owner,
            sibling.certificate.signing_key().to_bytes(),
        )?;
        assert!(
            client
                .revocations
                .observe(owner, &encode(&removal.to_bytes()))?
        );
        assert!(client.changes.has_changed()?);
        assert_eq!(client.current_key(scope)?, (epoch, &key));
        Ok(())
    }

    #[test]
    fn recovery_waits_once_for_a_missing_key_then_starts_fresh() {
        assert_eq!(backoff(1), Duration::from_secs(15));
        assert_eq!(backoff(2), Duration::from_secs(30));
        assert_eq!(backoff(99), Duration::from_secs(960));
        assert_eq!(recovery(true, 1), Recovery::Link);
        assert_eq!(recovery(true, 5), Recovery::Link);
        // Two new devices that never got the old key: wait one round, then re-key
        // without a link rather than never.
        assert_eq!(recovery(false, 1), Recovery::Wait);
        assert_eq!(recovery(false, 2), Recovery::Fresh);
    }

    #[test]
    fn failed_timer_syncs_wait_longer_each_time() {
        let now = std::time::Instant::now();
        let slot = ([9; 32], 3);
        let mut rekeyed = HashMap::from([(slot, (1, now))]);
        // A deadline already passed and the sync fails at once, twice in a row.
        delay_due(&mut rekeyed, now, 1);
        assert_eq!(rekeyed[&slot].1, now + Duration::from_secs(15));
        delay_due(&mut rekeyed, now, 2);
        assert_eq!(rekeyed[&slot].1, now + Duration::from_secs(30));
        // A later deadline stays as it is.
        let later = now + Duration::from_secs(600);
        rekeyed.insert(slot, (4, later));
        delay_due(&mut rekeyed, now, 1);
        assert_eq!(rekeyed[&slot].1, later);
    }

    #[test]
    fn a_paused_device_accepts_no_dm_offer() {
        let (me, peer, other) = ([1; 32], [2; 32], [3; 32]);
        let dms = HashMap::from([([9; 32], DmPin::With { peer, anchor: None })]);
        assert!(may_accept_offer(&dms, false, &me, &[9; 32], &peer));
        assert!(!may_accept_offer(&dms, false, &me, &[9; 32], &other));
        assert!(!may_accept_offer(&dms, true, &me, &[9; 32], &peer));
        // An ordinary channel's offers are unaffected.
        assert!(may_accept_offer(&dms, true, &me, &[8; 32], &other));
    }

    #[test]
    fn a_dm_live_file_goes_only_to_the_pinned_peer_and_not_while_paused() {
        let (me, peer, other) = ([1; 32], [2; 32], [3; 32]);
        let dms = HashMap::from([
            ([9; 32], DmPin::With { peer, anchor: None }),
            ([7; 32], DmPin::Blocked),
        ]);
        assert!(dm_peer_is(&dms, false, &[9; 32], &peer));
        assert!(!dm_peer_is(&dms, false, &[9; 32], &me));
        assert!(!dm_peer_is(&dms, false, &[9; 32], &other));
        // Pins couldn't be saved: outstanding offers stop too.
        assert!(!dm_peer_is(&dms, true, &[9; 32], &peer));
        assert!(!dm_peer_is(&dms, false, &[7; 32], &peer));
        // An ordinary channel is anyone's.
        assert!(dm_peer_is(&dms, true, &[8; 32], &other));
    }

    #[test]
    fn two_devices_with_conflicting_pins_dont_leak_the_key() {
        // Alice's two devices; the server told device 2 the DM is with Carol.
        let (words, first) = DeviceIdentity::create("first").unwrap();
        let second = DeviceIdentity::add_device(&words, "second").unwrap();
        let alice = *first.certificate.person_id();
        let (bob, carol) = ([2; 32], [3; 32]);
        let scope = Scope {
            kind: Kind::Channel,
            id: [9; 32],
        };
        // Device 1 starts an epoch for Alice and Bob and wraps it to device 2.
        let key = keys::random_key().unwrap();
        let made = EnvelopeContext::new_dm([1; 32], scope, 3, &key, keys::dm_pair(alice, bob));
        let envelope = made.wrap(&key, &second.certificate, &first).unwrap();
        let opened = made.open(&second, &first.certificate, &envelope).unwrap();
        // Device 2 can tell it isn't a key for Alice and Carol, nor a plain one, so
        // it doesn't keep it.
        assert!(!made.commits_dm(&opened, keys::dm_pair(alice, carol)));
        assert!(!made.commits(&opened));
        assert!(made.commits_dm(&opened, keys::dm_pair(bob, alice)));
        // Nor does it wrap a DM key that wasn't made for its pinned pair, even to
        // the person its pin names, or a pair key to anyone else.
        let dms = HashMap::from([(
            scope.id,
            DmPin::With {
                peer: carol,
                anchor: None,
            },
        )]);
        assert!(!may_wrap(&dms, &alice, scope, &carol, false));
        assert!(may_wrap(&dms, &alice, scope, &carol, true));
        assert!(!may_wrap(&dms, &alice, scope, &bob, true));
        // Ordinary channels are unchanged.
        let channel = Scope {
            id: [8; 32],
            ..scope
        };
        assert!(may_wrap(&dms, &alice, channel, &bob, false));
    }

    #[test]
    fn only_epochs_linked_from_a_new_key_are_trusted() {
        let link = |epoch, commitment| KeyLink { epoch, commitment };
        // Epoch 3 is the key this device started after pinning; 4 and 5 were shared
        // before the pin by someone else, 6 was announced in 3 by the other person.
        let held = |epoch: i64| (3..=6).contains(&epoch).then_some([epoch as u8; 32]);
        assert_eq!(chain((3, [3; 32]), held, &[]), Some(3));
        let links = [
            // A link sent in an epoch that isn't trusted proves nothing.
            (4, link(5, [5; 32])),
            // Nor does one whose commitment isn't that epoch's.
            (3, link(5, [9; 32])),
            (3, link(6, [6; 32])),
        ];
        assert_eq!(chain((3, [3; 32]), held, &links), Some(6));
        // A higher epoch number alone is not enough, and a changed anchor key is nothing.
        assert_eq!(chain((3, [3; 32]), held, &links[..2]), Some(3));
        assert_eq!(chain((3, [7; 32]), held, &links), None);
    }

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
            (dm.id, DmPin::With { peer, anchor: None }),
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
