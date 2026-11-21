//! Wikipedia articles (`<language>.wikipedia.org/wiki/<title>`), via the page summary
//! API of the link's own language.

use url::Url;

use super::{
    Details, Failure, Get, MAX_DESCRIPTION, MAX_JSON, MAX_TITLE, Provider, json_text, safe_url,
    token,
};
use crate::i18n::Lang;

const PICTURES: &str = "upload.wikimedia.org";

pub struct Wikipedia;

impl Provider for Wikipedia {
    fn id(&self) -> &'static str {
        "wikipedia"
    }

    fn name(&self) -> &'static str {
        "Wikipedia"
    }

    fn key(&self, url: &Url) -> Option<String> {
        let host = url.host_str()?.strip_suffix(".wikipedia.org")?;
        let lang = host.strip_suffix(".m").unwrap_or(host);
        let mut path = url.path_segments()?;
        if path.next() != Some("wiki") {
            return None;
        }
        // Titles may hold `/` (`AC/DC`): everything after `/wiki/` is the title, kept as
        // the link encoded it and joined into one path segment with `%2F`.
        let mut parts: Vec<&str> = path.collect();
        while parts.last() == Some(&"") {
            parts.pop();
        }
        let title = parts.join("%2F");
        (token(lang, 12, |b| b.is_ascii_lowercase() || b == b'-')
            && lang != "www"
            && !parts.contains(&"")
            && token(&title, 300, |b| b.is_ascii_graphic() && b != b'/'))
        .then(|| format!("{lang}/{title}"))
    }

    fn fetch(&self, get: &Get, key: &str, _: Lang, _: Option<&str>) -> Result<Details, Failure> {
        let (lang, title) = key.split_once('/').ok_or(Failure::Item)?;
        let host = format!("{lang}.wikipedia.org");
        let url = summary(lang, title).ok_or(Failure::Item)?;
        let bytes = get.request(&url, MAX_JSON, |h| h == host, None)?;
        parse(&bytes, key).ok_or(Failure::Item)
    }

    fn image_host(&self, host: &str) -> bool {
        host == PICTURES
    }

    fn fresh(&self) -> u64 {
        12 * 60 * 60
    }
}

/// The summary API address; `title` is one encoded path segment.
fn summary(lang: &str, title: &str) -> Option<Url> {
    Url::parse(&format!(
        "https://{lang}.wikipedia.org/api/rest_v1/page/summary/{title}"
    ))
    .ok()
}

pub fn parse(bytes: &[u8], key: &str) -> Option<Details> {
    let reply: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let (lang, title) = key.split_once('/')?;
    let mut details = Details::new(
        json_text(reply.get("title"), MAX_TITLE)?,
        format!("https://{lang}.wikipedia.org/wiki/{title}"),
    );
    details.description = json_text(reply.get("extract"), MAX_DESCRIPTION).unwrap_or_default();
    details.image = (reply.pointer("/thumbnail/source").and_then(|v| v.as_str()))
        .and_then(|u| safe_url(u, |host| host == PICTURES))
        .map(String::from);
    Some(details)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn article_links_keep_their_language() {
        let key = |s: &str| Wikipedia.key(&Url::parse(s).unwrap());
        assert_eq!(
            key("https://nl.wikipedia.org/wiki/Rust_(programmeertaal)").as_deref(),
            Some("nl/Rust_(programmeertaal)")
        );
        assert_eq!(
            key("https://en.m.wikipedia.org/wiki/Caf%C3%A9#History").as_deref(),
            Some("en/Caf%C3%A9")
        );
        assert_eq!(
            key("https://en.wikipedia.org/wiki/AC/DC").as_deref(),
            Some("en/AC%2FDC")
        );
        let ac_dc = parse(br#"{"title":"AC/DC"}"#, "en/AC%2FDC").unwrap();
        assert_eq!(ac_dc.url, "https://en.wikipedia.org/wiki/AC%2FDC");
        assert_eq!(
            summary("en", "AC%2FDC").unwrap().as_str(),
            "https://en.wikipedia.org/api/rest_v1/page/summary/AC%2FDC"
        );
        assert_eq!(
            summary("en", "AC%2FDC")
                .unwrap()
                .path_segments()
                .unwrap()
                .count(),
            5,
            "one segment for the title"
        );
        for link in [
            "https://www.wikipedia.org/wiki/Rust",
            "https://en.wikipedia.org/w/index.php?title=Rust",
            "https://en.wikipedia.org.evil.example/wiki/Rust",
            "https://evil.example/en.wikipedia.org/wiki/Rust",
            "https://EN.wikipedia.org/wiki/",
        ] {
            assert_eq!(key(link), None, "{link}");
        }
    }

    #[test]
    fn a_summary_becomes_a_card() {
        let reply = br#"{"title":"Rust (programming language)",
            "extract":"Rust is a <b>general-purpose</b> programming language.",
            "thumbnail":{"source":"https://upload.wikimedia.org/wikipedia/commons/r.png"}}"#;
        let details = parse(reply, "en/Rust_(programming_language)").unwrap();
        assert_eq!(
            details.description,
            "Rust is a general-purpose programming language."
        );
        assert_eq!(
            details.url,
            "https://en.wikipedia.org/wiki/Rust_(programming_language)"
        );
        assert!(details.image.is_some());
        let foreign =
            br#"{"title":"x","thumbnail":{"source":"https://upload.wikimedia.org:444/r.png"}}"#;
        assert_eq!(parse(foreign, "en/x").unwrap().image, None);
    }
}
