//! Steam store app pages: `https://store.steampowered.com/app/<id>/...`.

use serde_json::Value;
use url::Url;

use super::{
    Details, Field, Get, MAX_DESCRIPTION, MAX_FIELD, MAX_JSON, MAX_TITLE, Provider, plain,
};
use crate::i18n::Lang;

const STORE: &str = "store.steampowered.com";

pub struct Steam;

impl Provider for Steam {
    fn id(&self) -> &'static str {
        "steam"
    }

    fn name(&self) -> &'static str {
        "Steam"
    }

    fn key(&self, url: &Url) -> Option<String> {
        if url.host_str() != Some(STORE) {
            return None;
        }
        let mut path = url.path_segments()?;
        let (Some("app"), Some(id)) = (path.next(), path.next()) else {
            return None;
        };
        // App IDs are 32-bit numbers.
        (id.len() <= 10 && id.bytes().all(|b| b.is_ascii_digit()))
            .then(|| id.parse::<u32>().ok())
            .flatten()
            .map(|id| id.to_string())
    }

    fn fetch(&self, get: &Get, key: &str, lang: Lang, country: Option<&str>) -> Option<Details> {
        let mut url = Url::parse(&format!("https://{STORE}/api/appdetails")).ok()?;
        url.query_pairs_mut()
            .append_pair("appids", key)
            .append_pair("l", language(lang));
        if let Some(country) = country {
            url.query_pairs_mut().append_pair("cc", country);
        }
        parse(&get.get(&url, MAX_JSON, |host| host == STORE)?, key)
    }

    fn image_host(&self, host: &str) -> bool {
        image_host(host)
    }
}

fn image_host(host: &str) -> bool {
    host.ends_with(".steamstatic.com") || host == "steamcdn-a.akamaihd.net"
}

/// Steam's name for the app's language.
fn language(lang: Lang) -> &'static str {
    match lang {
        Lang::En => "english",
        Lang::De => "german",
        Lang::Nl => "dutch",
        Lang::Ja => "japanese",
    }
}

/// The details in an `appdetails` reply for app `id`. Every field is checked and shortened.
pub fn parse(bytes: &[u8], id: &str) -> Option<Details> {
    let reply: Value = serde_json::from_slice(bytes).ok()?;
    let app = reply.get(id)?;
    if app.get("success")? != &Value::Bool(true) {
        return None;
    }
    let data = app.get("data")?;
    let text = |value: &Value| value.as_str().map(|s| plain(s, MAX_FIELD));
    let list = |value: Option<&Value>, key: Option<&str>| {
        let items: Vec<_> = (value.and_then(Value::as_array).into_iter().flatten())
            .filter_map(|item| text(key.map_or(item, |key| &item[key])))
            .filter(|item| !item.is_empty())
            .take(4)
            .collect();
        (!items.is_empty()).then(|| plain(&items.join(", "), MAX_FIELD))
    };
    let title = plain(data.get("name")?.as_str()?, MAX_TITLE);
    if title.is_empty() {
        return None;
    }
    let price = if data.get("is_free") == Some(&Value::Bool(true)) {
        Some(t!("previews.free").to_owned())
    } else {
        data.pointer("/price_overview/final_formatted")
            .and_then(text)
    };
    let platforms: Vec<_> = [("windows", "Windows"), ("mac", "macOS"), ("linux", "Linux")]
        .into_iter()
        .filter(|(key, _)| data.pointer(&format!("/platforms/{key}")) == Some(&Value::Bool(true)))
        .map(|(_, name)| name)
        .collect();
    let fields = [
        (Field::Price, price),
        (
            Field::Released,
            data.pointer("/release_date/date").and_then(text),
        ),
        (Field::Developer, list(data.get("developers"), None)),
        (Field::Genres, list(data.get("genres"), Some("description"))),
        (
            Field::Platforms,
            (!platforms.is_empty()).then(|| platforms.join(", ")),
        ),
    ]
    .into_iter()
    .filter_map(|(field, value)| Some((field, value.filter(|v| !v.is_empty())?)))
    .collect();
    let image = data
        .get("header_image")
        .and_then(Value::as_str)
        .filter(|s| s.len() <= 1024)
        .and_then(|s| Url::parse(s).ok())
        .filter(|url| url.scheme() == "https" && url.host_str().is_some_and(image_host))
        .map(String::from);
    Some(Details {
        title,
        description: data
            .get("short_description")
            .and_then(Value::as_str)
            .map(|s| plain(s, MAX_DESCRIPTION))
            .unwrap_or_default(),
        fields,
        url: format!("https://{STORE}/app/{id}/"),
        image,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPLY: &str = r#"{"570":{"success":true,"data":{
        "type":"game","name":"Dota 2","steam_appid":570,"is_free":true,
        "short_description":"Every day, millions of players &quot;battle&quot; <b>as</b> heroes.",
        "header_image":"https://shared.akamai.steamstatic.com/store_item_assets/steam/apps/570/header.jpg?t=1",
        "developers":["Valve"],
        "platforms":{"windows":true,"mac":true,"linux":true},
        "genres":[{"id":"1","description":"Action"},{"id":"2","description":"Strategy"}],
        "release_date":{"coming_soon":false,"date":"9 Jul, 2013"}}}}"#;

    #[test]
    fn a_reply_becomes_checked_details() {
        let details = parse(REPLY.as_bytes(), "570").unwrap();
        assert_eq!(details.title, "Dota 2");
        assert_eq!(
            details.description,
            "Every day, millions of players \"battle\" as heroes."
        );
        assert_eq!(details.url, "https://store.steampowered.com/app/570/");
        assert_eq!(
            details.fields,
            [
                (Field::Price, "Free".to_owned()),
                (Field::Released, "9 Jul, 2013".to_owned()),
                (Field::Developer, "Valve".to_owned()),
                (Field::Genres, "Action, Strategy".to_owned()),
                (Field::Platforms, "Windows, macOS, Linux".to_owned()),
            ]
        );
        assert!(
            details
                .image
                .unwrap()
                .starts_with("https://shared.akamai.steamstatic.com/")
        );
    }

    #[test]
    fn bad_replies_and_foreign_pictures_are_refused() {
        assert!(parse(REPLY.as_bytes(), "571").is_none());
        assert!(parse(br#"{"570":{"success":false}}"#, "570").is_none());
        assert!(parse(b"not json", "570").is_none());
        let paid = REPLY
            .replace(
                r#""is_free":true"#,
                r#""is_free":false,"price_overview":{"final_formatted":"19,50€"}"#,
            )
            .replace(
                "shared.akamai.steamstatic.com",
                "steamstatic.com.evil.example",
            )
            .replace("Dota 2", &"x".repeat(500));
        let details = parse(paid.as_bytes(), "570").unwrap();
        assert_eq!(details.fields[0], (Field::Price, "19,50€".to_owned()));
        assert_eq!(details.image, None);
        assert_eq!(details.title.chars().count(), MAX_TITLE);
    }
}
