//! Spotify tracks, albums, playlists and episodes (`open.spotify.com/<kind>/<id>`), via oEmbed.

use url::Url;

use super::{Details, Failure, Get, MAX_JSON, MAX_TITLE, Provider, json_text, safe_url, token};
use crate::i18n::Lang;

const HOST: &str = "open.spotify.com";

pub struct Spotify;

impl Provider for Spotify {
    fn id(&self) -> &'static str {
        "spotify"
    }

    fn name(&self) -> &'static str {
        "Spotify"
    }

    fn key(&self, url: &Url) -> Option<String> {
        if url.host_str() != Some(HOST) {
            return None;
        }
        let mut path = url.path_segments()?.peekable();
        // Localised links start with `intl-<language>`.
        path.next_if(|segment| segment.starts_with("intl-"));
        let (kind, id) = (path.next()?, path.next()?);
        // IDs are 22 base62 characters.
        (matches!(kind, "track" | "album" | "playlist" | "episode")
            && id.len() == 22
            && token(id, 22, |b| b.is_ascii_alphanumeric()))
        .then(|| format!("{kind}/{id}"))
    }

    fn fetch(&self, get: &Get, key: &str, _: Lang, _: Option<&str>) -> Result<Details, Failure> {
        let mut url = Url::parse(&format!("https://{HOST}/oembed")).map_err(|_| Failure::Item)?;
        url.query_pairs_mut()
            .append_pair("url", &format!("https://{HOST}/{key}"));
        let bytes = get.request(&url, MAX_JSON, |host| host == HOST, None)?;
        parse(&bytes, key).ok_or(Failure::Item)
    }

    fn image_host(&self, host: &str) -> bool {
        image_host(host)
    }

    fn fresh(&self) -> u64 {
        12 * 60 * 60
    }

    fn action(&self) -> String {
        t!("previews.listen_on", site = self.name())
    }
}

fn image_host(host: &str) -> bool {
    host == "i.scdn.co" || host.ends_with(".spotifycdn.com")
}

pub fn parse(bytes: &[u8], key: &str) -> Option<Details> {
    let reply: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let mut details = Details::new(
        json_text(reply.get("title"), MAX_TITLE)?,
        format!("https://{HOST}/{key}"),
    );
    details.image = (reply.get("thumbnail_url").and_then(|v| v.as_str()))
        .and_then(|u| safe_url(u, image_host))
        .map(String::from);
    Some(details)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn music_links_and_nothing_else() {
        let key = |s: &str| Spotify.key(&Url::parse(s).unwrap());
        assert_eq!(
            key("https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC?si=x").as_deref(),
            Some("track/4uLU6hMCjMI75M1A2tKUQC")
        );
        assert_eq!(
            key("https://open.spotify.com/intl-de/album/4uLU6hMCjMI75M1A2tKUQC").as_deref(),
            Some("album/4uLU6hMCjMI75M1A2tKUQC")
        );
        for link in [
            "https://open.spotify.com/artist/4uLU6hMCjMI75M1A2tKUQC",
            "https://open.spotify.com/track/short",
            "https://open.spotify.com.evil.example/track/4uLU6hMCjMI75M1A2tKUQC",
            "https://spotify.com/track/4uLU6hMCjMI75M1A2tKUQC",
        ] {
            assert_eq!(key(link), None, "{link}");
        }
    }

    #[test]
    fn a_reply_becomes_a_card() {
        let reply = br#"{"title":"Never Gonna Give You Up",
            "thumbnail_url":"https://image-cdn-ak.spotifycdn.com/image/ab67616d"}"#;
        let details = parse(reply, "track/4uLU6hMCjMI75M1A2tKUQC").unwrap();
        assert_eq!(details.title, "Never Gonna Give You Up");
        assert_eq!(
            details.url,
            "https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC"
        );
        assert!(details.image.is_some());
        let foreign = br#"{"title":"x","thumbnail_url":"https://user@i.scdn.co/image/a"}"#;
        assert_eq!(parse(foreign, "track/x").unwrap().image, None);
    }
}
