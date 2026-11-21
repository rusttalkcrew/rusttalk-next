//! Link previews (#172). The app showing a message fetches details for its links
//! itself, only from sites the person allowed on this device, and only for messages
//! on screen. Neither the sender nor the RustTalk server is involved (see docs/privacy.md).

use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::{Cursor, Read},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::i18n::Lang;

pub mod steam;

/// Every site previews can come from. Each needs the person's yes first.
pub static PROVIDERS: &[&dyn Provider] = &[&steam::Steam];

pub const MAX_TITLE: usize = 120;
pub const MAX_DESCRIPTION: usize = 300;
pub const MAX_FIELD: usize = 80;
/// Preview cards under one message, at most.
pub const MAX_PER_MESSAGE: usize = 3;
const MAX_JSON: u64 = 512 * 1024;
const MAX_IMAGE: u64 = 1024 * 1024;
/// Decoding refuses pictures bigger than this or needing more memory than `MAX_DECODE`.
const MAX_SIDE: u32 = 2048;
const MAX_DECODE: u64 = 32 * 1024 * 1024;
/// Pictures are kept at most this big; bigger ones are scaled down once.
const IMAGE_WIDTH: u32 = 460;
const IMAGE_HEIGHT: u32 = 300;
const TIMEOUT: Duration = Duration::from_secs(10);
const WORKERS: usize = 2;
/// At most one request starts per this long, across workers.
const GAP: Duration = Duration::from_secs(1);
/// A link counts as on screen until it hasn't been drawn for this long.
const SEEN: Duration = Duration::from_secs(1);
/// While a preview loads, the screen is redrawn at least this often, renewing `SEEN`.
const REPAINT: Duration = Duration::from_millis(500);
/// Prices change; anything older is fetched again when shown, while the old copy stays up.
const FRESH: u64 = 60 * 60;
/// Previews are dropped after a day, in memory and on disk.
const KEEP: u64 = 24 * 60 * 60;
const MAX_SAVED: usize = 200;
/// Previews (and their pictures) kept in memory, least recently shown dropped first.
const MAX_MEMORY: usize = 50;
/// After a failed fetch, the whole site waits this long, so a down site isn't hammered.
const RETRY: Duration = Duration::from_secs(10 * 60);
const DIR: &str = "previews";

/// A site that can preview some of its links.
pub trait Provider: Send + Sync {
    /// The settings key.
    fn id(&self) -> &'static str;
    /// The site's name, shown on cards and in Settings.
    fn name(&self) -> &'static str;
    /// What to fetch for `url`, or `None` when it isn't this site's kind of link.
    fn key(&self, url: &Url) -> Option<String>;
    /// The details for `key`, in the app's language and the computer's country.
    fn fetch(&self, get: &Get, key: &str, lang: Lang, country: Option<&str>) -> Option<Details>;
    /// Whether a picture may be downloaded from `host`.
    fn image_host(&self, host: &str) -> bool;
}

/// The links a site can preview, once per site and key, at most `MAX_PER_MESSAGE`.
pub fn supported<'a>(links: impl Iterator<Item = &'a str>) -> Vec<(&'static dyn Provider, String)> {
    let mut found: Vec<(&'static dyn Provider, String)> = Vec::new();
    for url in links.filter_map(|link| Url::parse(link).ok()) {
        let Some((provider, key)) = find(&url) else {
            continue;
        };
        if !found
            .iter()
            .any(|(p, k)| p.id() == provider.id() && *k == key)
        {
            found.push((provider, key));
            if found.len() == MAX_PER_MESSAGE {
                break;
            }
        }
    }
    found
}

fn find(url: &Url) -> Option<(&'static dyn Provider, String)> {
    PROVIDERS
        .iter()
        .find_map(|provider| Some((*provider, provider.key(url)?)))
}

/// `text` as an HTTPS address on an allowed host, without a user name or password
/// and on the normal port.
pub fn safe_url(text: &str, host: impl Fn(&str) -> bool) -> Option<Url> {
    let url = Url::parse(text).ok().filter(|_| text.len() <= 1024)?;
    (url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url.host_str().is_some_and(host))
    .then_some(url)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Field {
    Price,
    Released,
    Developer,
    Genres,
    Platforms,
}

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Self::Price => t!("previews.price"),
            Self::Released => t!("previews.released"),
            Self::Developer => t!("previews.developer"),
            Self::Genres => t!("previews.genres"),
            Self::Platforms => t!("previews.platforms"),
        }
    }
}

/// What a site said about a link, checked and shortened.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Details {
    pub title: String,
    pub description: String,
    pub fields: Vec<(Field, String)>,
    /// Where "View on" goes; built by the provider, not taken from the site's reply.
    pub url: String,
    /// Checked with `safe_url` against the provider's picture hosts.
    pub image: Option<String>,
}

pub struct Preview {
    pub details: Details,
    /// The picture until it becomes a texture.
    image: Mutex<Option<ColorImage>>,
    /// Freed with the preview, so dropping it from memory frees the picture too.
    texture: OnceLock<TextureHandle>,
}

impl Preview {
    fn new(details: Details, image: Option<ColorImage>) -> Self {
        Self {
            details,
            image: Mutex::new(image),
            texture: OnceLock::new(),
        }
    }

    pub fn texture(&self, ctx: &egui::Context) -> Option<&TextureHandle> {
        if let Some(texture) = self.texture.get() {
            return Some(texture);
        }
        let image = lock(&self.image).take()?;
        Some(
            self.texture
                .get_or_init(|| ctx.load_texture("preview", image, TextureOptions::LINEAR)),
        )
    }
}

/// Text from a site as one plain line: tags removed, entities decoded,
/// whitespace collapsed, at most `max` characters.
pub fn plain(text: &str, max: usize) -> String {
    static NUMERIC: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"&#(?:[xX]([0-9a-fA-F]{1,6})|([0-9]{1,7}));").expect("entity pattern")
    });
    let mut out = String::new();
    let mut tag = false;
    for c in text.chars() {
        match c {
            '<' => tag = true,
            '>' if tag => {
                tag = false;
                out.push(' ');
            }
            _ if !tag => out.push(c),
            _ => {}
        }
    }
    let out = NUMERIC.replace_all(&out, |caps: &regex::Captures| {
        let code = match (caps.get(1), caps.get(2)) {
            (Some(hex), _) => u32::from_str_radix(hex.as_str(), 16).ok(),
            (_, Some(dec)) => dec.as_str().parse().ok(),
            _ => None,
        };
        code.and_then(char::from_u32)
            .map_or(String::new(), String::from)
    });
    let mut out = out.into_owned();
    for (entity, c) in [
        ("&quot;", "\""),
        ("&apos;", "'"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&nbsp;", " "),
        ("&amp;", "&"),
    ] {
        out = out.replace(entity, c);
    }
    // Controls (also line breaks) count as spaces; bidirectional overrides, which
    // could make text read differently than it is, are dropped.
    let out: String = (out.chars())
        .filter(|c| {
            !matches!(c, '\u{61C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
        })
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let line = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        return line;
    }
    let mut short: String = line.chars().take(max.saturating_sub(1)).collect();
    short.truncate(short.trim_end().len());
    short.push('…');
    short
}

/// HTTPS requests that carry nothing about the person: a generic User-Agent, no
/// cookies, no Referer, no redirects, a timeout and a size cap.
pub struct Get(reqwest::blocking::Client);

impl Get {
    fn new() -> reqwest::Result<Self> {
        reqwest::blocking::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .referer(false)
            .user_agent("RustTalk")
            .timeout(TIMEOUT)
            .build()
            .map(Self)
    }

    /// The body of `url`, when it passes `safe_url` with `host` and fits in `max` bytes.
    pub fn get(&self, url: &Url, max: u64, host: impl Fn(&str) -> bool) -> Option<Vec<u8>> {
        let url = safe_url(url.as_str(), host)?;
        let reply = match self.0.get(url).send().and_then(|r| r.error_for_status()) {
            Ok(reply) => reply,
            Err(error) => {
                tracing::info!(error = %error.without_url(), "link preview fetch failed");
                return None;
            }
        };
        let mut bytes = Vec::new();
        // One byte over the limit tells a too-big reply apart.
        reply.take(max + 1).read_to_end(&mut bytes).ok()?;
        (bytes.len() as u64 <= max).then_some(bytes)
    }
}

enum Entry {
    Loading,
    Ready {
        preview: Arc<Preview>,
        /// From when (Unix seconds) it may be fetched again; `u64::MAX` while it is.
        refresh: u64,
        /// Until when (Unix seconds) it may be shown.
        kept: u64,
    },
    Failed(Instant),
}

struct Slot {
    entry: Entry,
    used: Instant,
}

struct Job {
    provider: &'static dyn Provider,
    key: String,
    cache: String,
    lang: Lang,
    country: Option<String>,
    /// The site's consent generation when queued; results from an older one are dropped.
    generation: u64,
}

struct Shared {
    slots: Mutex<HashMap<String, Slot>>,
    queue: Mutex<VecDeque<Job>>,
    wake: Condvar,
    /// Per site: allowed right now, and a generation bumped each time that changes.
    consent: Mutex<HashMap<&'static str, (bool, u64)>>,
    /// Links drawn on screen lately, by cache key.
    seen: Mutex<HashMap<String, Instant>>,
    /// Per site: no request before this.
    backoff: Mutex<HashMap<&'static str, Instant>>,
    next: Mutex<Instant>,
    /// Set at sign-out: nothing is fetched or written after.
    stopped: AtomicBool,
    /// Held for every write, so none is under way once `stopped` is set.
    write: Mutex<()>,
    dir: PathBuf,
    ctx: egui::Context,
}

/// Previews fetched and saved on this device. Dropping it stops all of it.
pub struct Previews {
    shared: Arc<Shared>,
    started: OnceLock<()>,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    at: u64,
    details: Details,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Remove the saved previews, at sign-out.
pub fn forget(data: &Path) {
    match fs::remove_dir_all(data.join(DIR)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            tracing::error!(%error, "removing link previews at sign out failed");
        }
        _ => {}
    }
}

impl Previews {
    pub fn new(ctx: &egui::Context, data: &Path) -> Self {
        Self {
            shared: Arc::new(Shared {
                slots: Mutex::default(),
                queue: Mutex::default(),
                wake: Condvar::new(),
                consent: Mutex::default(),
                seen: Mutex::default(),
                backoff: Mutex::default(),
                next: Mutex::new(Instant::now()),
                stopped: AtomicBool::new(false),
                write: Mutex::new(()),
                dir: data.join(DIR),
                ctx: ctx.clone(),
            }),
            started: OnceLock::new(),
        }
    }

    /// A preview already loaded, without asking for one (for links off screen).
    pub fn cached(&self, provider: &dyn Provider, key: &str) -> Option<Arc<Preview>> {
        let mut slots = lock(&self.shared.slots);
        let slot = slots.get_mut(&cache_key(provider, key))?;
        match &slot.entry {
            Entry::Ready { preview, kept, .. } if now() < *kept => {
                slot.used = Instant::now();
                Some(preview.clone())
            }
            _ => None,
        }
    }

    /// The preview for a link on screen, from a site the person allowed. Starts
    /// loading it, or refreshing an old one, in the background.
    pub fn get(&self, provider: &'static dyn Provider, key: &str) -> Option<Arc<Preview>> {
        let cache = cache_key(provider, key);
        let mut seen = lock(&self.shared.seen);
        if seen.len() > 4 * MAX_MEMORY {
            seen.retain(|_, at| at.elapsed() < SEEN);
        }
        seen.insert(cache.clone(), Instant::now());
        drop(seen);
        self.allow(provider.id(), true);
        let generation = lock(&self.shared.consent)
            .get(provider.id())
            .map_or(0, |(_, generation)| *generation);
        let (shown, start) = self.claim(&cache);
        if start {
            self.send(Job {
                provider,
                key: key.to_owned(),
                cache: cache.clone(),
                lang: crate::i18n::current(),
                country: country(),
                generation,
            });
        }
        // Keep drawing while it loads, so the link stays counted as on screen even
        // when nothing else repaints.
        if matches!(
            lock(&self.shared.slots).get(&cache).map(|slot| &slot.entry),
            Some(
                Entry::Loading
                    | Entry::Ready {
                        refresh: u64::MAX,
                        ..
                    }
            )
        ) {
            self.shared.ctx.request_repaint_after(REPAINT);
        }
        shown
    }

    /// What is shown for `cache` now, and whether to start loading it.
    fn claim(&self, cache: &str) -> (Option<Arc<Preview>>, bool) {
        let mut slots = lock(&self.shared.slots);
        let now = now();
        let (shown, start) = match slots.get_mut(cache) {
            Some(slot) => {
                slot.used = Instant::now();
                match &mut slot.entry {
                    Entry::Ready { kept, .. } if now >= *kept => (None, true),
                    Entry::Ready {
                        preview, refresh, ..
                    } => {
                        let start = now >= *refresh;
                        if start {
                            // Not again until this refresh is done.
                            *refresh = u64::MAX;
                        }
                        (Some(preview.clone()), start)
                    }
                    Entry::Loading => (None, false),
                    Entry::Failed(at) => (None, at.elapsed() >= RETRY),
                }
            }
            None => (None, true),
        };
        if start && shown.is_none() {
            insert(&mut slots, cache.to_owned(), Entry::Loading);
        }
        (shown, start)
    }

    /// Record that a site was allowed or turned off.
    pub fn allow(&self, id: &'static str, on: bool) {
        if on {
            let mut consent = lock(&self.shared.consent);
            let (allowed, generation) = consent.entry(id).or_default();
            if !*allowed {
                *allowed = true;
                *generation += 1;
            }
        } else {
            revoke(&self.shared, id);
        }
    }

    fn send(&self, job: Job) {
        self.started.get_or_init(|| {
            let get = Get::new().ok().map(Arc::new);
            for index in 0..WORKERS {
                let (shared, get) = (self.shared.clone(), get.clone());
                let spawned = thread::Builder::new()
                    .name(format!("previews-{index}"))
                    .spawn(move || work(&shared, get.as_deref()));
                if let Err(error) = spawned {
                    tracing::warn!(%error, "starting link previews failed");
                }
            }
            let shared = self.shared.clone();
            let _ = thread::Builder::new()
                .name("previews-prune".into())
                .spawn(move || prune(&shared));
        });
        let mut queue = lock(&self.shared.queue);
        if !queue.iter().any(|queued| queued.cache == job.cache) {
            queue.push_back(job);
            self.shared.wake.notify_one();
        }
    }
}

impl Drop for Previews {
    fn drop(&mut self) {
        stop(&self.shared);
    }
}

fn stop(shared: &Shared) {
    {
        let _write = lock(&shared.write);
        shared.stopped.store(true, Ordering::SeqCst);
        for (allowed, generation) in lock(&shared.consent).values_mut() {
            *allowed = false;
            *generation += 1;
        }
    }
    lock(&shared.queue).clear();
    shared.wake.notify_all();
}

/// Turn a site off: under the write lock, so no result of its older requests is
/// kept after this, and its queued work is dropped and asked for again later.
fn revoke(shared: &Shared, id: &'static str) {
    {
        let _write = lock(&shared.write);
        let mut consent = lock(&shared.consent);
        let (allowed, generation) = consent.entry(id).or_default();
        *allowed = false;
        *generation += 1;
    }
    let dropped: VecDeque<Job> = {
        let mut queue = lock(&shared.queue);
        let (dropped, kept) = queue.drain(..).partition(|job| job.provider.id() == id);
        *queue = kept;
        dropped
    };
    for job in &dropped {
        abandon(shared, job);
    }
}

/// Add an entry, dropping the least recently shown beyond `MAX_MEMORY`.
fn insert(slots: &mut HashMap<String, Slot>, cache: String, entry: Entry) {
    let used = slots
        .get(&cache)
        .map_or_else(Instant::now, |slot| slot.used);
    slots.insert(cache, Slot { entry, used });
    while slots.len() > MAX_MEMORY {
        let Some(oldest) = (slots.iter())
            .min_by_key(|(_, slot)| slot.used)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        slots.remove(&oldest);
    }
}

fn cache_key(provider: &dyn Provider, key: &str) -> String {
    format!(
        "{}:{key}:{}:{}",
        provider.id(),
        crate::i18n::current().id(),
        country().unwrap_or_default()
    )
}

/// The computer's country, such as `NL` from `nl-NL`, for local prices.
fn country() -> Option<String> {
    let locale = sys_locale::get_locale()?;
    let region = locale.split(['-', '_', '.']).nth(1)?;
    (region.len() == 2 && region.bytes().all(|b| b.is_ascii_alphabetic()))
        .then(|| region.to_ascii_uppercase())
}

fn work(shared: &Shared, get: Option<&Get>) {
    loop {
        let job = {
            let mut queue = lock(&shared.queue);
            loop {
                if shared.stopped.load(Ordering::SeqCst) {
                    return;
                }
                if let Some(job) = queue.pop_front() {
                    break job;
                }
                queue = shared
                    .wake
                    .wait(queue)
                    .unwrap_or_else(|poison| poison.into_inner());
            }
        };
        if let Some(get) = get {
            run(shared, get, &job);
        }
    }
}

/// Not signed out, and the site still allowed since the job was queued.
fn allowed(shared: &Shared, job: &Job) -> bool {
    !shared.stopped.load(Ordering::SeqCst)
        && lock(&shared.consent).get(job.provider.id()) == Some(&(true, job.generation))
}

/// Allowed, on screen, and the site isn't waiting after a failure.
fn wanted(shared: &Shared, job: &Job) -> bool {
    allowed(shared, job)
        && lock(&shared.seen)
            .get(&job.cache)
            .is_some_and(|at| at.elapsed() < SEEN)
        && lock(&shared.backoff)
            .get(job.provider.id())
            .is_none_or(|until| Instant::now() >= *until)
}

/// Load one preview: from disk, else (or when old) from the site.
fn run(shared: &Shared, get: &Get, job: &Job) {
    let file = shared
        .dir
        .join(hex::encode(Sha256::digest(job.cache.as_bytes())));
    let saved = load(&file);
    if let Some((preview, at)) = &saved {
        let fresh = now() < at + FRESH;
        let refresh = if fresh { at + FRESH } else { u64::MAX };
        publish(shared, job, preview.clone(), refresh, at + KEEP);
        if fresh {
            return;
        }
    }
    if !wanted(shared, job) {
        return abandon(shared, job);
    }
    let start = {
        let mut next = lock(&shared.next);
        let start = (*next).max(Instant::now());
        *next = start + GAP;
        start
    };
    thread::sleep(start.saturating_duration_since(Instant::now()));
    // Scrolled away or turned off while waiting.
    if !wanted(shared, job) {
        return abandon(shared, job);
    }
    let fetched = fetch(get, job, || allowed(shared, job));
    if fetched.is_none() {
        lock(&shared.backoff).insert(job.provider.id(), Instant::now() + RETRY);
    }
    // Turned off while the request ran: keep nothing from it.
    if !allowed(shared, job) {
        return abandon(shared, job);
    }
    let committed = match (fetched, saved) {
        (Some((details, image, bytes)), _) => {
            let at = now();
            save(shared, job, &file, &details, bytes.as_deref())
                && publish(
                    shared,
                    job,
                    Arc::new(Preview::new(details, image)),
                    at + FRESH,
                    at + KEEP,
                )
        }
        // A failed refresh keeps the old copy, without an error.
        (None, Some((preview, at))) => {
            publish(shared, job, preview, now() + RETRY.as_secs(), at + KEEP)
        }
        (None, None) => set(shared, job, Entry::Failed(Instant::now())),
    };
    if !committed {
        abandon(shared, job);
    }
}

/// Not fetched after all: ask again when the link is next on screen.
fn abandon(shared: &Shared, job: &Job) {
    let mut slots = lock(&shared.slots);
    match slots.get_mut(&job.cache).map(|slot| &mut slot.entry) {
        Some(Entry::Loading) => {
            slots.remove(&job.cache);
        }
        Some(Entry::Ready { refresh, .. }) if *refresh == u64::MAX => *refresh = now(),
        _ => {}
    }
}

fn publish(shared: &Shared, job: &Job, preview: Arc<Preview>, refresh: u64, kept: u64) -> bool {
    set(
        shared,
        job,
        Entry::Ready {
            preview,
            refresh,
            kept,
        },
    )
}

/// Keep `entry`, unless signed out or the site was turned off since the job was queued.
fn set(shared: &Shared, job: &Job, entry: Entry) -> bool {
    let _write = lock(&shared.write);
    if !allowed(shared, job) {
        return false;
    }
    insert(&mut lock(&shared.slots), job.cache.clone(), entry);
    shared.ctx.request_repaint();
    true
}

type Fetched = (Details, Option<ColorImage>, Option<Vec<u8>>);

/// The details, and the picture decoded and as downloaded.
/// `go` is checked before every request, so none starts after sign-out or turning off.
fn fetch(get: &Get, job: &Job, go: impl Fn() -> bool) -> Option<Fetched> {
    if !go() {
        return None;
    }
    let details = job
        .provider
        .fetch(get, &job.key, job.lang, job.country.as_deref())?;
    let bytes = (details.image.as_deref())
        .filter(|_| go())
        .and_then(|u| safe_url(u, |host| job.provider.image_host(host)))
        .and_then(|url| get.get(&url, MAX_IMAGE, |host| job.provider.image_host(host)));
    let image = bytes.as_deref().and_then(picture);
    let bytes = image.is_some().then_some(bytes).flatten();
    Some((details, image, bytes))
}

/// A picture within the size and memory limits, scaled down to fit a card.
fn picture(bytes: &[u8]) -> Option<ColorImage> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(MAX_DECODE);
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.limits(limits);
    let image = reader.decode().ok()?;
    let image = if image.width() > IMAGE_WIDTH || image.height() > IMAGE_HEIGHT {
        image.resize(
            IMAGE_WIDTH,
            IMAGE_HEIGHT,
            image::imageops::FilterType::Triangle,
        )
    } else {
        image
    };
    let rgba = image.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Some(ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

fn load(file: &Path) -> Option<(Arc<Preview>, u64)> {
    let saved: Saved = serde_json::from_slice(&fs::read(file.with_extension("json")).ok()?).ok()?;
    if now() >= saved.at + KEEP {
        return None;
    }
    let image = fs::read(file.with_extension("img"))
        .ok()
        .and_then(|bytes| picture(&bytes));
    Some((Arc::new(Preview::new(saved.details, image)), saved.at))
}

/// Write a fetched preview, unless the job lost its consent; `false` then.
fn save(shared: &Shared, job: &Job, file: &Path, details: &Details, image: Option<&[u8]>) -> bool {
    {
        let _write = lock(&shared.write);
        if !allowed(shared, job) {
            return false;
        }
        let saved = Saved {
            at: now(),
            details: details.clone(),
        };
        let written = fs::create_dir_all(&shared.dir)
            .and_then(|()| match image {
                Some(bytes) => fs::write(file.with_extension("img"), bytes),
                None => match fs::remove_file(file.with_extension("img")) {
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
                    _ => Ok(()),
                },
            })
            .and_then(|()| fs::write(file.with_extension("json"), serde_json::to_vec(&saved)?));
        if let Err(error) = written {
            tracing::warn!(%error, "saving a link preview failed");
        }
    }
    prune(shared);
    true
}

/// Drop expired previews, and the oldest beyond `MAX_SAVED`.
fn prune(shared: &Shared) {
    let _write = lock(&shared.write);
    let Ok(read) = fs::read_dir(&shared.dir) else {
        return;
    };
    let mut saved: Vec<_> = read
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|e| e == "json"))
        .map(|entry| {
            let at = fs::read(entry.path())
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Saved>(&bytes).ok())
                .map_or(0, |saved| saved.at);
            (at, entry.path())
        })
        .collect();
    saved.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    for (index, (at, path)) in saved.into_iter().enumerate() {
        if index >= MAX_SAVED || now() >= at + KEEP {
            let _ = fs::remove_file(path.with_extension("img"));
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// A site that never touches the network: it counts calls and answers as set.
    struct Fake {
        calls: AtomicUsize,
        /// Picture requests begun: each checks the host first, which is refused,
        /// so none reaches the network.
        pictures: AtomicUsize,
        answer: bool,
        during: Mutex<Option<Box<dyn Fn() + Send>>>,
    }

    impl Provider for Fake {
        fn id(&self) -> &'static str {
            if self.answer { "fake" } else { "down" }
        }
        fn name(&self) -> &'static str {
            "Fake"
        }
        fn key(&self, _: &Url) -> Option<String> {
            None
        }
        fn fetch(&self, _: &Get, key: &str, _: Lang, _: Option<&str>) -> Option<Details> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(during) = lock(&self.during).as_ref() {
                during();
            }
            self.answer.then(|| Details {
                title: format!("Game {key}"),
                description: String::new(),
                fields: Vec::new(),
                url: "https://example.org/".into(),
                image: Some("https://picture.invalid/header.png".into()),
            })
        }
        fn image_host(&self, _: &str) -> bool {
            self.pictures.fetch_add(1, Ordering::SeqCst);
            false
        }
    }

    fn fake(answer: bool) -> &'static Fake {
        Box::leak(Box::new(Fake {
            calls: AtomicUsize::new(0),
            pictures: AtomicUsize::new(0),
            answer,
            during: Mutex::new(None),
        }))
    }

    fn setup() -> (Previews, tempfile::TempDir, Get) {
        let data = tempfile::tempdir().unwrap();
        let previews = Previews::new(&egui::Context::default(), data.path());
        (previews, data, Get::new().unwrap())
    }

    /// A job as `get` would queue it, run here instead of on a worker.
    fn job(previews: &Previews, provider: &'static Fake, key: &str, on_screen: bool) -> Job {
        let cache = cache_key(provider, key);
        if on_screen {
            lock(&previews.shared.seen).insert(cache.clone(), Instant::now());
        }
        let generation = lock(&previews.shared.consent)
            .get(provider.id())
            .map_or(0, |(_, generation)| *generation);
        Job {
            provider,
            key: key.into(),
            cache,
            lang: Lang::En,
            country: None,
            generation,
        }
    }

    fn ready(previews: &Previews, job: &Job) -> bool {
        matches!(
            lock(&previews.shared.slots)
                .get(&job.cache)
                .map(|s| &s.entry),
            Some(Entry::Ready { .. })
        )
    }

    #[test]
    fn only_allowed_links_on_screen_are_fetched() {
        let (previews, _data, get) = setup();
        let site = fake(true);
        run(&previews.shared, &get, &job(&previews, site, "1", true));
        assert_eq!(site.calls.load(Ordering::SeqCst), 0, "not allowed yet");
        previews.allow(site.id(), true);
        run(&previews.shared, &get, &job(&previews, site, "2", false));
        assert_eq!(site.calls.load(Ordering::SeqCst), 0, "off screen");
        let shown = job(&previews, site, "3", true);
        run(&previews.shared, &get, &shown);
        assert_eq!(site.calls.load(Ordering::SeqCst), 1);
        assert!(ready(&previews, &shown));
    }

    #[test]
    fn turning_off_mid_request_keeps_nothing() {
        let (previews, data, get) = setup();
        let site = fake(true);
        previews.allow(site.id(), true);
        let shared = previews.shared.clone();
        *lock(&site.during) = Some(Box::new(move || revoke(&shared, "fake")));
        let job = job(&previews, site, "1", true);
        run(&previews.shared, &get, &job);
        assert_eq!(site.calls.load(Ordering::SeqCst), 1);
        assert!(!ready(&previews, &job));
        assert!(!data.path().join(DIR).exists());
    }

    #[test]
    fn no_picture_request_after_turning_off_mid_request() {
        let (previews, _data, get) = setup();
        let site = fake(true);
        previews.allow(site.id(), true);
        run(&previews.shared, &get, &job(&previews, site, "1", true));
        assert_eq!(
            site.pictures.load(Ordering::SeqCst),
            1,
            "the picture is asked for"
        );
        let shared = previews.shared.clone();
        *lock(&site.during) = Some(Box::new(move || revoke(&shared, "fake")));
        *lock(&previews.shared.next) = Instant::now();
        run(&previews.shared, &get, &job(&previews, site, "2", true));
        assert_eq!(site.pictures.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn off_and_on_again_mid_request_still_keeps_nothing() {
        let (previews, data, get) = setup();
        let site = fake(true);
        previews.allow(site.id(), true);
        let shared = previews.shared.clone();
        *lock(&site.during) = Some(Box::new(move || {
            revoke(&shared, "fake");
            lock(&shared.consent).insert("fake", (true, 3));
        }));
        let job = job(&previews, site, "1", true);
        run(&previews.shared, &get, &job);
        assert_eq!(site.calls.load(Ordering::SeqCst), 1);
        assert!(!ready(&previews, &job), "an older generation's result");
        assert!(!data.path().join(DIR).exists());
    }

    #[test]
    fn turning_off_resets_queued_work_so_on_again_restarts_it() {
        let (previews, _data, get) = setup();
        let site = fake(true);
        previews.allow(site.id(), true);
        let loading = job(&previews, site, "1", true);
        let refreshing = job(&previews, site, "2", true);
        assert!(previews.claim(&loading.cache).1);
        let preview = Arc::new(Preview::new(
            site.fetch(&get, "2", Lang::En, None).unwrap(),
            None,
        ));
        insert(
            &mut lock(&previews.shared.slots),
            refreshing.cache.clone(),
            Entry::Ready {
                preview,
                refresh: u64::MAX,
                kept: u64::MAX,
            },
        );
        lock(&previews.shared.queue).extend([loading, refreshing]);
        previews.allow(site.id(), false);
        assert!(lock(&previews.shared.queue).is_empty());
        previews.allow(site.id(), true);
        for key in ["1", "2"] {
            assert!(
                previews.claim(&cache_key(site, key)).1,
                "{key} starts again"
            );
        }
    }

    #[test]
    fn a_down_site_is_tried_once_for_many_games() {
        let (previews, _data, get) = setup();
        let site = fake(false);
        previews.allow(site.id(), true);
        for key in ["1", "2", "3", "4", "5"] {
            run(&previews.shared, &get, &job(&previews, site, key, true));
        }
        assert_eq!(site.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn sign_out_stops_writes_and_forgets_saved_previews() {
        let (previews, data, get) = setup();
        let site = fake(true);
        previews.allow(site.id(), true);
        let saved = job(&previews, site, "1", true);
        run(&previews.shared, &get, &saved);
        assert!(data.path().join(DIR).exists());
        let shared = previews.shared.clone();
        let after = job(&previews, site, "2", true);
        drop(previews);
        run(&shared, &get, &after);
        assert_eq!(
            site.calls.load(Ordering::SeqCst),
            1,
            "no fetch after sign-out"
        );
        assert!(lock(&shared.slots).get(&after.cache).is_none());
        forget(data.path());
        assert!(!data.path().join(DIR).exists());
    }

    #[test]
    fn memory_and_disk_stay_bounded_and_expire() {
        let (previews, data, get) = setup();
        let site = fake(true);
        previews.allow(site.id(), true);
        for key in 0..MAX_SAVED + 5 {
            run(
                &previews.shared,
                &get,
                &job(&previews, site, &key.to_string(), true),
            );
            // Skip the one-second gap between requests.
            *lock(&previews.shared.next) = Instant::now();
        }
        assert_eq!(lock(&previews.shared.slots).len(), MAX_MEMORY);
        let files = fs::read_dir(data.path().join(DIR)).unwrap().count();
        assert_eq!(files, MAX_SAVED);
        let old = job(&previews, site, "0", false);
        let preview = Arc::new(Preview::new(
            site.fetch(&get, "0", Lang::En, None).unwrap(),
            None,
        ));
        set(
            &previews.shared,
            &old,
            Entry::Ready {
                preview,
                refresh: u64::MAX,
                kept: now() - 1,
            },
        );
        assert!(previews.cached(site, "0").is_none(), "expired");
    }

    #[test]
    fn pictures_are_bounded_before_and_after_decoding() {
        let png = |w, h| {
            let mut bytes = Vec::new();
            image::RgbImage::new(w, h)
                .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
                .unwrap();
            bytes
        };
        // Small compressed, but too big to decode.
        let huge = png(MAX_SIDE + 1, 8);
        assert!((huge.len() as u64) < MAX_IMAGE);
        assert!(picture(&huge).is_none());
        let tall = picture(&png(400, 1600)).unwrap();
        assert!(tall.size[0] <= IMAGE_WIDTH as usize && tall.size[1] <= IMAGE_HEIGHT as usize);
        assert!(picture(b"not a picture").is_none());
    }

    #[test]
    fn picture_addresses_must_be_plain_https_on_an_allowed_host() {
        let host = |h: &str| h.ends_with(".steamstatic.com");
        assert!(safe_url("https://cdn.steamstatic.com/a.jpg", host).is_some());
        assert!(safe_url("https://cdn.steamstatic.com:443/a.jpg", host).is_some());
        for bad in [
            "http://cdn.steamstatic.com/a.jpg",
            "https://user:pass@cdn.steamstatic.com/a.jpg",
            "https://user@cdn.steamstatic.com/a.jpg",
            "https://cdn.steamstatic.com:8443/a.jpg",
            "https://cdn.steamstatic.com.evil.example/a.jpg",
            "https://evil.example/cdn.steamstatic.com/a.jpg",
            "https://evil.example#.steamstatic.com",
        ] {
            assert!(safe_url(bad, host).is_none(), "{bad}");
        }
    }

    #[test]
    fn site_text_becomes_one_short_plain_line() {
        assert_eq!(
            plain("Fight <b>&quot;evil&quot;</b>&nbsp;&amp; win.\n\u{7}", 100),
            "Fight \"evil\" & win."
        );
        assert_eq!(plain("abcdef", 4), "abc…");
        assert_eq!(plain("a\u{202E}b", 10), "ab");
        assert_eq!(plain("<script>x</script>", 10), "x");
        assert_eq!(plain("&#8212; &#x2122; &#39;s &#0; &#xD800;", 50), "— ™ 's");
        assert_eq!(plain("&#x202E;ab", 10), "ab");
    }

    #[test]
    fn only_supported_links_count_and_once_each() {
        let mut links = vec!["https://example.org/a", "https://example.org/b"];
        links.extend(["https://example.org/c", "https://example.org/d"]);
        links.extend([
            "https://store.steampowered.com/app/570/Dota_2/",
            "https://store.steampowered.com/app/570/",
            "https://store.steampowered.com/app/620/",
        ]);
        let found = supported(links.into_iter());
        let keys: Vec<_> = found.iter().map(|(p, k)| (p.id(), k.as_str())).collect();
        assert_eq!(keys, [("steam", "570"), ("steam", "620")]);
        for other in [
            "https://store.steampowered.com.evil.example/app/570/",
            "https://evil.example/store.steampowered.com/app/570/",
            "https://store.steampowered.com/sub/570/",
            "https://store.steampowered.com/app/57x0/",
            "https://store.steampowered.com/app/99999999999/",
            "https://steamcommunity.com/app/570/",
        ] {
            assert!(supported([other].into_iter()).is_empty(), "{other}");
        }
    }
}
