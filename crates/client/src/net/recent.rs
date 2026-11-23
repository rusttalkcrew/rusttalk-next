//! Read-only startup view. Only verified, opened records enter local history.
//! Live directories and first pages replace it after authentication.
use std::{fs, io::Read, sync::Mutex};

use super::*;
use crate::server::{Category, Dm, Member};
use rusttalk_core::{
    auth::decode_bytes,
    client::revocations::Revocations,
    keys::{Key, open_cache, seal_cache},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

const LIMIT: usize = 16 * 1024 * 1024;
const CHANNELS: usize = 12;
const SERVERS: usize = 8;
// The profile lock excludes other app processes. Serialize competing workers' evictions.
static FILES: Mutex<()> = Mutex::new(());

/// Shared with the UI so a departing worker cannot recreate removed files.
pub(super) struct Store {
    dir: PathBuf,
    scope: Id,
    key: Key,
    active: Mutex<bool>,
}

impl Store {
    pub fn new(data: &Path, identity: &DeviceIdentity, server: &KnownServer) -> Self {
        let scope: Id = Sha256::new()
            .chain_update(identity.certificate.person_id())
            .chain_update(identity.certificate.signing_key().as_bytes())
            .chain_update(serde_json::to_vec(server).expect("known server serializes"))
            .finalize()
            .into();
        Self {
            dir: data.join("recent").join(hex::encode(scope)),
            scope,
            key: identity.cache_key(),
            active: Mutex::new(true),
        }
    }

    fn aad(&self, name: &str) -> Vec<u8> {
        [self.scope.as_slice(), name.as_bytes()].concat()
    }

    fn read<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        let active = self.active.lock().unwrap();
        if !*active {
            return Ok(None);
        }
        let file = match fs::File::open(self.dir.join(name)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            other => other?,
        };
        let mut bytes = Vec::new();
        file.take((LIMIT + 1) as u64).read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len() <= LIMIT, "recent cache too large");
        let plain = Zeroizing::new(open_cache(&self.key, &self.aad(name), &bytes)?);
        Ok(Some(serde_json::from_slice(&plain)?))
    }

    pub fn write(&self, name: &str, value: &impl Serialize) {
        let active = self.active.lock().unwrap();
        if !*active {
            return;
        }
        let path = self.dir.join(name);
        let write = || -> Result<()> {
            let plain = Zeroizing::new(serde_json::to_vec(value)?);
            anyhow::ensure!(plain.len() + 40 <= LIMIT, "recent cache too large");
            let bytes = seal_cache(&self.key, &self.aad(name), &plain)?;
            let _files = FILES.lock().unwrap();
            if !self.dir.try_exists()? {
                let root = self.dir.parent().expect("cache root");
                fs::create_dir_all(root)?;
                let mut dirs = Vec::new();
                for entry in fs::read_dir(root)? {
                    let entry = entry?;
                    if entry.file_type()?.is_dir() {
                        dirs.push((entry.metadata()?.modified()?, entry.path()));
                    }
                }
                dirs.sort_unstable();
                for (_, dir) in dirs.iter().take(dirs.len().saturating_sub(SERVERS - 1)) {
                    fs::remove_dir_all(dir)?;
                }
                fs::create_dir(&self.dir)?;
            }
            // A process killed before the atomic rename can leave its sealed temporary file.
            for entry in fs::read_dir(&self.dir)? {
                let entry = entry?;
                if entry.file_name().to_string_lossy().starts_with('.') {
                    fs::remove_file(entry.path())?;
                }
            }
            crate::keystore::write_atomic(&path, &bytes)
        };
        if let Err(error) = write() {
            tracing::warn!(%error, "could not replace recent cache");
            // Never fall back to the previous version after a deletion or permission change.
            if let Err(error) = fs::remove_file(path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::error!(%error, "could not discard obsolete cache");
            }
        }
    }

    pub fn remove(&self) {
        let mut active = self.active.lock().unwrap();
        if !*active {
            return;
        }
        *active = false;
        let _files = FILES.lock().unwrap();
        if let Err(error) = fs::remove_dir_all(&self.dir)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::error!(%error, "could not clear recent cache");
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
pub(super) struct Listing {
    name: Option<String>,
    categories: Vec<Category>,
    members: Vec<(Id, Option<String>, bool)>,
    dms: Vec<Dm>,
    // Owner, owner-bound ciphertext hash, original image bytes.
    pictures: Vec<(Id, Id, String)>,
}

impl Listing {
    pub fn save(state: &State, dir: &Directory, members: &Value, space: &Value) {
        let mut photos = Vec::new();
        let mut budget = LIMIT / 2;
        let mut pixel_budget = 8 * 1024 * 1024;
        let mut add = |owner: Id, hash: Option<&str>, image: Option<&Picture>| {
            if let (Some(hash), Some(image)) = (hash, image)
                && photos.len() < 32
                && image.bytes.len() <= budget
                && image
                    .frames
                    .iter()
                    .map(|(frame, _)| frame.pixels.len())
                    .sum::<usize>()
                    <= pixel_budget
            {
                budget -= image.bytes.len();
                pixel_budget -= image
                    .frames
                    .iter()
                    .map(|(frame, _)| frame.pixels.len())
                    .sum::<usize>();
                photos.push((
                    owner,
                    directory::picture_key(&owner, hash),
                    encode(&image.bytes),
                ));
            }
        };
        add(
            state.server_id,
            space["icon_hash"].as_str(),
            dir.icon.as_ref(),
        );
        let hashes: HashMap<_, _> = members
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| {
                Some((
                    decode::<32>(row["person_id"].as_str()?).ok()?,
                    row["avatar_hash"].as_str()?,
                ))
            })
            .collect();
        for m in &dir.members {
            add(m.person, hashes.get(&m.person).copied(), m.avatar.as_ref());
        }
        let listing = Self {
            name: dir.name.clone(),
            categories: dir.categories.clone(),
            dms: dir.dms.clone(),
            members: dir
                .members
                .iter()
                .map(|m| (m.person, m.name.clone(), m.hide_offline))
                .collect(),
            pictures: photos,
        };
        state.store().write("directory", &listing);
    }

    fn unpack(self, state: &mut State, pins: Option<HashMap<Id, DmPin>>) -> Directory {
        let mut by_owner = HashMap::new();
        for (owner, key, bytes) in self.pictures {
            if let Ok(bytes) = decode_bytes(&bytes)
                && let Some(image) = directory::picture(&bytes)
            {
                state.pictures.insert(key, image.clone());
                by_owner.insert(owner, image);
            }
        }
        Directory {
            name: self.name,
            categories: self.categories,
            icon: by_owner.remove(&state.server_id),
            members: self
                .members
                .into_iter()
                .map(|(person, name, hide_offline)| Member {
                    person,
                    name,
                    hide_offline,
                    avatar: by_owner.remove(&person),
                    presence: Presence::Offline,
                    devices: vec![],
                    roles: vec![],
                    bio: None,
                    voice_muted: false,
                })
                .collect(),
            dms: self
                .dms
                .into_iter()
                .filter(|dm| {
                    matches!(pins.as_ref().and_then(|pins| pins.get(&dm.channel)),
                Some(DmPin::With {peer,..}) if peer == &dm.peer)
                })
                .collect(),
            ..Default::default()
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(super) struct Record {
    id: Id,
    person: Id,
    device: Id,
    #[serde(default)]
    epoch: Option<i64>,
    at: i64,
    backfilled: bool,
    body: String,
    edit_of: Option<Id>,
}

pub(super) type Source = (Id, Id, Option<i64>);

impl Record {
    pub fn source(&self) -> Source {
        (self.person, self.device, self.epoch)
    }

    pub fn new(record: &MessageRecord, body: &MessageBody) -> Option<Self> {
        Some(Self {
            id: record.id,
            person: record.person,
            device: record.sender_device,
            epoch: Some(record.epoch),
            at: record.at,
            backfilled: record.backfilled,
            body: encode(&body.cache_bytes().ok()?),
            edit_of: body.edit_of,
        })
    }

    fn message(&self, revoked: &Revocations) -> Option<Msg> {
        revoked.check(&self.person, &self.device).ok()?;
        let bytes = Zeroizing::new(decode_bytes(&self.body).ok()?);
        let body = MessageBody::from_cache(&bytes).ok()?;
        Some(super::display_message(
            self.id,
            self.person,
            self.at,
            self.backfilled,
            &body,
        ))
    }
}

#[derive(Default, Serialize, Deserialize)]
pub(super) struct History(VecDeque<Page>);

#[derive(Serialize, Deserialize)]
struct Page {
    channel: Id,
    peer: Option<Id>,
    records: Vec<Record>,
}

impl Page {
    fn visible(&self, dir: &Directory) -> bool {
        match self.peer {
            Some(peer) => dir
                .dms
                .iter()
                .any(|d| d.channel == self.channel && d.peer == peer),
            None => dir
                .categories
                .iter()
                .flat_map(|c| &c.channels)
                .any(|c| c.id == self.channel),
        }
    }
}

impl History {
    pub fn put(&mut self, channel: Id, peer: Option<Id>, records: Vec<Record>, replace: bool) {
        let mut page = self
            .0
            .iter()
            .position(|p| p.channel == channel)
            .and_then(|i| self.0.remove(i))
            .filter(|p| p.peer == peer)
            .map(|p| p.records)
            .unwrap_or_default();
        if replace {
            page.clear();
        }
        for record in records {
            page.retain(|r| r.id != record.id);
            page.push(record);
        }
        page.drain(..page.len().saturating_sub(PAGE as usize));
        if page.is_empty() {
            return;
        }
        self.0.push_back(Page {
            channel,
            peer,
            records: page,
        });
        if self.0.len() > CHANNELS {
            self.0.pop_front();
        }
    }

    pub fn delete(&mut self, channel: Id, id: Id) {
        if let Some(page) = self.0.iter_mut().find(|p| p.channel == channel) {
            page.records.retain(|r| r.id != id && r.edit_of != Some(id));
        }
    }
}

pub(super) fn visible(dir: &Directory, id: &Id) -> bool {
    dir.categories
        .iter()
        .flat_map(|c| &c.channels)
        .any(|c| &c.id == id)
        || dir.dms.iter().any(|d| &d.channel == id)
}

impl State {
    pub fn sent(
        &mut self,
        ctx: &Ctx,
        url: &Url,
        channel: Id,
        record: &MessageRecord,
        body: &MessageBody,
    ) {
        self.sources.entry(channel).or_default().insert((
            record.person,
            record.sender_device,
            Some(record.epoch),
        ));
        if let Some(record) = Record::new(record, body) {
            self.remember(channel, vec![record], false);
        }
        send_messages(ctx, url, channel, vec![to_msg(record, body)], false, None);
    }

    pub fn deleted(&mut self, ctx: &Ctx, url: &Url, channel: Id, id: Id) {
        self.history.delete(channel, id);
        if let Some(ids) = self.waiting.get_mut(&channel) {
            ids.remove(&id);
        }
        self.save_history();
        ctx.send(Update::Deleted(url.clone(), channel, id));
    }

    pub fn remember(&mut self, channel: Id, records: Vec<Record>, replace: bool) {
        let Some(dir) = self.last.as_ref().filter(|dir| visible(dir, &channel)) else {
            return;
        };
        let peer = dir
            .dms
            .iter()
            .find(|dm| dm.channel == channel)
            .map(|d| d.peer);
        self.history.put(channel, peer, records, replace);
        self.save_history();
    }

    pub fn store(&self) -> &Store {
        self.cache.as_deref().expect("started server")
    }

    pub fn save_history(&self) {
        self.store().write("messages", &self.history);
    }

    // A successful catalog or access denial is authoritative even if a later request times out.
    pub fn discard_channels(&mut self, ctx: &Ctx, url: &Url, ids: HashSet<Id>) {
        if ids.is_empty() {
            return;
        }
        self.history.0.retain(|page| !ids.contains(&page.channel));
        self.save_history();
        self.loaded.retain(|id| !ids.contains(id));
        self.waiting.retain(|id, _| !ids.contains(id));
        self.sources.retain(|id, _| !ids.contains(id));
        if let Some(dir) = &mut self.last {
            dir.discard_channels(&ids);
        }
        match self.store().read::<Listing>("directory") {
            Ok(Some(mut listing)) => {
                listing.dms.retain(|dm| !ids.contains(&dm.channel));
                for category in &mut listing.categories {
                    category
                        .channels
                        .retain(|channel| !ids.contains(&channel.id));
                }
                self.store().write("directory", &listing);
            }
            Err(error) => tracing::warn!(%error, "could not read obsolete directory"),
            Ok(None) => {}
        }
        ctx.send(Update::Inaccessible(url.clone(), ids));
    }

    pub fn prune_history(&mut self, dir: &Directory) {
        self.history.0.retain(|page| page.visible(dir));
        self.loaded.retain(|id| visible(dir, id));
        self.save_history();
    }

    pub fn revalidate(
        &mut self,
        ctx: &Ctx,
        url: &Url,
        revoked: &Revocations,
        client: Option<&KeyClient<'_>>,
    ) {
        use rusttalk_core::keys::{Kind, Scope};
        let mut pages = Vec::new();
        for (id, sources) in &mut self.sources {
            let scope = Scope {
                kind: Kind::Channel,
                id: *id,
            };
            let trusted = |(person, device, epoch): &Source| {
                revoked.check(person, device).is_ok()
                    && client.is_none_or(|client| match epoch {
                        Some(epoch) => client.key(scope, *epoch).is_some(),
                        // Older caches only recorded the source device and channel.
                        None => client.current_key(scope).is_ok(),
                    })
            };
            let before = sources.len();
            sources.retain(trusted);
            if sources.len() == before {
                continue;
            }
            let mut items = Vec::new();
            if let Some(page) = self.history.0.iter_mut().find(|p| p.channel == *id) {
                page.records.retain(|r| sources.contains(&r.source()));
                items.extend(page.records.iter().filter_map(|r| r.message(revoked)));
            }
            pages.push((*id, items));
        }
        if !pages.is_empty() {
            self.save_history();
            ctx.send(Update::ResetHistory(url.clone(), pages));
        }
    }

    pub fn restore(&mut self, ctx: &Ctx, url: &Url, records: &Revocations) -> Result<()> {
        let Some(listing) = self.store().read::<Listing>("directory")? else {
            return Ok(());
        };
        let mut dir = listing.unpack(self, load_pins(&ctx.data, &self.server_id));
        dir.me = *ctx.identity.certificate.person_id();
        dir.my_name_hidden = true; // Only live profile data may trigger a profile update.
        self.history = self
            .store()
            .read("messages")
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "could not restore recent messages");
                None
            })
            .unwrap_or_default();
        self.history.0.retain(|page| page.visible(&dir));
        let mut pages = Vec::new();
        for page in &self.history.0 {
            self.loaded.insert(page.channel);
            self.sources.insert(
                page.channel,
                page.records.iter().map(Record::source).collect(),
            );
            pages.push((
                page.channel,
                page.records
                    .iter()
                    .filter_map(|r| r.message(records))
                    .collect(),
            ));
        }
        self.last = Some(dir.clone());
        ctx.send(Update::Cached(url.clone(), Box::new(dir), pages));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The child gets only a profile path. No decoded pictures, keys or chat objects
    // can survive in memory, and the listening socket never answers a TLS handshake.
    #[test]
    fn cold_start_before_server_responds() -> Result<()> {
        const PROFILE: &str = "RUSTTALK_TEST_OFFLINE_PROFILE";
        const EXPECTED: &str = "RUSTTALK_TEST_OFFLINE_COUNT";
        if let Some(path) = std::env::var_os(PROFILE) {
            let path = PathBuf::from(path);
            let (_lock, start) = crate::keystore::Keystore::open(&path, "RustTalk-offline-test")?;
            let crate::keystore::Start::Ready(Some(device)) = start else {
                anyhow::bail!("test identity unavailable");
            };
            let ctx = egui::Context::default();
            let net = Net::new(
                ctx.clone(),
                Arc::from(device),
                path.clone(),
                Audio::new(&path)?,
                Video::new(ctx),
            )?;
            let known = net.joined().pop().context("saved server")?;
            let _commands = net.start(known, None, None);
            let Update::Cached(_, dir, pages) = net.updates.recv_timeout(Duration::from_secs(5))?
            else {
                anyhow::bail!("connection started before cache restore");
            };
            assert_eq!(dir.name.as_deref(), Some("Workshop"));
            assert!(dir.icon.is_some() && dir.members[0].avatar.is_some());
            assert_eq!(pages[0].1.len(), std::env::var(EXPECTED)?.parse::<usize>()?);
            assert!(matches!(&pages[0].1[0].body, Body::Text(text) if text == "Saturday"));
            return Ok(());
        }

        let root = tempfile::tempdir()?;
        let path = root.path();
        let socket = std::net::TcpListener::bind("127.0.0.1:0")?;
        let known = KnownServer {
            address: format!("https://{}/", socket.local_addr()?).parse()?,
            server_id: [3; 32],
            tls_fingerprint: Some([4; 32]),
        };
        let (_, device) = DeviceIdentity::create("Cache test")?;
        let person = *device.certificate.person_id();
        let mut keystore = crate::keystore::without_keychain(path);
        keystore.choose(crate::keystore::Kind::File, None)?;
        keystore.save(&device)?;
        drop(keystore);
        let sync = DeviceSync::open(path)?;
        Runtime::new()?.block_on(sync.edit(path, vec![Edit::Join(known.clone())]))?;
        let store = Store::new(path, &device, &known);
        let photo = encode(include_bytes!("../../assets/rusttalk.png"));
        let channel = [6; 32];
        let listing = Listing {
            name: Some("Workshop".into()),
            members: vec![(person, None, false)],
            categories: vec![Category {
                id: None,
                name: String::new(),
                channels: vec![crate::server::Channel {
                    id: channel,
                    name: "weekend".into(),
                    topic: String::new(),
                    voice: false,
                    page: false,
                    forum: true,
                    audio_kbps: 0,
                    flags: vec![],
                }],
            }],
            pictures: vec![
                (known.server_id, [7; 32], photo.clone()),
                (person, [8; 32], photo),
            ],
            ..Default::default()
        };
        let mut legacy = serde_json::to_value(&listing)?;
        legacy["categories"][0]["channels"][0]
            .as_object_mut()
            .unwrap()
            .remove("forum");
        store.write("directory", &legacy);
        assert!(!store.read::<Listing>("directory")?.unwrap().categories[0].channels[0].forum);
        store.write("directory", &listing);
        assert!(store.read::<Listing>("directory")?.unwrap().categories[0].channels[0].forum);
        let mut history = History::default();
        let records = [9, 10]
            .into_iter()
            .map(|n| {
                Ok(Record {
                    id: [n; 32],
                    person,
                    epoch: Some(1),
                    device: device.certificate.signing_key().to_bytes(),
                    at: n.into(),
                    backfilled: false,
                    edit_of: None,
                    body: encode(
                        &MessageBody {
                            text: "Saturday".into(),
                            ..Default::default()
                        }
                        .cache_bytes()?,
                    ),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        history.put(channel, None, records, true);
        store.write("messages", &history);
        // A successful live directory followed by a first-page timeout must leave
        // the same readable page for a second process with the network still down.
        let device = Arc::new(device);
        for pass in 0..2 {
            let child = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "net::recent::tests::cold_start_before_server_responds",
                ])
                .env(PROFILE, path)
                .env(EXPECTED, (2 - pass).to_string())
                .status()?;
            anyhow::ensure!(child.success(), "cold-start process failed");
            if pass == 0 {
                let ctx = egui::Context::default();
                let net = Net::new(
                    ctx.clone(),
                    device.clone(),
                    path.into(),
                    Audio::new(path)?,
                    Video::new(ctx),
                )?;
                let mut state = State {
                    cache: Some(Arc::new(Store::new(path, &device, &known))),
                    ..Default::default()
                };
                state.restore(&net.ctx, &known.address, &Revocations::in_profile(path))?;
                let dir = state.last.clone().unwrap();
                state.prune_history(&dir);
                state.deleted(&net.ctx, &known.address, channel, [10; 32]);
                let error = net.handle().block_on(async {
                    reqwest::Client::new()
                        .get(known.address.clone())
                        .timeout(Duration::from_millis(100))
                        .send()
                        .await
                        .unwrap_err()
                });
                assert!(error.is_timeout());
                let error = anyhow::Error::from(error);
                assert!(verification_incomplete(&error));
                page_failed(&net.ctx, &known.address, &mut state, channel, None, &error);

                // The page itself arrived, but fetching the original for an edit
                // ended in the middle of its JSON response.
                use std::io::{BufRead, BufReader, Write};
                let original = std::net::TcpListener::bind("127.0.0.1:0")?;
                let address = original.local_addr()?;
                let responder = std::thread::spawn(move || -> std::io::Result<()> {
                    let (mut stream, _) = original.accept()?;
                    for line in BufReader::new(&stream).lines() {
                        if line?.is_empty() {
                            break;
                        }
                    }
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 128\r\n\r\n{")
                });
                let error = net
                    .handle()
                    .block_on(async {
                        reqwest::Client::builder()
                            .no_proxy()
                            .build()?
                            .get(format!("http://{address}/original"))
                            .send()
                            .await?
                            .error_for_status()?
                            .json::<MessageRecord>()
                            .await
                            .map(|_| ())
                    })
                    .unwrap_err();
                responder.join().unwrap()?;
                assert!(error.is_decode() || error.is_body());
                let error = anyhow::Error::from(error);
                assert!(verification_incomplete(&error));
                page_failed(&net.ctx, &known.address, &mut state, channel, None, &error);
            }
        }
        Ok(())
    }

    #[test]
    fn previous_session_records_are_scoped_bounded_and_removable() {
        let root = tempfile::tempdir().unwrap();
        let (_, device) = DeviceIdentity::create("Local cache test").unwrap();
        let mut known = KnownServer {
            address: "https://127.0.0.1:1/".parse().unwrap(),
            server_id: [1; 32],
            tls_fingerprint: None,
        };
        let store = Store::new(root.path(), &device, &known);
        let person = *device.certificate.person_id();
        let signing_key = device.certificate.signing_key().to_bytes();
        let channel = [1; 32];
        let record = |n: u8, edit_of| {
            let body = MessageBody {
                text: format!("Private record {n}"),
                edit_of,
                ..Default::default()
            };
            Record {
                id: [n; 32],
                person,
                epoch: Some(1),
                device: signing_key,
                at: n.into(),
                backfilled: false,
                edit_of,
                body: encode(&body.cache_bytes().unwrap()),
            }
        };
        let mut history = History::default();
        for n in 0..=CHANNELS {
            history.put([n as u8; 32], None, vec![record(1, None)], true);
        }
        assert_eq!(history.0.len(), CHANNELS);
        let page = (0..=PAGE as u8).map(|n| record(n, None)).collect();
        history.put(channel, None, page, true);
        history.put(channel, None, vec![record(81, Some([49; 32]))], false);
        history.delete(channel, [49; 32]);
        store.write("messages", &history);
        let bytes = fs::read(store.dir.join("messages")).unwrap();
        assert!(!bytes.windows(7).any(|w| w == b"Private"));
        assert!(open_cache(&Key::new([5; 32]), &store.aad("messages"), &bytes).is_err());
        assert!(open_cache(&store.key, &store.aad("directory"), &bytes).is_err());
        known.server_id = [9; 32];
        let other = Store::new(root.path(), &device, &known);
        assert!(open_cache(&store.key, &other.aad("messages"), &bytes).is_err());
        let reopened: History = store.read("messages").unwrap().unwrap();
        let page = &reopened.0.back().unwrap().records;
        assert!(page.len() <= PAGE as usize);
        assert!(page.iter().all(|r| ![0, 49, 81].contains(&r.id[0])));
        let revoked = Revocations::in_profile(root.path());
        assert!(page[0].message(&revoked).is_some());
        let dir = root.path().join("revocations").join(hex::encode(person));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(hex::encode(signing_key)), b"damaged tombstone").unwrap();
        assert!(page[0].message(&revoked).is_none());
        history.put(channel, None, vec![], true);
        assert!(history.0.iter().all(|p| p.channel != channel));
        store.remove();
        store.write("messages", &history);
        assert!(!store.dir.exists());
    }
}
