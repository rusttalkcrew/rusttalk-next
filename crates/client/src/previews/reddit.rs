//! Reddit posts (`reddit.com/r/<subreddit>/comments/<id>`), via the post's JSON. Its
//! oEmbed reply has no picture and no adult flag, so the JSON is the one to trust.
//! Reddit often refuses unknown apps; that just leaves the plain link.

use serde_json::Value;
use url::Url;

use super::{
    Details, Failure, Field, Get, MAX_JSON, MAX_TITLE, Provider, json_text, safe_url, token,
};
use crate::i18n::Lang;

const HOST: &str = "www.reddit.com";

pub struct Reddit;

fn subreddit(name: &str) -> bool {
    token(name, 21, |b| b.is_ascii_alphanumeric() || b == b'_')
}

impl Provider for Reddit {
    fn id(&self) -> &'static str {
        "reddit"
    }

    fn name(&self) -> &'static str {
        "Reddit"
    }

    fn key(&self, url: &Url) -> Option<String> {
        if !matches!(
            url.host_str()?,
            "reddit.com" | "www.reddit.com" | "old.reddit.com" | "new.reddit.com"
        ) {
            return None;
        }
        let mut path = url.path_segments()?;
        let (Some("r"), Some(sub), Some("comments"), Some(id)) =
            (path.next(), path.next(), path.next(), path.next())
        else {
            return None;
        };
        (subreddit(sub) && token(id, 12, |b| b.is_ascii_lowercase() || b.is_ascii_digit()))
            .then(|| format!("{sub}/{id}"))
    }

    fn fetch(&self, get: &Get, key: &str, _: Lang, _: Option<&str>) -> Result<Details, Failure> {
        let (sub, id) = key.split_once('/').ok_or(Failure::Item)?;
        let url = Url::parse(&format!(
            "https://{HOST}/r/{sub}/comments/{id}.json?limit=1&raw_json=1"
        ))
        .map_err(|_| Failure::Item)?;
        let bytes = get.request(&url, MAX_JSON, |host| host == HOST, None)?;
        parse(&bytes, key).ok_or(Failure::Item)
    }

    fn image_host(&self, host: &str) -> bool {
        image_host(host)
    }

    fn fresh(&self) -> u64 {
        12 * 60 * 60
    }
}

fn image_host(host: &str) -> bool {
    matches!(
        host,
        "i.redd.it" | "preview.redd.it" | "external-preview.redd.it"
    )
}

pub fn parse(bytes: &[u8], key: &str) -> Option<Details> {
    let reply: Value = serde_json::from_slice(bytes).ok()?;
    let post = reply.pointer("/0/data/children/0/data")?;
    let (sub, id) = key.split_once('/')?;
    if post.get("id").and_then(Value::as_str) != Some(id) {
        return None;
    }
    let mut details = Details::new(
        json_text(post.get("title"), MAX_TITLE)?,
        format!("https://{HOST}/r/{sub}/comments/{id}/"),
    );
    let name = (post.get("subreddit").and_then(Value::as_str))
        .filter(|name| subreddit(name))
        .unwrap_or(sub);
    details.fields.push((Field::Subreddit, format!("r/{name}")));
    // Anything but a clear "no" counts as adult content: no picture then.
    details.nsfw = post.get("over_18") != Some(&Value::Bool(false));
    if !details.nsfw {
        details.image = (post
            .pointer("/preview/images/0/source/url")
            .and_then(Value::as_str))
        .and_then(|u| safe_url(u, image_host))
        .map(String::from);
    }
    Some(details)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(over_18: &str) -> String {
        format!(
            r#"[{{"data":{{"children":[{{"data":{{"id":"abc123","title":"Look at this",
            "subreddit":"rust","over_18":{over_18},
            "preview":{{"images":[{{"source":{{"url":"https://preview.redd.it/x.jpg?s=1"}}}}]}}}}}}]}}}},{{}}]"#
        )
    }

    #[test]
    fn post_links_only() {
        let key = |s: &str| Reddit.key(&Url::parse(s).unwrap());
        assert_eq!(
            key("https://old.reddit.com/r/rust/comments/abc123/look_at_this/").as_deref(),
            Some("rust/abc123")
        );
        for link in [
            "https://www.reddit.com/r/rust/",
            "https://www.reddit.com/user/someone/comments/abc123/",
            "https://www.reddit.com/r/r-ust/comments/abc123/",
            "https://reddit.com.evil.example/r/rust/comments/abc123/",
        ] {
            assert_eq!(key(link), None, "{link}");
        }
    }

    #[test]
    fn adult_posts_get_a_label_and_no_picture() {
        let safe = parse(reply("false").as_bytes(), "rust/abc123").unwrap();
        assert_eq!(safe.title, "Look at this");
        assert_eq!(safe.fields, [(Field::Subreddit, "r/rust".into())]);
        assert!(!safe.nsfw && safe.image.is_some());
        let adult = parse(reply("true").as_bytes(), "rust/abc123").unwrap();
        assert!(adult.nsfw && adult.image.is_none());
        let unknown = parse(reply("null").as_bytes(), "rust/abc123").unwrap();
        assert!(unknown.nsfw && unknown.image.is_none());
        assert!(
            parse(reply("false").as_bytes(), "rust/other").is_none(),
            "another post"
        );
    }
}
