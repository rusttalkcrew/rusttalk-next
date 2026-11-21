//! YouTube videos (`youtube.com/watch?v=`, `youtu.be/`, `youtube.com/shorts/`), via oEmbed.

use url::Url;

use super::{
    Details, Failure, Field, Get, MAX_FIELD, MAX_JSON, MAX_TITLE, Provider, json_text, safe_url,
    token,
};
use crate::i18n::Lang;

const OEMBED: &str = "www.youtube.com";
const PICTURES: &str = "i.ytimg.com";

pub struct YouTube;

impl Provider for YouTube {
    fn id(&self) -> &'static str {
        "youtube"
    }

    fn name(&self) -> &'static str {
        "YouTube"
    }

    fn key(&self, url: &Url) -> Option<String> {
        let mut path = url.path_segments()?;
        let id = match url.host_str()? {
            "youtu.be" => path.next()?.to_owned(),
            "youtube.com" | "www.youtube.com" | "m.youtube.com" => match path.next()? {
                "watch" => url.query_pairs().find(|(k, _)| k == "v")?.1.into_owned(),
                "shorts" => path.next()?.to_owned(),
                _ => return None,
            },
            _ => return None,
        };
        // Video IDs are 11 URL-safe base64 characters.
        (id.len() == 11
            && token(&id, 11, |b| {
                b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
            }))
        .then_some(id)
    }

    fn fetch(&self, get: &Get, key: &str, _: Lang, _: Option<&str>) -> Result<Details, Failure> {
        let mut url = Url::parse(&format!("https://{OEMBED}/oembed")).map_err(|_| Failure::Item)?;
        url.query_pairs_mut()
            .append_pair("url", &watch(key))
            .append_pair("format", "json");
        let bytes = get.request(&url, MAX_JSON, |host| host == OEMBED, None)?;
        parse(&bytes, key).ok_or(Failure::Item)
    }

    fn image_host(&self, host: &str) -> bool {
        host == PICTURES
    }

    fn fresh(&self) -> u64 {
        12 * 60 * 60
    }

    fn action(&self) -> String {
        t!("previews.watch_on", site = self.name())
    }
}

fn watch(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

pub fn parse(bytes: &[u8], id: &str) -> Option<Details> {
    let reply: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let mut details = Details::new(json_text(reply.get("title"), MAX_TITLE)?, watch(id));
    if let Some(channel) = json_text(reply.get("author_name"), MAX_FIELD) {
        details.fields.push((Field::Channel, channel));
    }
    details.image = (reply.get("thumbnail_url").and_then(|v| v.as_str()))
        .and_then(|u| safe_url(u, |host| host == PICTURES))
        .map(String::from);
    Some(details)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_links_and_nothing_else() {
        let key = |s: &str| YouTube.key(&Url::parse(s).unwrap());
        for link in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=42",
            "https://youtu.be/dQw4w9WgXcQ?si=x",
            "https://youtube.com/shorts/dQw4w9WgXcQ",
            "https://m.youtube.com/watch?v=dQw4w9WgXcQ",
        ] {
            assert_eq!(key(link).as_deref(), Some("dQw4w9WgXcQ"), "{link}");
        }
        for link in [
            "https://www.youtube.com/watch?v=short",
            "https://www.youtube.com/channel/dQw4w9WgXcQ",
            "https://youtube.com.evil.example/watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com/watch?v=dQw4w9WgX%2F",
        ] {
            assert_eq!(key(link), None, "{link}");
        }
    }

    #[test]
    fn a_reply_becomes_a_card() {
        let reply = br#"{"title":"Never &amp; Gonna","author_name":"Rick Astley",
            "thumbnail_url":"https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg"}"#;
        let details = parse(reply, "dQw4w9WgXcQ").unwrap();
        assert_eq!(details.title, "Never & Gonna");
        assert_eq!(details.fields, [(Field::Channel, "Rick Astley".to_owned())]);
        assert_eq!(details.url, "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        assert!(details.image.is_some());
        let foreign = br#"{"title":"x","thumbnail_url":"https://i.ytimg.com.evil.example/a.jpg"}"#;
        assert_eq!(parse(foreign, "dQw4w9WgXcQ").unwrap().image, None);
        assert!(parse(br#"{"author_name":"no title"}"#, "dQw4w9WgXcQ").is_none());
    }
}
