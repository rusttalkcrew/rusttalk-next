//! GIF search through KLIPY, off until the person turns it on (see docs/privacy.md).
//! Requests go straight from this device to KLIPY without account, device or server
//! identifiers. A picked GIF is downloaded here and sent as a normal encrypted file,
//! so the people who receive it never contact KLIPY.

use std::{
    collections::HashMap,
    io::Read as _,
    path::PathBuf,
    sync::{Mutex, mpsc},
    thread,
    time::Duration,
};

use anyhow::{Context as _, Result, ensure};
use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions, Ui};
use serde_json::Value;

use crate::{home::frame_at, net::MAX_FILE, server::Id};

/// KLIPY app key, set when the app is built. Keys are per app, so RustTalk ships none
/// in its source; without one the GIF tab says search isn't set up.
pub const KEY: Option<&str> = option_env!("RUSTTALK_KLIPY_KEY");
const API: &str = "https://api.klipy.com/api/v1";
/// The service, as the opt-in screen and privacy.md name it.
pub const SERVICE: &str = "KLIPY";
const PER_PAGE: usize = 24;
/// Wait this long after the last keystroke before searching (seconds).
const DEBOUNCE: f64 = 0.4;
/// At most one search or trending request per this many seconds.
const GAP: f64 = 1.0;
/// Results are asked again after this long (seconds).
const FRESH: f64 = 300.0;
/// Previews downloading at once, and kept before the cache is emptied.
const MAX_LOADING: usize = 6;
const MAX_PREVIEWS: usize = 96;
const PREVIEW_BYTES: u64 = 4 * 1024 * 1024;
const PREVIEW_BUDGET: usize = 16 * 1024 * 1024;
const LIST_BYTES: u64 = 2 * 1024 * 1024;
/// A generic agent, the same for every RustTalk user and version.
const USER_AGENT: &str = "Mozilla/5.0";

/// Tests answer requests here instead of the network: URL in, body out.
pub static TEST_FETCH: Mutex<Option<Fetcher>> = Mutex::new(None);
pub type Fetcher = fn(&str) -> Result<Vec<u8>>;

/// One result: what to show in the grid, and what to send.
#[derive(Clone, Debug, PartialEq)]
pub struct Gif {
    pub title: String,
    pub preview: String,
    pub file: String,
    pub size: u64,
}

enum Listing {
    Loading,
    Ready(Vec<Gif>),
    Failed,
}

enum Preview {
    Loading,
    Ready(Vec<TextureHandle>, Vec<u32>),
    Failed,
}

enum Msg {
    Listing(String, Result<Vec<Gif>>),
    Preview(String, Result<Vec<(ColorImage, u32)>>),
    /// A GIF to send in a channel: its downloaded file, or why not.
    Saved(Id, Result<PathBuf>),
}

/// What the GIF tab shows now.
pub enum Shown<'a> {
    Loading,
    Ready(&'a [Gif]),
    Failed,
}

pub struct Gifs {
    /// The person's choice, copied from the settings every frame.
    pub enabled: bool,
    /// The opt-in button was pressed; the caller saves it to the settings.
    pub turn_on: bool,
    /// Results by search words (empty for trending) and when they were asked.
    lists: HashMap<String, (f64, Listing)>,
    previews: HashMap<String, Preview>,
    /// When the search words last changed, and the words.
    typed: (f64, String),
    last_request: f64,
    /// A GIF being downloaded to send, by channel.
    pub sending: Option<Id>,
    ready: Vec<(Id, PathBuf)>,
    pub problem: bool,
    inbox: (mpsc::Sender<Msg>, mpsc::Receiver<Msg>),
}

impl Default for Gifs {
    fn default() -> Self {
        Self {
            enabled: false,
            turn_on: false,
            lists: HashMap::new(),
            previews: HashMap::new(),
            typed: (f64::NEG_INFINITY, String::new()),
            last_request: f64::NEG_INFINITY,
            sending: None,
            ready: Vec::new(),
            problem: false,
            inbox: mpsc::channel(),
        }
    }
}

impl Gifs {
    /// Take finished requests. Off, nothing was ever asked, so nothing arrives.
    fn receive(&mut self, ctx: &egui::Context) {
        for msg in self.inbox.1.try_iter() {
            match msg {
                Msg::Listing(query, result) => {
                    let listing = match result {
                        Ok(gifs) => Listing::Ready(gifs),
                        Err(error) => {
                            tracing::warn!(error = %format_args!("{error:#}"), "GIF search failed");
                            Listing::Failed
                        }
                    };
                    if let Some(entry) = self.lists.get_mut(&query) {
                        entry.1 = listing;
                    }
                }
                Msg::Preview(url, result) => {
                    let preview = match result {
                        Ok(frames) => {
                            let delays = frames.iter().map(|(_, ms)| *ms).collect();
                            let textures = (frames.into_iter())
                                .map(|(image, _)| {
                                    ctx.load_texture("gif", image, TextureOptions::LINEAR)
                                })
                                .collect();
                            Preview::Ready(textures, delays)
                        }
                        Err(error) => {
                            tracing::warn!(error = %format_args!("{error:#}"), "GIF preview failed");
                            Preview::Failed
                        }
                    };
                    if let Some(slot) = self.previews.get_mut(&url) {
                        *slot = preview;
                    }
                }
                Msg::Saved(channel, result) => {
                    self.sending = None;
                    match result {
                        Ok(path) => self.ready.push((channel, path)),
                        Err(error) => {
                            tracing::warn!(error = %format_args!("{error:#}"), "downloading a GIF failed");
                            self.problem = true;
                        }
                    }
                }
            }
        }
    }

    /// The results for `query`: trending when empty. Asks once the typing has paused,
    /// at most once a `GAP`, and only while search is on and the app has a key.
    pub fn results(&mut self, ui: &Ui, query: &str) -> Shown<'_> {
        self.receive(ui.ctx());
        let now = ui.input(|input| input.time);
        let query = query.trim().to_lowercase();
        if query != self.typed.1 {
            self.typed = (now, query.clone());
        }
        let stale = self
            .lists
            .get(&query)
            .is_none_or(|(at, listing)| !matches!(listing, Listing::Loading) && now - at > FRESH);
        if self.enabled
            && stale
            && let Some(key) = KEY
        {
            let waited = now - self.typed.0 >= DEBOUNCE || query.is_empty();
            if waited && now - self.last_request >= GAP {
                self.last_request = now;
                self.lists.insert(query.clone(), (now, Listing::Loading));
                let (inbox, ctx, url) = (
                    self.inbox.0.clone(),
                    ui.ctx().clone(),
                    list_url(key, &query),
                );
                thread::spawn(move || {
                    let result = fetch(&url, LIST_BYTES).and_then(|body| parse(&body));
                    let _ = inbox.send(Msg::Listing(query, result));
                    ctx.request_repaint();
                });
            } else {
                // Come back when the wait is over.
                ui.ctx().request_repaint_after(Duration::from_millis(150));
            }
        }
        match self.lists.get(&self.typed.1).or_else(|| self.lists.get("")) {
            Some((_, Listing::Ready(gifs))) => Shown::Ready(gifs),
            Some((_, Listing::Failed)) => Shown::Failed,
            _ => Shown::Loading,
        }
    }

    /// The preview's frame to show now, playing; starts downloading it if there is room.
    pub fn preview(&mut self, ui: &Ui, url: &str) -> Option<TextureHandle> {
        if !self.previews.contains_key(url) {
            let loading = (self.previews.values())
                .filter(|p| matches!(p, Preview::Loading))
                .count();
            if !self.enabled || loading >= MAX_LOADING {
                return None;
            }
            if self.previews.len() >= MAX_PREVIEWS {
                self.previews.retain(|_, p| matches!(p, Preview::Loading));
            }
            self.previews.insert(url.to_owned(), Preview::Loading);
            let (inbox, ctx, url) = (self.inbox.0.clone(), ui.ctx().clone(), url.to_owned());
            thread::spawn(move || {
                let frames = fetch(&url, PREVIEW_BYTES).and_then(|bytes| {
                    let frames =
                        rusttalk_core::client::decode_frames(&bytes, Some(240), PREVIEW_BUDGET)?;
                    Ok(frames.into_iter().map(crate::net::frame_image).collect())
                });
                let _ = inbox.send(Msg::Preview(url, frames));
                ctx.request_repaint();
            });
        }
        match self.previews.get(url)? {
            Preview::Ready(textures, delays) => {
                let now = (ui.input(|input| input.time) * 1000.0) as u64;
                let at = match frame_at(delays.iter().copied(), now) {
                    Some((at, left)) if textures.len() > 1 => {
                        ui.ctx().request_repaint_after(Duration::from_millis(left));
                        at
                    }
                    _ => 0,
                };
                Some(textures[at].clone())
            }
            _ => None,
        }
    }

    /// Download `gif` to a temporary file, to send in `channel` once it is there.
    pub fn send(&mut self, ctx: &egui::Context, channel: Id, gif: &Gif) {
        if !self.enabled || self.sending.is_some() {
            return;
        }
        self.sending = Some(channel);
        self.problem = false;
        let (inbox, ctx, url) = (self.inbox.0.clone(), ctx.clone(), gif.file.clone());
        let name = file_name(&gif.title);
        thread::spawn(move || {
            let saved = fetch(&url, MAX_FILE).and_then(|bytes| {
                let id = rusttalk_core::keys::random_key()?;
                let folder = crate::files::temp_folder(&id[..8]);
                std::fs::create_dir(&folder)?;
                let path = folder.join(name);
                std::fs::write(&path, bytes)?;
                Ok(path)
            });
            let _ = inbox.send(Msg::Saved(channel, saved));
            ctx.request_repaint();
        });
    }

    /// A downloaded GIF to send now in `channel`.
    pub fn take_ready(&mut self, ctx: &egui::Context, channel: Id) -> Option<PathBuf> {
        self.receive(ctx);
        let at = self.ready.iter().position(|(id, _)| *id == channel)?;
        Some(self.ready.remove(at).1)
    }
}

fn list_url(key: &str, query: &str) -> String {
    // No customer ID or locale: nothing that tells people apart.
    let base = format!("{API}/{key}/gifs");
    let common = format!("per_page={PER_PAGE}&content_filter=medium&format_filter=gif,webp");
    if query.is_empty() {
        format!("{base}/trending?{common}")
    } else {
        let mut url = url::Url::parse(&format!("{base}/search?{common}")).expect("fixed URL");
        url.query_pairs_mut().append_pair("q", query);
        url.into()
    }
}

/// A file name from the title: letters, digits and dashes, ending in `.gif`.
fn file_name(title: &str) -> String {
    let words: Vec<_> = title
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(6)
        .collect();
    let stem = words.join("-").to_lowercase();
    if stem.is_empty() {
        "gif.gif".into()
    } else {
        format!("{stem}.gif")
    }
}

/// GET over HTTPS from KLIPY only, with a generic agent and no Referer, up to `limit` bytes.
fn fetch(url: &str, limit: u64) -> Result<Vec<u8>> {
    let parsed = url::Url::parse(url)?;
    let host = parsed.host_str().unwrap_or_default();
    ensure!(
        parsed.scheme() == "https" && (host == "klipy.com" || host.ends_with(".klipy.com")),
        "not a KLIPY address"
    );
    let test = *TEST_FETCH.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(answer) = test {
        return answer(url);
    }
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .referer(false)
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::limited(3))
        .timeout(Duration::from_secs(20))
        .build()?;
    let reply = client.get(parsed).send()?.error_for_status()?;
    let mut bytes = Vec::new();
    // One byte over the limit tells a too-big answer apart.
    reply.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "answer too big");
    Ok(bytes)
}

/// The GIFs in a KLIPY answer: a small preview (WebP if offered) and a GIF to send.
fn parse(body: &[u8]) -> Result<Vec<Gif>> {
    let value: Value = serde_json::from_slice(body)?;
    let items = value["data"]["data"]
        .as_array()
        .context("no results in answer")?;
    let file = |item: &Value, sizes: &[&str], formats: &[&str]| {
        sizes.iter().find_map(|size| {
            formats.iter().find_map(|format| {
                let file = &item["file"][size][format];
                Some((
                    file["url"].as_str()?.to_owned(),
                    file["size"].as_u64().unwrap_or(0),
                ))
            })
        })
    };
    Ok(items
        .iter()
        .filter_map(|item| {
            let (preview, _) = file(item, &["sm", "xs", "md"], &["webp", "gif"])?;
            let (file, size) = file(item, &["md", "hd", "sm"], &["gif"])?;
            (size <= MAX_FILE).then(|| Gif {
                title: item["title"].as_str().unwrap_or_default().to_owned(),
                preview,
                file,
                size,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANSWER: &str = r#"{"result":true,"data":{"data":[
        {"id":1,"title":"Happy Dance!","file":{
            "md":{"gif":{"url":"https://static.klipy.com/a/md.gif","size":1200}},
            "sm":{"gif":{"url":"https://static.klipy.com/a/sm.gif","size":300},
                  "webp":{"url":"https://static.klipy.com/a/sm.webp","size":200}}}},
        {"id":2,"title":"no gif","file":{"sm":{"webp":{"url":"https://static.klipy.com/b.webp"}}}}
    ],"has_next":true}}"#;

    #[test]
    fn answers_give_a_webp_preview_and_a_gif_to_send() {
        let gifs = parse(ANSWER.as_bytes()).unwrap();
        assert_eq!(
            gifs,
            [Gif {
                title: "Happy Dance!".into(),
                preview: "https://static.klipy.com/a/sm.webp".into(),
                file: "https://static.klipy.com/a/md.gif".into(),
                size: 1200,
            }]
        );
        assert!(parse(b"{}").is_err());
    }

    #[test]
    fn requests_carry_no_identifiers() {
        let search = list_url("KEY", "cat & dog");
        assert!(search.starts_with("https://api.klipy.com/api/v1/KEY/gifs/search?"));
        assert!(search.ends_with("&q=cat+%26+dog"));
        let trending = list_url("KEY", "");
        assert!(trending.contains("/gifs/trending?"));
        for url in [search, trending] {
            assert!(!url.contains("customer_id") && !url.contains("locale"));
        }
    }

    #[test]
    fn only_klipy_over_https_is_fetched() {
        assert!(fetch("http://static.klipy.com/a.gif", 10).is_err());
        assert!(fetch("https://example.com/a.gif", 10).is_err());
        assert!(fetch("https://klipy.com.example.com/a.gif", 10).is_err());
    }

    #[test]
    fn file_names_come_from_the_title() {
        assert_eq!(file_name("Happy Dance!"), "happy-dance.gif");
        assert_eq!(file_name("  "), "gif.gif");
    }

    #[test]
    fn off_means_no_requests() {
        let ctx = egui::Context::default();
        let mut gifs = Gifs::default();
        let mut asked = false;
        let mut output = ctx.run_ui(Default::default(), |ui| {
            let _ = gifs.results(ui, "cats");
            asked = gifs.preview(ui, "https://static.klipy.com/a.gif").is_some()
                || !gifs.lists.is_empty()
                || !gifs.previews.is_empty();
        });
        output.textures_delta.clear();
        gifs.send(&ctx, [1; 32], &parse(ANSWER.as_bytes()).unwrap()[0]);
        assert!(!asked && gifs.sending.is_none());
    }
}
