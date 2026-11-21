//! Link previews (#172). The app showing a message fetches details for its links
//! itself, only from sites the person allowed on this device, and only for messages
//! on screen. Neither the sender nor the RustTalk server is involved (see docs/privacy.md).

use std::{
    collections::HashMap,
    fs,
    io::Read,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use eframe::egui::{self, ColorImage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::{i18n::Lang, server::Picture};

pub mod steam;

/// Every site previews can come from. Each needs the person's yes first.
pub static PROVIDERS: &[&dyn Provider] = &[&steam::Steam];

pub const MAX_TITLE: usize = 120;
pub const MAX_DESCRIPTION: usize = 300;
pub const MAX_FIELD: usize = 80;
const MAX_JSON: u64 = 512 * 1024;
const MAX_IMAGE: u64 = 1024 * 1024;
/// Pictures are shown at most this wide, so bigger ones are scaled down once.
const IMAGE_WIDTH: u32 = 460;
const TIMEOUT: Duration = Duration::from_secs(10);
const WORKERS: usize = 2;
/// At most one request starts per this long, across workers.
const GAP: Duration = Duration::from_secs(1);
/// Prices change; anything older is fetched again when shown, while the old copy stays up.
const FRESH: u64 = 60 * 60;
/// Saved previews are dropped after a day.
const KEEP: u64 = 24 * 60 * 60;
const MAX_SAVED: usize = 200;
/// A failed fetch is tried again only after this long, so a down site isn't hammered.
const RETRY: Duration = Duration::from_secs(10 * 60);

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

/// The provider and key for a link, when a site can preview it.
pub fn find(url: &Url) -> Option<(&'static dyn Provider, String)> {
    PROVIDERS
        .iter()
        .find_map(|provider| Some((*provider, provider.key(url)?)))
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
    /// Checked against the provider's picture hosts.
    pub image: Option<String>,
}

pub struct Preview {
    pub details: Details,
    pub image: Option<Picture>,
}

/// Text from a site as one plain line: tags removed, common entities decoded,
/// whitespace collapsed, at most `max` characters.
pub fn plain(text: &str, max: usize) -> String {
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
    for (entity, c) in [
        ("&quot;", "\""),
        ("&#39;", "'"),
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

    /// The body of `url`, when its host passes `host` and it fits in `max` bytes.
    pub fn get(&self, url: &Url, max: u64, host: impl Fn(&str) -> bool) -> Option<Vec<u8>> {
        if url.scheme() != "https" || !url.host_str().is_some_and(host) {
            return None;
        }
        let reply = match self
            .0
            .get(url.clone())
            .send()
            .and_then(|r| r.error_for_status())
        {
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
    /// The preview, and from when (Unix seconds) it may be fetched again.
    Ready(Arc<Preview>, u64),
    Failed(Instant),
}

struct Job {
    provider: &'static dyn Provider,
    key: String,
    cache: String,
    lang: Lang,
    country: Option<String>,
}

struct Shared {
    entries: Mutex<HashMap<String, Entry>>,
    /// Sites allowed right now; a queued fetch for a site turned off is dropped.
    allowed: Mutex<Vec<&'static str>>,
    next: Mutex<Instant>,
    dir: PathBuf,
    ctx: egui::Context,
}

/// Previews fetched and saved on this device.
pub struct Previews {
    shared: Arc<Shared>,
    jobs: Mutex<Option<mpsc::Sender<Job>>>,
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

impl Previews {
    pub fn new(ctx: &egui::Context, data: &std::path::Path) -> Self {
        Self {
            shared: Arc::new(Shared {
                entries: Mutex::default(),
                allowed: Mutex::default(),
                next: Mutex::new(Instant::now()),
                dir: data.join("previews"),
                ctx: ctx.clone(),
            }),
            jobs: Mutex::new(None),
        }
    }

    /// A preview already loaded, without asking for one.
    pub fn cached(&self, provider: &dyn Provider, key: &str) -> Option<Arc<Preview>> {
        match lock(&self.shared.entries).get(&cache_key(provider, key)) {
            Some(Entry::Ready(preview, _)) => Some(preview.clone()),
            _ => None,
        }
    }

    /// The preview for a link on screen, from a site the person allowed. Starts
    /// loading it, or refreshing an old one, in the background.
    pub fn get(&self, provider: &'static dyn Provider, key: &str) -> Option<Arc<Preview>> {
        let cache = cache_key(provider, key);
        let mut entries = lock(&self.shared.entries);
        let (shown, start) = match entries.get(&cache) {
            Some(Entry::Ready(preview, refresh)) => (Some(preview.clone()), now() >= *refresh),
            Some(Entry::Loading) => (None, false),
            Some(Entry::Failed(at)) => (None, at.elapsed() >= RETRY),
            None => (None, true),
        };
        if start {
            if shown.is_none() {
                entries.insert(cache.clone(), Entry::Loading);
            } else if let Some(Entry::Ready(_, refresh)) = entries.get_mut(&cache) {
                // Not again until this refresh is done.
                *refresh = u64::MAX;
            }
            drop(entries);
            self.allow(provider.id(), true);
            let lang = crate::i18n::current();
            let country = country();
            self.send(Job {
                provider,
                key: key.to_owned(),
                cache,
                lang,
                country,
            });
        }
        shown
    }

    /// Record that a site was allowed or turned off.
    pub fn allow(&self, id: &'static str, on: bool) {
        let mut allowed = lock(&self.shared.allowed);
        allowed.retain(|known| *known != id);
        if on {
            allowed.push(id);
        }
    }

    fn send(&self, job: Job) {
        let mut jobs = lock(&self.jobs);
        let sender = jobs.get_or_insert_with(|| {
            let (sender, receiver) = mpsc::channel();
            let receiver = Arc::new(Mutex::new(receiver));
            let get = Get::new().ok().map(Arc::new);
            for index in 0..WORKERS {
                let (shared, receiver, get) = (self.shared.clone(), receiver.clone(), get.clone());
                let spawned = thread::Builder::new()
                    .name(format!("previews-{index}"))
                    .spawn(move || work(&shared, &receiver, get.as_deref()));
                if let Err(error) = spawned {
                    tracing::warn!(%error, "starting link previews failed");
                }
            }
            let shared = self.shared.clone();
            let _ = thread::Builder::new()
                .name("previews-prune".into())
                .spawn(move || prune(&shared.dir));
            sender
        });
        let _ = sender.send(job);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
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

fn work(shared: &Shared, jobs: &Mutex<mpsc::Receiver<Job>>, get: Option<&Get>) {
    loop {
        let Ok(job) = lock(jobs).recv() else {
            return;
        };
        let file = shared
            .dir
            .join(hex::encode(Sha256::digest(job.cache.as_bytes())));
        let saved = load(&file);
        if let Some((preview, at)) = &saved {
            let fresh = now() < at + FRESH;
            let refresh = if fresh { at + FRESH } else { u64::MAX };
            publish(shared, &job.cache, Entry::Ready(preview.clone(), refresh));
            if fresh {
                continue;
            }
        }
        let fetched = get.and_then(|get| fetch(shared, get, &job));
        let entry = match (fetched, saved) {
            (Some((preview, image)), _) => {
                save(&file, &preview.details, image.as_deref());
                Entry::Ready(Arc::new(preview), now() + FRESH)
            }
            // A failed refresh keeps the old copy, without an error.
            (None, Some((preview, _))) => Entry::Ready(preview, now() + RETRY.as_secs()),
            (None, None) => Entry::Failed(Instant::now()),
        };
        publish(shared, &job.cache, entry);
    }
}

fn publish(shared: &Shared, cache: &str, entry: Entry) {
    lock(&shared.entries).insert(cache.to_owned(), entry);
    shared.ctx.request_repaint();
}

/// The details and picture bytes, when the site is still allowed.
fn fetch(shared: &Shared, get: &Get, job: &Job) -> Option<(Preview, Option<Vec<u8>>)> {
    let wait = || {
        let allowed = lock(&shared.allowed).contains(&job.provider.id());
        if allowed {
            let mut next = lock(&shared.next);
            let start = (*next).max(Instant::now());
            *next = start + GAP;
            drop(next);
            thread::sleep(start.saturating_duration_since(Instant::now()));
        }
        allowed
    };
    if !wait() {
        return None;
    }
    let details = job
        .provider
        .fetch(get, &job.key, job.lang, job.country.as_deref())?;
    let bytes = match details.image.as_deref().and_then(|u| Url::parse(u).ok()) {
        Some(url) if wait() => get.get(&url, MAX_IMAGE, |host| job.provider.image_host(host)),
        _ => None,
    };
    let image = bytes.as_deref().and_then(picture);
    let bytes = image.is_some().then_some(bytes).flatten();
    Some((Preview { details, image }, bytes))
}

fn picture(bytes: &[u8]) -> Option<Picture> {
    let image = image::load_from_memory(bytes).ok()?;
    let image = if image.width() > IMAGE_WIDTH {
        image.resize(IMAGE_WIDTH, u32::MAX, image::imageops::FilterType::Triangle)
    } else {
        image
    };
    let rgba = image.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Some(Picture {
        key: Sha256::digest(bytes).into(),
        frames: Arc::new(vec![(
            ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()),
            0,
        )]),
    })
}

fn load(file: &std::path::Path) -> Option<(Arc<Preview>, u64)> {
    let saved: Saved = serde_json::from_slice(&fs::read(file.with_extension("json")).ok()?).ok()?;
    if now() >= saved.at + KEEP {
        return None;
    }
    let image = fs::read(file.with_extension("img"))
        .ok()
        .and_then(|bytes| picture(&bytes));
    Some((
        Arc::new(Preview {
            details: saved.details,
            image,
        }),
        saved.at,
    ))
}

fn save(file: &std::path::Path, details: &Details, image: Option<&[u8]>) {
    let saved = Saved {
        at: now(),
        details: details.clone(),
    };
    let written = fs::create_dir_all(file.parent().unwrap_or(file))
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

/// Drop expired previews, and the oldest beyond `MAX_SAVED`.
fn prune(dir: &std::path::Path) {
    let Ok(read) = fs::read_dir(dir) else {
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
    use super::*;

    #[test]
    fn site_text_becomes_one_short_plain_line() {
        assert_eq!(
            plain("Fight <b>&quot;evil&quot;</b>&nbsp;&amp; win.\n\u{7}", 100),
            "Fight \"evil\" & win."
        );
        assert_eq!(plain("abcdef", 4), "abc…");
        assert_eq!(plain("a\u{202E}b", 10), "ab");
        assert_eq!(plain("<script>x</script>", 10), "x");
    }

    #[test]
    fn only_supported_links_find_a_provider() {
        let url = |s: &str| Url::parse(s).unwrap();
        let (provider, key) = find(&url("https://store.steampowered.com/app/570/Dota_2/")).unwrap();
        assert_eq!((provider.id(), key.as_str()), ("steam", "570"));
        for other in [
            "https://store.steampowered.com.evil.example/app/570/",
            "https://evil.example/store.steampowered.com/app/570/",
            "https://store.steampowered.com/sub/570/",
            "https://store.steampowered.com/app/57x0/",
            "https://store.steampowered.com/app/99999999999/",
            "https://steamcommunity.com/app/570/",
        ] {
            assert!(find(&url(other)).is_none(), "{other}");
        }
    }
}
