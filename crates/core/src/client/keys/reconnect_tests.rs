//! Reconnects against a plain-HTTP stand-in for the API, through the real key sync.

use std::{
    cell::Cell,
    sync::{Arc, Mutex},
};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::keys::{KeyNeed, WrappedKey, exclusion_id};

thread_local! {
    /// Envelopes the key sync opened (decrypted and checked) on this thread.
    pub(super) static OPENS: Cell<u32> = const { Cell::new(0) };
    /// Device certificates the key sync checked on this thread.
    pub(super) static VERIFIES: Cell<u32> = const { Cell::new(0) };
}

/// Envelopes opened since the last call.
fn opens() -> u32 {
    OPENS.with(Cell::take)
}

/// Certificates checked since the last call.
fn verifies() -> u32 {
    VERIFIES.with(Cell::take)
}

/// Replies for `"METHOD /path"`: each request takes the next one and the last one
/// repeats. Status 0 never answers; a route not listed gets a 500.
type Routes = Arc<Mutex<HashMap<String, VecDeque<(u16, String)>>>>;

struct Api {
    routes: Routes,
    log: Arc<Mutex<Vec<String>>>,
    url: url::Url,
}

impl Api {
    /// Every test starts here, so the counters start from zero even when an earlier
    /// test on this thread left some behind.
    async fn start() -> Result<Self> {
        opens();
        verifies();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/", listener.local_addr()?).parse()?;
        let (routes, log) = (Routes::default(), Arc::<Mutex<Vec<String>>>::default());
        let (served, logged) = (routes.clone(), log.clone());
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(answer(socket, served.clone(), logged.clone()));
            }
        });
        Ok(Self { routes, log, url })
    }

    fn set(&self, route: &str, replies: Vec<(u16, String)>) {
        let mut routes = self.routes.lock().unwrap();
        routes.insert(route.into(), replies.into());
    }

    fn keys(&self, states: &[KeyState]) {
        let replies = states
            .iter()
            .map(|state| (200, serde_json::to_string(state).unwrap()))
            .collect();
        self.set("GET /v1/keys", replies);
    }

    fn devices(&self, devices: &[&DeviceIdentity]) {
        let listed: Vec<_> = (devices.iter())
            .map(|device| {
                let cert = &device.certificate;
                (*cert.person_id(), cert.public_bytes().unwrap().to_vec())
            })
            .collect();
        self.certificates(&listed);
    }

    /// List these certificate bytes, each for the person given.
    fn certificates(&self, listed: &[(PersonId, Vec<u8>)]) {
        let listed: Vec<_> = (listed.iter())
            .map(|(person, bytes)| json!({"person": encode(person), "certificate": encode(bytes)}))
            .collect();
        self.set(
            "GET /v1/member-devices",
            vec![(200, json!(listed).to_string())],
        );
    }

    /// The catalog lists `channel` as a DM with `peer`, or as a text channel.
    fn catalog(&self, channel: Scope, peer: Option<&PersonId>) {
        let catalog = json!({"channels": [{"id": encode(&channel.id),
            "kind": if peer.is_some() { "dm" } else { "text" }, "category_id": null,
            "position": 0, "name_ct": null, "topic_ct": null,
            "peer": peer.map(|peer| encode(peer.as_slice())), "flags": []}], "categories": []});
        self.set("GET /v1/catalog", vec![(200, catalog.to_string())]);
    }

    fn asked(&self, route: &str) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|r| *r == route)
            .count()
    }

    fn server(&self, profile: &std::path::Path, server_id: [u8; 32]) -> Server {
        Server::for_test(2, profile).at(self.url.clone(), server_id)
    }
}

async fn answer(
    mut socket: tokio::net::TcpStream,
    routes: Routes,
    log: Arc<Mutex<Vec<String>>>,
) -> std::io::Result<()> {
    let mut request = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        request.extend_from_slice(&chunk[..read]);
        let text = String::from_utf8_lossy(&request);
        if let Some(end) = text.find("\r\n\r\n") {
            let length = (text[..end].lines())
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            if request.len() >= end + 4 + length {
                break;
            }
        }
    }
    let text = String::from_utf8_lossy(&request);
    let mut line = text.lines().next().unwrap_or_default().split(' ');
    let route = format!(
        "{} {}",
        line.next().unwrap_or_default(),
        line.next().unwrap_or_default()
    );
    log.lock().unwrap().push(route.clone());
    let (status, body) = {
        let mut routes = routes.lock().unwrap();
        match routes.get_mut(&route) {
            Some(replies) if replies.len() > 1 => replies.pop_front().unwrap(),
            Some(replies) => replies.front().cloned().unwrap(),
            None => (500, "{}".into()),
        }
    };
    if status == 0 {
        std::future::pending::<()>().await;
    }
    let reply = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(reply.as_bytes()).await
}

/// An event socket nobody reads from; key sync doesn't need one.
async fn events() -> Result<EventStream> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let socket = tokio::net::TcpStream::connect(listener.local_addr()?).await?;
    Ok(EventStream::from_raw_socket(
        tokio_tungstenite::MaybeTlsStream::Plain(crate::route::RouteStream::Direct(socket)),
        tokio_tungstenite::tungstenite::protocol::Role::Client,
        None,
    )
    .await)
}

fn session() -> Session {
    Session {
        token: String::new(),
        expires_at: 0,
    }
}

async fn connect<'a>(
    server: &'a Server,
    identity: &'a DeviceIdentity,
    dms: Option<HashMap<[u8; 32], DmPin>>,
    earlier: &Verified,
) -> Result<KeyClient<'a>> {
    KeyClient::start(server, identity, &session(), dms, events().await?, earlier).await
}

/// `key` for `epoch` of `scope`, wrapped to `recipient` by `sender`, under `commitment`.
fn wrap(
    server_id: [u8; 32],
    (scope, epoch): (Scope, i64),
    key: &Key,
    commitment: [u8; 32],
    recipient: &DeviceIdentity,
    sender: &DeviceIdentity,
) -> Result<WrappedKey> {
    let context = EnvelopeContext {
        server_id,
        scope,
        epoch,
        commitment,
    };
    Ok(WrappedKey {
        need: KeyNeed {
            scope,
            epoch,
            device: encode(recipient.certificate.signing_key().as_bytes()),
        },
        sender: encode(sender.certificate.signing_key().as_bytes()),
        commitment,
        envelope: encode(&context.wrap(key, &recipient.certificate, sender)?),
        excluded: Excluded::new(),
    })
}

fn plain(server_id: [u8; 32], slot: (Scope, i64), key: &Key) -> [u8; 32] {
    EnvelopeContext::new(server_id, slot.0, slot.1, key).commitment
}

fn state(scopes: &[(Scope, i64)], envelopes: Vec<WrappedKey>) -> KeyState {
    KeyState {
        supports_exclusions: false,
        revocations: Vec::new(),
        scopes: (scopes.iter())
            .map(|&(scope, epoch)| ScopeState {
                scope,
                epoch,
                rotate: false,
                excluded: Excluded::new(),
            })
            .collect(),
        envelopes,
        pending: Vec::new(),
    }
}

const SERVER: [u8; 32] = [1; 32];

fn space() -> Scope {
    Scope {
        kind: Kind::Space,
        id: SERVER,
    }
}

fn channel(id: u8) -> Scope {
    Scope {
        kind: Kind::Channel,
        id: [id; 32],
    }
}

#[tokio::test]
async fn a_reconnect_reuses_listed_keys_and_drops_the_rest() -> Result<()> {
    let profile = tempfile::tempdir()?;
    let api = Api::start().await?;
    let server = api.server(profile.path(), SERVER);
    let (words, me) = DeviceIdentity::create("reader")?;
    let sibling = DeviceIdentity::add_device(&words, "sibling")?;
    let held: Vec<_> = (0..2)
        .map(|_| keys::random_key())
        .collect::<Result<_, _>>()?;
    let listed: Vec<_> = (1..=2)
        .map(|epoch| {
            let slot = (space(), epoch);
            let key = &held[epoch as usize - 1];
            wrap(SERVER, slot, key, plain(SERVER, slot, key), &me, &sibling)
        })
        .collect::<Result<_>>()?;
    api.devices(&[&me, &sibling]);
    api.keys(&[state(&[(space(), 2)], listed.clone())]);
    let mut live = connect(&server, &me, Some(HashMap::new()), &Verified::default()).await?;
    assert_eq!(opens(), 2);

    // The server lists only epoch 1 now.
    api.keys(&[state(&[(space(), 2)], listed[..1].to_vec())]);
    live.resume(&session(), events().await?).await?;
    assert_eq!(opens(), 0, "an unchanged envelope isn't opened again");
    assert!(live.key(space(), 1) == Some(&held[0]));
    assert!(
        live.key(space(), 2).is_none(),
        "a key not listed again is dropped"
    );
    Ok(())
}

#[tokio::test]
async fn a_reconnect_checks_changed_envelopes_again() -> Result<()> {
    let profile = tempfile::tempdir()?;
    let api = Api::start().await?;
    let server = api.server(profile.path(), SERVER);
    let (words, me) = DeviceIdentity::create("reader")?;
    let sibling = DeviceIdentity::add_device(&words, "sibling")?;
    let key = keys::random_key()?;
    let slot = |epoch| (space(), epoch);
    let first: Vec<_> = (1..=4)
        .map(|epoch| {
            wrap(
                SERVER,
                slot(epoch),
                &key,
                plain(SERVER, slot(epoch), &key),
                &me,
                &sibling,
            )
        })
        .collect::<Result<_>>()?;
    api.devices(&[&me, &sibling]);
    api.set("POST /v1/keys/reject", vec![(200, "{}".into())]);
    let scopes = [(space(), 4)];
    api.keys(&[state(&scopes, first.clone())]);
    let mut live = connect(&server, &me, Some(HashMap::new()), &Verified::default()).await?;
    assert_eq!(opens(), 4);

    // Epoch 1 wrapped again, epoch 2 under changed exclusions, epoch 3 claimed by
    // another device; epoch 4 unchanged. The server drops what this device rejects.
    let mut changed = first.clone();
    changed[0] = wrap(SERVER, slot(1), &key, first[0].commitment, &me, &sibling)?;
    assert_ne!(changed[0].envelope, first[0].envelope);
    let other = exclusion_id(me.certificate.person_id(), &[7; 32]);
    changed[1].excluded = Excluded::from([other]);
    changed[2].sender = encode(me.certificate.signing_key().as_bytes());
    let kept = vec![changed[0].clone(), changed[3].clone()];
    api.keys(&[state(&scopes, changed), state(&scopes, kept)]);
    live.resume(&session(), events().await?).await?;
    // Epochs 1 and 3 were opened again; epoch 2 failed its commitment as when fresh.
    assert_eq!(opens(), 2);
    assert_eq!(api.asked("POST /v1/keys/reject"), 2);
    assert!(live.key(space(), 1) == Some(&key));
    assert!(live.key(space(), 2).is_none() && live.key(space(), 3).is_none());
    assert!(live.key(space(), 4) == Some(&key));
    Ok(())
}

#[tokio::test]
async fn a_reconnect_drops_keys_from_removed_senders_and_outside_a_dm() -> Result<()> {
    let profile = tempfile::tempdir()?;
    let api = Api::start().await?;
    let server = api.server(profile.path(), SERVER);
    let (words, me) = DeviceIdentity::create("reader")?;
    let revoked = DeviceIdentity::add_device(&words, "revoked")?;
    let (_, unlisted) = DeviceIdentity::create("left")?;
    let (_, peer) = DeviceIdentity::create("peer")?;
    let (_, stranger) = DeviceIdentity::create("stranger")?;
    let key = keys::random_key()?;
    let listed = [
        ((space(), 1), &revoked),
        ((space(), 2), &unlisted),
        ((channel(5), 1), &peer),
    ]
    .into_iter()
    .map(|(slot, sender)| wrap(SERVER, slot, &key, plain(SERVER, slot, &key), &me, sender))
    .collect::<Result<Vec<_>>>()?;
    let scopes = [(space(), 2), (channel(5), 1)];
    api.devices(&[&me, &revoked, &unlisted, &peer]);
    api.keys(&[state(&scopes, listed)]);
    let mut live = connect(&server, &me, Some(HashMap::new()), &Verified::default()).await?;
    assert_eq!(opens(), 3);

    // One sender is revoked, one no longer listed, and channel 5 turns out to be a DM
    // with someone else. The server still lists every envelope.
    let person = *me.certificate.person_id();
    let removal = crate::identity::revoke_device(
        &words,
        &person,
        revoked.certificate.signing_key().to_bytes(),
    )?;
    assert!(
        live.revocations
            .observe(&person, &encode(&removal.to_bytes()))?
    );
    api.devices(&[&me, &revoked, &peer]);
    let pin = DmPin::With {
        peer: *stranger.certificate.person_id(),
        anchor: None,
    };
    live.dms.insert(channel(5).id, pin);
    live.resume(&session(), events().await?).await?;
    assert!(live.key(space(), 1).is_none(), "revoked sender");
    assert!(live.key(space(), 2).is_none(), "sender no longer listed");
    assert!(live.key(channel(5), 1).is_none(), "sender outside the DM");
    assert_eq!(opens(), 0);
    Ok(())
}

#[tokio::test]
async fn another_server_or_device_does_not_reuse_keys() -> Result<()> {
    let profile = tempfile::tempdir()?;
    let api = Api::start().await?;
    let server = api.server(profile.path(), SERVER);
    let (words, me) = DeviceIdentity::create("reader")?;
    let sibling = DeviceIdentity::add_device(&words, "sibling")?;
    let other = DeviceIdentity::add_device(&words, "other")?;
    let key = keys::random_key()?;
    let slot = (space(), 1);
    let listed = wrap(SERVER, slot, &key, plain(SERVER, slot, &key), &me, &sibling)?;
    api.devices(&[&me, &sibling, &other]);
    api.set("POST /v1/keys/reject", vec![(200, "{}".into())]);
    api.keys(&[state(&[slot], vec![listed.clone()])]);
    let live = connect(&server, &me, Some(HashMap::new()), &Verified::default()).await?;
    assert_eq!(opens(), 1);
    let earlier = live.verified();

    // The same listing on a server with another ID, and for another device: each is
    // opened afresh, fails, and is rejected.
    let elsewhere = api.server(profile.path(), [2; 32]);
    for (server, identity) in [(&elsewhere, &me), (&server, &other)] {
        api.keys(&[state(&[slot], vec![listed.clone()]), state(&[slot], vec![])]);
        let client = connect(server, identity, Some(HashMap::new()), &earlier).await?;
        assert_eq!(opens(), 1);
        assert!(client.key(space(), 1).is_none());
    }
    assert_eq!(api.asked("POST /v1/keys/reject"), 2);
    Ok(())
}

#[tokio::test]
async fn a_failed_or_dropped_reconnect_leaves_the_client_as_it_was() -> Result<()> {
    let profile = tempfile::tempdir()?;
    let api = Api::start().await?;
    let server = api.server(profile.path(), SERVER);
    let (words, me) = DeviceIdentity::create("reader")?;
    let sibling = DeviceIdentity::add_device(&words, "sibling")?;
    let (_, peer) = DeviceIdentity::create("peer")?;
    let key = keys::random_key()?;
    let slot = (space(), 1);
    let listed = wrap(SERVER, slot, &key, plain(SERVER, slot, &key), &me, &sibling)?;
    api.devices(&[&me, &sibling, &peer]);
    api.keys(&[state(&[slot], vec![listed.clone()])]);
    let mut live = connect(&server, &me, Some(HashMap::new()), &Verified::default()).await?;

    // The next sync learns a DM key made for this pair, then fails (the server
    // refuses a rejection) before it completes.
    let dm = channel(9);
    let dm_key = keys::random_key()?;
    let pair = keys::dm_pair(*me.certificate.person_id(), *peer.certificate.person_id());
    let commitment = EnvelopeContext::new_dm(SERVER, dm, 1, &dm_key, pair).commitment;
    let mut broken = wrap(SERVER, (dm, 2), &dm_key, commitment, &me, &peer)?;
    broken.envelope = encode(&[0; 8]);
    let envelopes = vec![
        listed,
        wrap(SERVER, (dm, 1), &dm_key, commitment, &me, &peer)?,
        broken,
    ];
    api.keys(&[state(&[slot, (dm, 2)], envelopes)]);
    api.catalog(dm, Some(peer.certificate.person_id()));
    api.set("POST /v1/keys/reject", vec![(500, "{}".into())]);
    assert_eq!(opens(), 1);
    assert!(live.resume(&session(), events().await?).await.is_err());
    assert_eq!(opens(), 2, "the DM key and the broken one");
    assert_eq!(
        api.asked("GET /v1/catalog"),
        1,
        "the failed sync learned the DM"
    );
    assert!(live.key(dm, 1).is_none());
    assert!(live.dms().is_empty());
    assert!(live.key(space(), 1) == Some(&key));

    // A reconnect dropped while waiting on the server changes nothing either.
    api.set("GET /v1/keys", vec![(0, String::new())]);
    let fresh = session();
    let resumed = live.resume(&fresh, events().await?);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), resumed)
            .await
            .is_err()
    );
    assert!(live.key(space(), 1) == Some(&key));
    assert_eq!(live.verified().keys.len(), 1);
    assert_eq!(live.verified().certificates.len(), 3);
    assert_eq!(opens(), 0);
    Ok(())
}

#[tokio::test]
async fn a_reconnect_reuses_certificate_checks_only_for_the_same_bytes() -> Result<()> {
    let profile = tempfile::tempdir()?;
    let api = Api::start().await?;
    let server = api.server(profile.path(), SERVER);
    let (words, me) = DeviceIdentity::create("reader")?;
    let sibling = DeviceIdentity::add_device(&words, "sibling")?;
    let (_, peer) = DeviceIdentity::create("peer")?;
    let key = keys::random_key()?;
    let slot = (space(), 1);
    let listed = wrap(SERVER, slot, &key, plain(SERVER, slot, &key), &me, &sibling)?;
    api.devices(&[&me, &sibling, &peer]);
    api.keys(&[state(&[slot], vec![listed])]);
    let mut live = connect(&server, &me, Some(HashMap::new()), &Verified::default()).await?;
    assert_eq!(verifies(), 3);
    live.resume(&session(), events().await?).await?;
    assert_eq!(verifies(), 0, "unchanged certificates aren't checked again");

    // Changed bytes for a cached device, or its bytes claimed for another person,
    // are checked, fail, and fail the reconnect.
    let cert = |device: &DeviceIdentity| -> Result<_> {
        Ok((
            *device.certificate.person_id(),
            device.certificate.public_bytes()?.to_vec(),
        ))
    };
    let mut tampered = cert(&sibling)?;
    let created_at = tampered.1.len() - 64 - 32 - 1;
    tampered.1[created_at] ^= 1;
    let mut claimed = cert(&sibling)?;
    claimed.0 = *peer.certificate.person_id();
    for changed in [tampered, claimed] {
        api.certificates(&[cert(&me)?, changed, cert(&peer)?]);
        assert!(live.resume(&session(), events().await?).await.is_err());
        assert_eq!(verifies(), 1);
    }

    // A device no longer listed leaves the cache, so it is checked again later.
    api.devices(&[&me, &peer]);
    live.resume(&session(), events().await?).await?;
    assert_eq!(verifies(), 0);
    assert_eq!(live.verified().certificates.len(), 2);
    api.devices(&[&me, &sibling, &peer]);
    live.resume(&session(), events().await?).await?;
    assert_eq!(verifies(), 1);

    // A removed device stays out although its certificate check is cached.
    let person = *me.certificate.person_id();
    let removal = crate::identity::revoke_device(
        &words,
        &person,
        sibling.certificate.signing_key().to_bytes(),
    )?;
    assert!(
        live.revocations
            .observe(&person, &encode(&removal.to_bytes()))?
    );
    live.resume(&session(), events().await?).await?;
    assert_eq!(verifies(), 0);
    assert!(
        live.device(&encode(sibling.certificate.signing_key().as_bytes()))
            .is_none()
    );
    assert_eq!(live.devices().count(), 2);
    assert!(live.key(space(), 1).is_none());
    Ok(())
}

#[tokio::test]
async fn a_removal_in_the_reconnect_reply_stops_reusing_that_device_s_key() -> Result<()> {
    let profile = tempfile::tempdir()?;
    let api = Api::start().await?;
    let server = api.server(profile.path(), SERVER);
    let (_, me) = DeviceIdentity::create("reader")?;
    let (peer_words, peer) = DeviceIdentity::create("peer")?;
    let phone = DeviceIdentity::add_device(&peer_words, "peer phone")?;
    let key = keys::random_key()?;
    let slot = (channel(4), 1);
    let listed = wrap(SERVER, slot, &key, plain(SERVER, slot, &key), &me, &phone)?;
    api.devices(&[&me, &peer, &phone]);
    api.keys(&[state(&[slot], vec![listed.clone()])]);
    let mut live = connect(&server, &me, Some(HashMap::new()), &Verified::default()).await?;
    assert_eq!(opens(), 1);

    let person = *peer.certificate.person_id();
    let phone_key = phone.certificate.signing_key().to_bytes();
    let removal = crate::identity::revoke_device(&peer_words, &person, phone_key)?;
    let mut reply = state(&[slot], vec![listed]);
    reply.revocations = vec![(encode(&person), encode(&removal.to_bytes()))];
    api.keys(&[reply]);
    live.resume(&session(), events().await?).await?;
    assert_eq!(opens(), 0);
    assert!(live.key(slot.0, 1).is_none());
    assert!(live.device(&encode(&phone_key)).is_none());
    Ok(())
}

#[tokio::test]
async fn a_carried_dm_key_needs_the_same_pin() -> Result<()> {
    let profile = tempfile::tempdir()?;
    let api = Api::start().await?;
    let server = api.server(profile.path(), SERVER);
    let (_, me) = DeviceIdentity::create("reader")?;
    let (_, peer) = DeviceIdentity::create("peer")?;
    let (_, stranger) = DeviceIdentity::create("stranger")?;
    let dm = channel(6);
    let key = keys::random_key()?;
    let peer_id = *peer.certificate.person_id();
    let pair = keys::dm_pair(*me.certificate.person_id(), peer_id);
    let commitment = EnvelopeContext::new_dm(SERVER, dm, 1, &key, pair).commitment;
    let listed = wrap(SERVER, (dm, 1), &key, commitment, &me, &peer)?;
    api.devices(&[&me, &peer]);
    api.set("POST /v1/keys/reject", vec![(200, "{}".into())]);
    api.keys(&[state(&[(dm, 1)], vec![listed.clone()])]);
    let pins = HashMap::from([(
        dm.id,
        DmPin::With {
            peer: peer_id,
            anchor: None,
        },
    )]);
    let mut lost = connect(&server, &me, Some(pins.clone()), &Verified::default()).await?;
    let mut relabelled = connect(&server, &me, Some(pins.clone()), &Verified::default()).await?;
    let mut unpinned = connect(&server, &me, Some(pins), &Verified::default()).await?;
    for client in [&lost, &relabelled, &unpinned] {
        assert_eq!(client.dm_history_epoch(dm.id, 1), Some(true));
    }

    // The saved pins are lost: the DM is blocked, its key not taken.
    lost.pins_lost = true;
    api.catalog(dm, Some(&peer_id));
    lost.resume(&session(), events().await?).await?;
    assert!(lost.key(dm, 1).is_none());

    // Pinned to someone else: the key's sender is outside the DM.
    let pin = DmPin::With {
        peer: *stranger.certificate.person_id(),
        anchor: None,
    };
    relabelled.dms.insert(dm.id, pin);
    relabelled.resume(&session(), events().await?).await?;
    assert!(relabelled.key(dm, 1).is_none());

    // Not a DM any more: a key committed to the pair fails a plain channel's check.
    unpinned.dms.clear();
    api.catalog(dm, None);
    api.keys(&[state(&[(dm, 1)], vec![listed]), state(&[(dm, 1)], vec![])]);
    unpinned.resume(&session(), events().await?).await?;
    assert!(unpinned.key(dm, 1).is_none());
    assert_eq!(api.asked("POST /v1/keys/reject"), 1);
    Ok(())
}

#[tokio::test]
async fn protocol_three_reuses_keys_under_exclusions_but_not_stripped_ones() -> Result<()> {
    let profile = tempfile::tempdir()?;
    let api = Api::start().await?;
    let server = Server::for_test(3, profile.path()).at(api.url.clone(), SERVER);
    let (words, me) = DeviceIdentity::create("reader")?;
    let sibling = DeviceIdentity::add_device(&words, "sibling")?;
    let key = keys::random_key()?;
    let slot = (space(), 1);
    let excluded = Excluded::from([exclusion_id(me.certificate.person_id(), &[7; 32])]);
    let commitment = EnvelopeContext::new(SERVER, slot.0, slot.1, &key)
        .excluding(&excluded)
        .commitment;
    let mut listed = wrap(SERVER, slot, &key, commitment, &me, &sibling)?;
    listed.excluded = excluded;
    api.devices(&[&me, &sibling]);
    api.set("POST /v1/keys/reject", vec![(200, "{}".into())]);
    api.keys(&[state(&[slot], vec![listed.clone()])]);
    let mut live = connect(&server, &me, Some(HashMap::new()), &Verified::default()).await?;
    assert_eq!(opens(), 1);
    live.resume(&session(), events().await?).await?;
    assert_eq!(opens(), 0);
    assert!(live.key(space(), 1) == Some(&key));

    // The server leaves out the exclusion the key was committed under.
    let mut stripped = listed;
    stripped.excluded = Excluded::new();
    api.keys(&[state(&[slot], vec![stripped]), state(&[slot], vec![])]);
    live.resume(&session(), events().await?).await?;
    assert!(live.key(space(), 1).is_none());
    assert_eq!(api.asked("POST /v1/keys/reject"), 1);
    Ok(())
}
