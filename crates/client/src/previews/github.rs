//! GitHub repositories, issues and pull requests, via the public REST API. It allows
//! 60 requests an hour per address, so results are kept long and its rate limit pauses
//! the whole site until it resets.

use serde_json::Value;
use url::Url;

use super::{
    Details, Failure, Field, Get, MAX_DESCRIPTION, MAX_FIELD, MAX_JSON, MAX_TITLE, Provider,
    json_text, safe_url, token,
};
use crate::i18n::Lang;

const API: &str = "api.github.com";
const AVATARS: &str = "avatars.githubusercontent.com";
/// First path parts that are GitHub's own pages, not owners.
const RESERVED: &[&str] = &[
    "about",
    "apps",
    "collections",
    "enterprise",
    "explore",
    "features",
    "login",
    "marketplace",
    "new",
    "notifications",
    "organizations",
    "orgs",
    "pricing",
    "search",
    "settings",
    "sponsors",
    "topics",
];

pub struct GitHub;

impl Provider for GitHub {
    fn id(&self) -> &'static str {
        "github"
    }

    fn name(&self) -> &'static str {
        "GitHub"
    }

    fn key(&self, url: &Url) -> Option<String> {
        if !matches!(url.host_str()?, "github.com" | "www.github.com") {
            return None;
        }
        let path: Vec<_> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        let (owner, repo) = (*path.first()?, *path.get(1)?);
        let named = token(owner, 39, |b| b.is_ascii_alphanumeric() || b == b'-')
            && !RESERVED.contains(&owner.to_ascii_lowercase().as_str())
            && token(repo, 100, |b| {
                b.is_ascii_alphanumeric() || b"._-".contains(&b)
            })
            && !repo.starts_with('.');
        if !named {
            return None;
        }
        match path.get(2..) {
            Some([]) => Some(format!("{owner}/{repo}")),
            Some([kind @ ("issues" | "pull"), number, ..])
                if token(number, 10, |b| b.is_ascii_digit()) =>
            {
                Some(format!(
                    "{owner}/{repo}/{kind}/{}",
                    number.parse::<u32>().ok()?
                ))
            }
            _ => None,
        }
    }

    fn fetch(&self, get: &Get, key: &str, _: Lang, _: Option<&str>) -> Result<Details, Failure> {
        let url = Url::parse(&format!("https://{API}/repos/{}", api_path(key)))
            .map_err(|_| Failure::Item)?;
        let bytes = get.request(
            &url,
            MAX_JSON,
            |host| host == API,
            Some("application/vnd.github+json"),
        )?;
        parse(&bytes, key).ok_or(Failure::Item)
    }

    fn image_host(&self, host: &str) -> bool {
        host == AVATARS
    }

    fn fresh(&self) -> u64 {
        12 * 60 * 60
    }
}

/// The API path for a key: the pulls endpoint tells merged apart, and the page says
/// `pull`. Only the kind part changes, never an owner or repository named `pull`.
fn api_path(key: &str) -> String {
    match key.split('/').collect::<Vec<_>>().as_slice() {
        [owner, repo, "pull", number] => format!("{owner}/{repo}/pulls/{number}"),
        _ => key.to_owned(),
    }
}

pub fn parse(bytes: &[u8], key: &str) -> Option<Details> {
    let reply: Value = serde_json::from_slice(bytes).ok()?;
    let parts: Vec<_> = key.split('/').collect();
    let repo = format!("{}/{}", parts.first()?, parts.get(1)?);
    let avatar = |person: &str| {
        reply
            .pointer(&format!("/{person}/avatar_url"))
            .and_then(Value::as_str)
            .and_then(|u| safe_url(u, |host| host == AVATARS))
            .map(String::from)
    };
    let mut details = match parts.get(2..) {
        Some([kind, number]) => {
            let state = match reply.get("state").and_then(Value::as_str)? {
                _ if reply.get("merged") == Some(&Value::Bool(true)) => t!("previews.merged"),
                "open" => t!("previews.open"),
                "closed" => t!("previews.closed"),
                _ => return None,
            };
            let mut details = Details::new(
                json_text(reply.get("title"), MAX_TITLE)?,
                format!("https://github.com/{repo}/{kind}/{number}"),
            );
            details.description = format!("{repo} #{number}");
            details.fields.push((Field::State, state.to_owned()));
            details.image = avatar("user");
            details
        }
        _ => {
            let mut details = Details::new(
                json_text(reply.get("full_name"), MAX_TITLE)?,
                format!("https://github.com/{repo}"),
            );
            details.description =
                json_text(reply.get("description"), MAX_DESCRIPTION).unwrap_or_default();
            if let Some(stars) = reply.get("stargazers_count").and_then(Value::as_u64) {
                details.fields.push((Field::Stars, stars.to_string()));
            }
            if let Some(language) = json_text(reply.get("language"), MAX_FIELD) {
                details.fields.push((Field::Language, language));
            }
            details.image = avatar("owner");
            details
        }
    };
    details.icon = true;
    Some(details)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_issue_and_pull_links_only() {
        let key = |s: &str| GitHub.key(&Url::parse(s).unwrap());
        assert_eq!(
            key("https://github.com/rust-lang/rust").as_deref(),
            Some("rust-lang/rust")
        );
        assert_eq!(
            key("https://github.com/rust-lang/rust/issues/42#issuecomment-1").as_deref(),
            Some("rust-lang/rust/issues/42")
        );
        assert_eq!(
            key("https://github.com/rust-lang/rust/pull/7/files").as_deref(),
            Some("rust-lang/rust/pull/7")
        );
        for link in [
            "https://github.com/rust-lang",
            "https://github.com/settings/profile",
            "https://github.com/rust-lang/rust/blob/main/README.md",
            "https://github.com/rust-lang/rust/issues/x",
            "https://github.com.evil.example/rust-lang/rust",
            "https://gist.github.com/rust-lang/rust",
            "https://github.com/a/..",
        ] {
            assert_eq!(key(link), None, "{link}");
        }
    }

    #[test]
    fn only_the_kind_part_becomes_pulls() {
        let key = |s: &str| GitHub.key(&Url::parse(s).unwrap()).unwrap();
        assert_eq!(api_path(&key("https://github.com/pull/pull")), "pull/pull");
        assert_eq!(
            api_path(&key("https://github.com/someone/pull/pull/7")),
            "someone/pull/pulls/7"
        );
        assert_eq!(
            api_path(&key("https://github.com/someone/pull/issues/7")),
            "someone/pull/issues/7"
        );
    }

    #[test]
    fn replies_become_cards() {
        let repo = br#"{"full_name":"rust-lang/rust","description":"Empowering everyone",
            "stargazers_count":99000,"language":"Rust",
            "owner":{"avatar_url":"https://avatars.githubusercontent.com/u/5430905?v=4"}}"#;
        let details = parse(repo, "rust-lang/rust").unwrap();
        assert_eq!(details.title, "rust-lang/rust");
        assert_eq!(
            details.fields,
            [
                (Field::Stars, "99000".into()),
                (Field::Language, "Rust".into())
            ]
        );
        assert!(details.icon && details.image.is_some());
        let pull = br#"{"title":"Fix it","state":"closed","merged":true,
            "user":{"avatar_url":"https://avatars.githubusercontent.evil.example/u/1"}}"#;
        let details = parse(pull, "rust-lang/rust/pull/7").unwrap();
        assert_eq!(details.description, "rust-lang/rust #7");
        assert_eq!(details.fields, [(Field::State, "Merged".into())]);
        assert_eq!(details.url, "https://github.com/rust-lang/rust/pull/7");
        assert_eq!(details.image, None);
        let issue = br#"{"title":"Bug","state":"open"}"#;
        let details = parse(issue, "rust-lang/rust/issues/42").unwrap();
        assert_eq!(details.fields, [(Field::State, "Open".into())]);
        assert!(parse(br#"{"title":"Bug","state":"weird"}"#, "a/b/issues/1").is_none());
    }
}
