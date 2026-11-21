//! The app in the person's language. Translations are plain `key = value` tables in
//! `locales/`, built into the binary; nothing is downloaded and the choice never
//! leaves this computer except in the encrypted device sync.
//!
//! `t!("key")` gives the text in the current language, `t!("key", name = value)` fills
//! `{name}` placeholders, and `t!("key", count = n, ...)` picks `key.one` or
//! `key.other` by the language's plural rule. A key missing from a language shows in
//! English, never as an empty label.

use std::{
    collections::HashMap,
    fmt::Display,
    sync::{
        LazyLock,
        atomic::{AtomicU8, Ordering},
    },
};

/// The text for `key` in the current language.
macro_rules! t {
    ($key:literal) => {
        $crate::i18n::text($key)
    };
    ($key:literal, count = $count:expr $(, $name:ident = $value:expr)* $(,)?) => {{
        let count = $count;
        $crate::i18n::plural(
            $key,
            count as i64,
            &[("count", &count as &dyn ::std::fmt::Display)
                $(, (stringify!($name), &$value as &dyn ::std::fmt::Display))*],
        )
    }};
    ($key:literal, $($name:ident = $value:expr),+ $(,)?) => {
        $crate::i18n::format(
            $key,
            &[$((stringify!($name), &$value as &dyn ::std::fmt::Display)),+],
        )
    };
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    En,
    De,
    Nl,
    Ja,
}

pub const LANGS: [Lang; 4] = [Lang::En, Lang::De, Lang::Nl, Lang::Ja];

const SOURCES: [&str; 4] = [
    include_str!("../locales/en.txt"),
    include_str!("../locales/de.txt"),
    include_str!("../locales/nl.txt"),
    include_str!("../locales/ja.txt"),
];

impl Lang {
    /// The ID kept in the settings.
    pub fn id(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::De => "de",
            Self::Nl => "nl",
            Self::Ja => "ja",
        }
    }

    /// The language's own name, so people find it whatever the app shows now.
    pub fn name(self) -> &'static str {
        match self {
            Self::En => "English",
            Self::De => "Deutsch",
            Self::Nl => "Nederlands",
            Self::Ja => "日本語",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        LANGS.into_iter().find(|lang| lang.id() == id)
    }

    /// A BCP 47 or POSIX locale such as `de-AT` or `nl_NL.UTF-8`.
    fn from_locale(locale: &str) -> Option<Self> {
        let language = locale.split(['-', '_', '.']).next()?;
        Self::from_id(&language.to_ascii_lowercase())
    }

    /// The plural form for `count`: CLDR's rule, reduced to what these languages use.
    fn plural(self, count: i64) -> &'static str {
        match self {
            Self::Ja => "other",
            Self::En | Self::De | Self::Nl if count == 1 => "one",
            Self::En | Self::De | Self::Nl => "other",
        }
    }
}

/// `u8::MAX` until the settings pick a language: the computer's language until then.
static CURRENT: AtomicU8 = AtomicU8::new(u8::MAX);

/// The language the app shows.
pub fn current() -> Lang {
    LANGS
        .get(usize::from(CURRENT.load(Ordering::Relaxed)))
        .copied()
        .unwrap_or_else(system)
}

/// Show the app in `choice`: a language ID, or "system" (or anything unknown) for the
/// computer's language.
pub fn set(choice: &str) {
    let lang = Lang::from_id(choice).unwrap_or_else(system);
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

/// The first of the computer's preferred languages that has a translation, else English.
pub fn system() -> Lang {
    // Tests check English texts whatever the machine's language.
    if cfg!(test) {
        return Lang::En;
    }
    static SYSTEM: LazyLock<Lang> = LazyLock::new(|| pick(sys_locale::get_locales()));
    *SYSTEM
}

fn pick(locales: impl IntoIterator<Item = String>) -> Lang {
    locales
        .into_iter()
        .find_map(|locale| Lang::from_locale(&locale))
        .unwrap_or(Lang::En)
}

type Table = HashMap<&'static str, String>;

static TABLES: LazyLock<Vec<Table>> = LazyLock::new(|| SOURCES.iter().map(|s| parse(s)).collect());

/// `key = value` lines; `#` starts a comment line and `\n` in a value is a line break.
fn parse(source: &'static str) -> Table {
    source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once(" = "))
        .map(|(key, value)| (key.trim(), value.replace("\\n", "\n")))
        .collect()
}

fn lookup(lang: Lang, key: &str) -> Option<&'static str> {
    find(&TABLES, lang, key)
}

fn find<'a>(tables: &'a [Table], lang: Lang, key: &str) -> Option<&'a str> {
    (tables[lang as usize].get(key))
        .or_else(|| tables[Lang::En as usize].get(key))
        .map(String::as_str)
}

/// The text for `key`; the key itself if even English lacks it (a test prevents that).
pub fn text(key: &'static str) -> &'static str {
    lookup(current(), key).unwrap_or(key)
}

pub fn format(key: &'static str, args: &[(&str, &dyn Display)]) -> String {
    fill(text(key), args)
}

pub fn plural(key: &'static str, count: i64, args: &[(&str, &dyn Display)]) -> String {
    let lang = current();
    let template = lookup(lang, &format!("{key}.{}", lang.plural(count)))
        .or_else(|| lookup(lang, &format!("{key}.other")))
        // A text with a count but no plural forms, like "Members — {count}".
        .or_else(|| lookup(lang, key))
        .unwrap_or(key);
    fill(template, args)
}

/// Replace each `{name}` with its value. Never pieces sentences together: each
/// language orders its own placeholders.
fn fill(template: &str, args: &[(&str, &dyn Display)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let value = after.find('}').and_then(|end| {
            let name = &after[..end];
            let (_, value) = args.iter().find(|(arg, _)| *arg == name)?;
            Some((value.to_string(), end))
        });
        match value {
            Some((value, end)) => {
                out.push_str(&value);
                rest = &after[end + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// A clock time or date in the current language's format; see the `time.*` keys.
pub fn time(at: chrono::DateTime<chrono::Local>, pattern: &'static str) -> String {
    at.format(text(pattern)).to_string()
}

/// A number with `decimals` decimals and the language's decimal separator.
pub fn number(value: f64, decimals: usize) -> String {
    let text = format!("{value:.decimals$}");
    match current() {
        Lang::De | Lang::Nl => text.replace('.', ","),
        Lang::En | Lang::Ja => text,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn placeholders(text: &str) -> BTreeSet<&str> {
        text.split('{')
            .skip(1)
            .filter_map(|part| part.split_once('}').map(|(name, _)| name))
            .collect()
    }

    #[test]
    fn every_language_has_every_key_with_the_same_placeholders() {
        let english = &TABLES[Lang::En as usize];
        for lang in LANGS {
            let table = &TABLES[lang as usize];
            let source = SOURCES[lang as usize];
            let lines = (source.lines())
                .filter(|l| !l.starts_with('#') && l.contains(" = "))
                .count();
            assert_eq!(lines, table.len(), "{lang:?} has a duplicate key");
            for (key, text) in english {
                let translated = table
                    .get(key)
                    .unwrap_or_else(|| panic!("{lang:?} lacks {key}"));
                assert!(!translated.trim().is_empty(), "{lang:?} {key} is empty");
                assert_eq!(
                    placeholders(text),
                    placeholders(translated),
                    "{lang:?} {key} placeholders"
                );
            }
            for key in table.keys() {
                assert!(english.contains_key(key), "{lang:?} has unknown key {key}");
            }
        }
    }

    #[test]
    fn plural_keys_come_in_pairs() {
        for key in TABLES[Lang::En as usize].keys() {
            if let Some(base) = key.strip_suffix(".one") {
                assert!(
                    lookup(Lang::En, &format!("{base}.other")).is_some(),
                    "{key}"
                );
            }
        }
    }

    /// Every `t!("key")` in the client source has an English text.
    #[test]
    fn every_used_key_exists() {
        let mut stack = vec![std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src"
        ))];
        let mut used = 0;
        while let Some(path) = stack.pop() {
            if path.is_dir() {
                stack.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
                continue;
            }
            if path.ends_with("i18n.rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            let parts = source.split("t!(\"").collect::<Vec<_>>();
            // `format!("` ends in the same characters; only a bare `t!` counts.
            for pair in parts.windows(2) {
                if pair[0].ends_with(|c: char| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                let key = pair[1].split('"').next().unwrap();
                used += 1;
                let english = &TABLES[Lang::En as usize];
                assert!(
                    english.contains_key(key)
                        || (english.contains_key(&*format!("{key}.one"))
                            && english.contains_key(&*format!("{key}.other"))),
                    "{} uses missing key {key}",
                    path.display()
                );
            }
        }
        assert!(used > 100, "found only {used} keys");
    }

    #[test]
    fn counts_pick_plural_forms() {
        assert_eq!(t!("home.members", count = 1), "1 member");
        assert_eq!(t!("home.members", count = 3), "3 members");
        assert_eq!(t!("home.members_count", count = 3), "Members — 3");
    }

    #[test]
    fn plural_rules() {
        assert_eq!(Lang::En.plural(1), "one");
        assert_eq!(Lang::En.plural(0), "other");
        assert_eq!(Lang::De.plural(2), "other");
        assert_eq!(Lang::Nl.plural(1), "one");
        assert_eq!(Lang::Ja.plural(1), "other");
    }

    #[test]
    fn placeholders_fill_by_name() {
        let args: [(&str, &dyn Display); 2] = [("name", &"Ann"), ("count", &3)];
        assert_eq!(fill("{name} has {count}", &args), "Ann has 3");
        assert_eq!(fill("{count}件 {name}", &args), "3件 Ann");
        assert_eq!(fill("{missing} {", &args), "{missing} {");
    }

    #[test]
    fn language_choice_falls_back() {
        assert_eq!(Lang::from_locale("de-AT"), Some(Lang::De));
        assert_eq!(Lang::from_locale("nl_NL.UTF-8"), Some(Lang::Nl));
        assert_eq!(Lang::from_locale("ja"), Some(Lang::Ja));
        assert_eq!(Lang::from_locale("fr-FR"), None);
        assert_eq!(pick(["fr-FR".into(), "nl-BE".into()]), Lang::Nl);
        assert_eq!(pick(["fr-FR".to_owned()]), Lang::En);
        assert_eq!(pick(Vec::<String>::new()), Lang::En);
        // A key missing from a translation shows the English text, never nothing.
        let tables = [
            parse("hello = Hello\nbye = Bye"),
            parse("hello = Hallo"),
            parse(""),
            parse(""),
        ];
        assert_eq!(find(&tables, Lang::De, "hello"), Some("Hallo"));
        assert_eq!(find(&tables, Lang::De, "bye"), Some("Bye"));
        assert_eq!(find(&tables, Lang::Ja, "bye"), Some("Bye"));
        assert_eq!(find(&tables, Lang::De, "missing"), None);
    }
}
