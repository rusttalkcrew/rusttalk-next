//! Previews of links to any other website (#185). Unlike the sites in `previews`,
//! where each reader's app asks the site, these are made once by the SENDER's app
//! while sending and travel inside the encrypted message as a small hidden file
//! (`MEDIA_TYPE`) whose thumbnail is the page's picture. Apps that know the type
//! draw a card and hide the file; older apps show a small extra file.
//! See docs/privacy.md and docs/protocol.md.

use std::{
    cell::{Cell, RefCell},
    io::Read,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use html5ever::{
    tendril::StrTendril,
    tokenizer::{
        BufferQueue, StartTag, Tag, TagToken, Token, TokenSink, TokenSinkResult, Tokenizer,
        TokenizerOpts, states::RawKind,
    },
};
use serde::{Deserialize, Serialize};
use url::{Host, Url};

use crate::previews::{self, plain};

pub const MEDIA_TYPE: &str = "application/vnd.rusttalk.link-preview+json";
/// The name older apps show for the file.
pub const FILE_NAME: &str = "link-preview.json";
/// The file is a few hundred bytes; anything bigger isn't a preview.
pub const MAX_CARD_BYTES: u32 = 4096;
const MAX_URL: usize = 2048;
const MAX_TITLE: usize = 200;
const MAX_DESCRIPTION: usize = 300;
const MAX_SITE: usize = 80;
const MAX_HTML: u64 = 512 * 1024;
const MAX_IMAGE: u64 = 4 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REDIRECTS: usize = 3;

/// The preview file's contents.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Card {
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub site: String,
}

impl Card {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// A received preview, checked: the sender's app is trusted no more than its text.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_CARD_BYTES as usize {
            return None;
        }
        let card: Self = serde_json::from_slice(bytes).ok()?;
        let url = web_url(&card.url)?;
        let title = plain(&card.title, MAX_TITLE);
        (!title.is_empty()).then(|| Self {
            url: url.into(),
            title,
            description: plain(&card.description, MAX_DESCRIPTION),
            site: plain(&card.site, MAX_SITE),
        })
    }
}

impl Card {
    /// Shortened until its file fits `MAX_CARD_BYTES`: the description first, then the
    /// title, then the site. `None` when even that isn't enough.
    pub fn fit(mut self) -> Option<Self> {
        let fields: [fn(&mut Self) -> &mut String; 3] =
            [|c| &mut c.description, |c| &mut c.title, |c| &mut c.site];
        for field in fields {
            while self.encode().len() > MAX_CARD_BYTES as usize && field(&mut self).pop().is_some()
            {
            }
        }
        (self.encode().len() <= MAX_CARD_BYTES as usize && !self.title.trim().is_empty())
            .then_some(self)
    }
}

/// Whether a received card is about one of the links in its message, so a sender can't
/// attach a card for some other page. Both sides are compared as parsed addresses.
pub fn matches<'a>(card: &Card, links: impl IntoIterator<Item = &'a str>) -> bool {
    let Some(url) = web_url(&card.url) else {
        return false;
    };
    links
        .into_iter()
        .filter_map(web_url)
        .any(|link| link == url)
}

/// An `http` or `https` address with a host and no user name or password.
pub fn web_url(text: &str) -> Option<Url> {
    let url = Url::parse(text).ok().filter(|_| text.len() <= MAX_URL)?;
    (matches!(url.scheme(), "http" | "https")
        && url.has_host()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

/// The one link in a message to make a preview for: the first web link no reader-side
/// site (`previews`) covers.
pub fn candidate<'a>(links: impl Iterator<Item = &'a str>) -> Option<Url> {
    links
        .filter(|link| previews::supported(std::iter::once(*link)).is_empty())
        .find_map(web_url)
}

/// Making a preview with a fresh `Fetcher` on its own thread, while the message waits.
/// Dropping it (removed, timed out, turned off, signed out) stops the fetch.
pub struct Job {
    done: std::sync::mpsc::Receiver<Option<Made>>,
    deadline: Instant,
    cancel: Arc<AtomicBool>,
}

impl Job {
    pub fn start(url: Url, ctx: &eframe::egui::Context) -> Self {
        let (sender, done) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let deadline = started + GIVE_UP;
        let stop = Stop {
            cancel: cancel.clone(),
            deadline: started + TIMEOUT,
        };
        let spawned = std::thread::Builder::new()
            .name("link-preview".into())
            .spawn(move || {
                let made = Fetcher::new()
                    .make(&url, &stop)
                    .map(|(card, picture)| Made {
                        card,
                        picture,
                        deadline,
                    });
                let _ = sender.send(made);
                ctx.request_repaint();
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "starting a link preview failed");
        }
        Self {
            done,
            deadline,
            cancel,
        }
    }

    /// `None` while still working; then the preview, or `None` inside when there is none
    /// (also when name lookup alone took too long).
    pub fn poll(&self) -> Option<Option<Made>> {
        match self.done.try_recv() {
            Ok(made) => Some(made),
            Err(std::sync::mpsc::TryRecvError::Empty) if Instant::now() < self.deadline => None,
            Err(_) => Some(None),
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

/// The whole preview, upload included, has this long; then the message goes without.
const GIVE_UP: Duration = Duration::from_secs(8);

/// A finished preview: the card, the page's picture as downloaded, and when uploading it
/// must have finished.
pub struct Made {
    pub card: Card,
    pub picture: Option<Vec<u8>>,
    pub deadline: Instant,
}

/// When a fetch must stop: cancelled, or out of time.
pub struct Stop {
    pub cancel: Arc<AtomicBool>,
    pub deadline: Instant,
}

impl Stop {
    /// The time left for the next step, or `None` to stop now.
    fn left(&self) -> Option<Duration> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        (!self.cancel.load(Ordering::SeqCst) && !left.is_zero()).then_some(left)
    }
}

/// Fetches pages for previews, only from public addresses.
#[derive(Default)]
pub struct Fetcher {
    /// Tests serve pages on 127.0.0.1; nothing else may turn the guard off.
    allow_local: bool,
}

impl Fetcher {
    pub fn new() -> Self {
        Self { allow_local: false }
    }

    #[cfg(test)]
    fn local() -> Self {
        Self { allow_local: true }
    }

    /// The card and picture for `url`, or `None` when the page can't give one in time.
    pub fn make(&self, url: &Url, stop: &Stop) -> Option<(Card, Option<Vec<u8>>)> {
        let (page, html) = self.get(url, MAX_HTML, stop, true)?;
        let meta = parse(&String::from_utf8_lossy(&html));
        let title = plain(&meta.title?, MAX_TITLE);
        if title.is_empty() {
            return None;
        }
        let picture = (meta.image.as_deref())
            .and_then(|src| page.join(src).ok())
            .and_then(|src| web_url(src.as_str()))
            .and_then(|src| self.get(&src, MAX_IMAGE, stop, false))
            .map(|(_, bytes)| bytes);
        let card = Card {
            url: url.as_str().into(),
            title,
            description: plain(&meta.description.unwrap_or_default(), MAX_DESCRIPTION),
            site: plain(&meta.site.unwrap_or_default(), MAX_SITE),
        };
        Some((card.fit()?, picture))
    }

    /// Follow up to `MAX_REDIRECTS` redirects, checking every address, and read at most
    /// `max` bytes (a page may be cut there, a picture may not be bigger).
    fn get(&self, url: &Url, max: u64, stop: &Stop, page: bool) -> Option<(Url, Vec<u8>)> {
        let mut url = url.clone();
        for _ in 0..=MAX_REDIRECTS {
            let left = stop.left()?;
            let resolution =
                rusttalk_core::route::resolve_for_link_card(&url, self.allow_local, public, left)
                    .ok()?;
            let addresses = match resolution {
                rusttalk_core::route::LinkCardResolution::Direct => Some(self.resolve(&url)?),
                rusttalk_core::route::LinkCardResolution::RemoteDns => None,
                rusttalk_core::route::LinkCardResolution::Resolved(addrs) => Some(addrs),
            };
            // Name lookup can take long; check again, and give the request only what's left.
            let left = stop.left()?;
            let builder = reqwest::blocking::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .referer(false)
                .user_agent("RustTalk")
                .timeout(left);
            // Connect to exactly the checked addresses when resolved locally or in-tunnel,
            // using a strict DNS resolver override (and `socks5://` for WireGuard) so a
            // second DNS lookup can never rebind to a private IP while keeping original
            // Host header and TLS SNI intact.
            let client = match (addresses, url.host()) {
                (Some(addresses), Some(Host::Domain(domain))) => {
                    crate::route::configure_blocking_reqwest_resolved(builder, domain, addresses)
                        .ok()?
                }
                _ => crate::route::configure_blocking_reqwest(builder).ok()?,
            };
            let reply = client.build().ok()?.get(url.clone()).send().ok()?;
            if reply.status().is_redirection() {
                let next = reply
                    .headers()
                    .get(reqwest::header::LOCATION)?
                    .to_str()
                    .ok()?;
                url = web_url(url.join(next).ok()?.as_str())?;
                continue;
            }
            if !reply.status().is_success() {
                return None;
            }
            let kind = (reply.headers().get(reqwest::header::CONTENT_TYPE))
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if page && !kind.contains("html") {
                return None;
            }
            let mut bytes = Vec::new();
            let limit = if page { max } else { max + 1 };
            reply.take(limit).read_to_end(&mut bytes).ok()?;
            if bytes.len() as u64 > max {
                return None;
            }
            return Some((url, bytes));
        }
        None
    }

    /// The host's addresses, when all of them are public.
    fn resolve(&self, url: &Url) -> Option<Vec<SocketAddr>> {
        let port = url.port_or_known_default()?;
        let addresses: Vec<SocketAddr> = match url.host()? {
            Host::Ipv4(ip) => vec![SocketAddr::new(ip.into(), port)],
            Host::Ipv6(ip) => vec![SocketAddr::new(ip.into(), port)],
            Host::Domain(domain) => (domain, port).to_socket_addrs().ok()?.collect(),
        };
        let ok = !addresses.is_empty()
            && (self.allow_local || addresses.iter().all(|address| public(address.ip())));
        ok.then_some(addresses)
    }
}

/// Whether an address is on the public internet: not private, loopback, link-local,
/// multicast, shared, reserved or for documentation.
pub fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_multicast()
                || ip.is_broadcast()
                || ip.is_unspecified()
                || ip.is_documentation()
                || a == 0
                || a >= 240
                || (a == 100 && b & 0xc0 == 64)
                || (a == 198 && b & 0xfe == 18)
                || (a == 192 && b == 0 && c == 0))
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public(v4.into());
            }
            // Only global unicast (2000::/3), without its special-use blocks: all of
            // 2001::/23 (Teredo, benchmarking, ORCHID and others), documentation
            // (2001:db8::/32), 6to4 (2002::/16) and 3fff::/20 (documentation too). Everything outside
            // 2000::/3 (loopback, ULA, link-local, multicast, NAT64, discard) is refused.
            let [first, second, ..] = ip.segments();
            first & 0xe000 == 0x2000
                && !(first == 0x2001 && second < 0x0200)
                && !(first == 0x2001 && second == 0x0db8)
                && first != 0x2002
                && !(first == 0x3fff && second < 0x1000)
        }
    }
}

/// What a page says about itself.
#[derive(Debug, Default, PartialEq)]
struct Meta {
    title: Option<String>,
    description: Option<String>,
    image: Option<String>,
    site: Option<String>,
}

#[derive(Default)]
struct Sink {
    og: RefCell<Meta>,
    twitter: RefCell<Meta>,
    plain: RefCell<Meta>,
    in_title: Cell<bool>,
    title: RefCell<String>,
}

fn attribute<'a>(tag: &'a Tag, name: &str) -> Option<&'a StrTendril> {
    tag.attrs
        .iter()
        .find(|attribute| &*attribute.name.local == name)
        .map(|attribute| &attribute.value)
}

impl TokenSink for Sink {
    type Handle = ();

    fn process_token(&self, token: Token, _: u64) -> TokenSinkResult<()> {
        match token {
            TagToken(tag) if tag.kind == StartTag => {
                match &*tag.name {
                    "meta" => self.meta(&tag),
                    "title" if self.title.borrow().is_empty() => {
                        self.in_title.set(true);
                        return TokenSinkResult::RawData(RawKind::Rcdata);
                    }
                    "title" => return TokenSinkResult::RawData(RawKind::Rcdata),
                    // Their contents are not markup; reading them as tags could find
                    // a fake `<meta>`.
                    "script" => return TokenSinkResult::RawData(RawKind::ScriptData),
                    "style" | "noscript" | "textarea" => {
                        return TokenSinkResult::RawData(RawKind::Rawtext);
                    }
                    _ => {}
                }
            }
            TagToken(_) => self.in_title.set(false),
            Token::CharacterTokens(text) if self.in_title.get() => {
                let mut title = self.title.borrow_mut();
                if title.len() < 4 * MAX_TITLE {
                    title.push_str(&text);
                }
            }
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

impl Sink {
    fn meta(&self, tag: &Tag) {
        let Some(content) = attribute(tag, "content") else {
            return;
        };
        let key = attribute(tag, "property")
            .or_else(|| attribute(tag, "name"))
            .map(|key| key.to_ascii_lowercase())
            .unwrap_or_default();
        let (meta, field) = match key.as_str() {
            "og:title" => (&self.og, 0),
            "og:description" => (&self.og, 1),
            "og:image" | "og:image:url" | "og:image:secure_url" => (&self.og, 2),
            "og:site_name" => (&self.og, 3),
            "twitter:title" => (&self.twitter, 0),
            "twitter:description" => (&self.twitter, 1),
            "twitter:image" | "twitter:image:src" => (&self.twitter, 2),
            "description" => (&self.plain, 1),
            _ => return,
        };
        let mut meta = meta.borrow_mut();
        let slot = match field {
            0 => &mut meta.title,
            1 => &mut meta.description,
            2 => &mut meta.image,
            _ => &mut meta.site,
        };
        // The first of each wins, as for crawlers.
        if slot.is_none() && content.len() <= 4 * MAX_URL {
            *slot = Some(content.to_string());
        }
    }
}

/// Open Graph first, then Twitter cards, then `<title>` and the meta description.
fn parse(html: &str) -> Meta {
    let tokenizer = Tokenizer::new(Sink::default(), TokenizerOpts::default());
    let input = BufferQueue::default();
    input.push_back(StrTendril::from(html));
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    let sink = tokenizer.sink;
    let (og, twitter, mut fallback) = (
        sink.og.into_inner(),
        sink.twitter.into_inner(),
        sink.plain.into_inner(),
    );
    let title = sink.title.into_inner();
    fallback.title = (!title.trim().is_empty()).then_some(title);
    let pick = |f: fn(&Meta) -> &Option<String>| {
        [&og, &twitter, &fallback]
            .into_iter()
            .find_map(|meta| f(meta).clone().filter(|v| !v.trim().is_empty()))
    };
    Meta {
        title: pick(|m| &m.title),
        description: pick(|m| &m.description),
        image: pick(|m| &m.image),
        site: og.site.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        thread,
    };

    use super::*;

    fn stop() -> Stop {
        Stop {
            cancel: Arc::new(AtomicBool::new(false)),
            deadline: Instant::now() + TIMEOUT,
        }
    }

    #[test]
    fn open_graph_beats_twitter_beats_the_title() {
        let meta = parse(
            r#"<html><head><title>Plain &amp; title</title>
            <meta name="description" content="Plain description">
            <meta name="twitter:title" content="Twitter title">
            <meta property="og:title" content="OG title">
            <meta property="og:image" content="/pic.png">
            <meta property="og:site_name" content="Example">
            <script>document.write('<meta property="og:description" content="fake">')</script>
            </head><body>x</body></html>"#,
        );
        assert_eq!(meta.title.as_deref(), Some("OG title"));
        assert_eq!(meta.description.as_deref(), Some("Plain description"));
        assert_eq!(meta.image.as_deref(), Some("/pic.png"));
        assert_eq!(meta.site.as_deref(), Some("Example"));
        let meta = parse("<title>Only &amp; a title</title><title>second</title>");
        assert_eq!(meta.title.as_deref(), Some("Only & a title"));
        assert_eq!(parse("<p>nothing</p>"), Meta::default());
    }

    #[test]
    fn received_cards_are_checked() {
        let card = Card {
            url: "https://example.org/a".into(),
            title: "A <b>title</b>".into(),
            description: "Text\u{202E}".into(),
            site: String::new(),
        };
        let decoded = Card::decode(&card.encode()).unwrap();
        assert_eq!(decoded.title, "A title");
        assert_eq!(decoded.description, "Text");
        for url in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "https://u:p@example.org/",
        ] {
            let bad = Card {
                url: url.into(),
                ..card.clone()
            };
            assert!(Card::decode(&bad.encode()).is_none(), "{url}");
        }
        let empty = Card {
            title: " ".into(),
            ..card.clone()
        };
        assert!(Card::decode(&empty.encode()).is_none());
        let big = Card {
            description: "x".repeat(5000),
            ..card
        };
        assert!(Card::decode(&big.encode()).is_none());
    }

    #[test]
    fn only_links_no_reader_side_site_covers() {
        let links = [
            "https://store.steampowered.com/app/570/",
            "https://store.steampowered.com/app/620/",
            "https://news.example.org/story",
            "https://other.example.org/",
        ];
        assert_eq!(
            candidate(links.into_iter()).map(String::from).as_deref(),
            Some("https://news.example.org/story")
        );
        assert!(candidate(["https://store.steampowered.com/app/620/"].into_iter()).is_none());
    }

    #[test]
    fn private_and_special_addresses_are_refused() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b:1::1",
            "2002:7f00:1::",
            "100::1",
            "2001::1",
            "2001:2::1",
            "2001:10::1",
            "2001:1ff::1",
            "2001:db8::1",
            "3fff::1",
            "4000::1",
            "ff02::1",
            "fec0::1",
            "::",
            "::2",
        ] {
            assert!(!public(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "93.184.216.34",
            "1.1.1.1",
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
            "2a00:1450::1",
        ] {
            assert!(public(ip.parse().unwrap()), "{ip}");
        }
        // The real guard: these never get a connection, and `localhost` resolves locally.
        let _route = crate::route::test_lock();
        let fetcher = Fetcher::new();
        for url in [
            "http://127.0.0.1:9/",
            "http://[::1]:9/",
            "http://localhost:9/",
            "http://10.0.0.1/",
        ] {
            assert!(
                fetcher.resolve(&Url::parse(url).unwrap()).is_none(),
                "{url}"
            );
            assert!(
                fetcher.make(&Url::parse(url).unwrap(), &stop()).is_none(),
                "{url}"
            );
        }
    }

    /// Serve canned replies on 127.0.0.1, one per connection, by path.
    fn serve(routes: Vec<(&'static str, String, Vec<u8>)>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap_or_default();
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) <= 2 {
                        break;
                    }
                }
                let (head, body) = routes.iter().find(|(route, ..)| *route == path).map_or(
                    ("404 Not Found\r\n".to_owned(), Vec::new()),
                    |(_, head, body)| (head.clone(), body.clone()),
                );
                let mut stream = &stream;
                let _ = write!(
                    stream,
                    "HTTP/1.1 {head}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body);
            }
        });
        port
    }

    fn png() -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbImage::new(64, 32)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    #[test]
    fn a_page_becomes_a_card_with_its_picture() {
        let _route = crate::route::test_lock();
        let html = r#"<head><meta property="og:title" content="Big news">
            <meta property="og:description" content="Something happened.">
            <meta property="og:image" content="/moved.png"></head>"#;
        let port = serve(vec![
            (
                "/start",
                "301 Moved\r\nLocation: /story\r\n".into(),
                Vec::new(),
            ),
            (
                "/story",
                "200 OK\r\nContent-Type: text/html; charset=utf-8\r\n".into(),
                html.into(),
            ),
            (
                "/moved.png",
                "302 Found\r\nLocation: /pic.png\r\n".into(),
                Vec::new(),
            ),
            (
                "/pic.png",
                "200 OK\r\nContent-Type: image/png\r\n".into(),
                png(),
            ),
        ]);
        let url = Url::parse(&format!("http://127.0.0.1:{port}/start")).unwrap();
        let made = Fetcher::local().make(&url, &stop()).unwrap();
        assert_eq!(made.0.title, "Big news");
        assert_eq!(made.0.description, "Something happened.");
        assert_eq!(
            made.0.url,
            url.as_str(),
            "the link as sent, not where it led"
        );
        assert_eq!(made.1, Some(png()));
        assert!(
            Fetcher::new().make(&url, &stop()).is_none(),
            "the guard holds outside tests"
        );
    }

    #[test]
    fn too_many_redirects_or_no_page_give_nothing() {
        let _route = crate::route::test_lock();
        let port = serve(vec![
            ("/1", "302 Found\r\nLocation: /2\r\n".into(), Vec::new()),
            ("/2", "302 Found\r\nLocation: /3\r\n".into(), Vec::new()),
            ("/3", "302 Found\r\nLocation: /4\r\n".into(), Vec::new()),
            ("/4", "302 Found\r\nLocation: /5\r\n".into(), Vec::new()),
            (
                "/5",
                "200 OK\r\nContent-Type: text/html\r\n".into(),
                b"<title>t</title>".to_vec(),
            ),
            (
                "/pdf",
                "200 OK\r\nContent-Type: application/pdf\r\n".into(),
                b"%PDF".to_vec(),
            ),
            (
                "/away",
                "302 Found\r\nLocation: file:///etc/passwd\r\n".into(),
                Vec::new(),
            ),
        ]);
        let fetcher = Fetcher::local();
        for path in ["/1", "/pdf", "/away", "/missing"] {
            let url = Url::parse(&format!("http://127.0.0.1:{port}{path}")).unwrap();
            assert!(fetcher.make(&url, &stop()).is_none(), "{path}");
        }
        let url = Url::parse(&format!("http://127.0.0.1:{port}/2")).unwrap();
        assert!(
            fetcher.make(&url, &stop()).is_some(),
            "three redirects are fine"
        );
    }

    #[test]
    fn a_cancelled_or_late_preview_makes_no_request() {
        let _route = crate::route::test_lock();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = Url::parse(&format!(
            "http://127.0.0.1:{}/",
            listener.local_addr().unwrap().port()
        ))
        .unwrap();
        let cancelled = Stop {
            cancel: Arc::new(AtomicBool::new(true)),
            deadline: Instant::now() + TIMEOUT,
        };
        let late = Stop {
            cancel: Arc::new(AtomicBool::new(false)),
            deadline: Instant::now(),
        };
        for stop in [cancelled, late] {
            assert!(Fetcher::local().make(&url, &stop).is_none());
        }
        assert!(listener.accept().is_err(), "nothing connected");
    }

    #[test]
    fn cards_are_shortened_to_fit_their_file() {
        let card = Card {
            url: format!("https://example.org/{}", "a".repeat(MAX_URL - 20)),
            title: "\u{1F980}".repeat(MAX_TITLE),
            description: "\u{1F980}".repeat(MAX_DESCRIPTION),
            site: "\u{1F980}".repeat(MAX_SITE),
        };
        assert!(card.encode().len() > MAX_CARD_BYTES as usize);
        let fitted = card.clone().fit().unwrap();
        assert!(fitted.encode().len() <= MAX_CARD_BYTES as usize);
        assert!(!fitted.title.is_empty());
        assert!(fitted.description.len() < card.description.len());
        let hopeless = Card {
            url: format!("https://example.org/{}", "a".repeat(4100)),
            ..card
        };
        assert!(hopeless.fit().is_none());
    }

    #[test]
    fn previews_are_asked_for_until_chosen() {
        assert_eq!(crate::audio::Settings::default().create_link_previews, None);
    }

    #[test]
    fn a_card_must_be_about_a_link_in_its_message() {
        let card = Card {
            url: "https://news.example.org/story".into(),
            title: "Story".into(),
            description: String::new(),
            site: String::new(),
        };
        assert!(matches(&card, ["https://news.example.org/story"]));
        assert!(
            matches(&card, ["HTTPS://News.Example.org:443/story"]),
            "normalised"
        );
        assert!(
            !matches(&card, ["https://news.example.org/other"]),
            "same host, other page"
        );
        assert!(!matches(&card, ["https://news.example.org/story/"]));
        assert!(
            !matches(&card, std::iter::empty::<&str>()),
            "no link at all"
        );
    }
}
