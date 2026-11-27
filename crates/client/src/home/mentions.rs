//! Mentions are resolved from verified, decrypted text and the current directory.

use std::{borrow::Cow, ops::Range, sync::Arc};

use eframe::egui::{self, CompletionQuery, Suggestion, Ui, text::LayoutJob};
use rusttalk_core::auth::{decode, encode};

use crate::{
    emoji,
    server::{Body, Directory, Id, MENTION_EVERYONE, Member, Msg},
    ui,
};

use super::markdown::{self, Document};

const BROADCASTS: [&str; 2] = ["@everyone", "@here"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mention<'a> {
    Person(Id),
    Role(&'a str),
    Broadcast(&'a str),
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '@')
}

fn starts_at(text: &str, offset: usize) -> bool {
    !text[..offset].ends_with(|c| word_char(c) || c == '<')
}

fn ends_at(text: &str, offset: usize) -> bool {
    !text[offset..].starts_with(word_char)
}

fn tokens(text: &str) -> impl Iterator<Item = (Range<usize>, Mention<'_>)> {
    let mut covered = 0;
    text.match_indices(['<', '@'])
        .filter_map(move |(start, _)| {
            if start < covered {
                return None;
            }
            let rest = &text[start..];
            let (len, mention) = if let Some(tag) = rest.strip_prefix("<@") {
                // A person ID is the longest identifier accepted in an angle token.
                let end = tag.bytes().take(44).position(|c| c == b'>')?;
                let id = &tag[..end];
                let mention = if let Some(role) = id.strip_prefix('&') {
                    if !matches!(role, "owner" | "admin" | "moderator" | "member")
                        && decode::<16>(role).is_err()
                    {
                        return None;
                    }
                    Mention::Role(role)
                } else {
                    Mention::Person(decode(id).ok()?)
                };
                (end + 3, mention)
            } else {
                let word = BROADCASTS.into_iter().find(|word| {
                    rest.starts_with(word)
                        && starts_at(text, start)
                        && ends_at(text, start + word.len())
                })?;
                (word.len(), Mention::Broadcast(word))
            };
            covered = start + len;
            Some((start..covered, mention))
        })
}

impl Mention<'_> {
    fn allowed(self, wide: bool) -> bool {
        matches!(self, Self::Person(_)) || wide
    }

    fn label(self, dir: &Directory) -> String {
        let name = match self {
            Self::Person(id) => (dir.members.get(&id)).and_then(|member| member.name.as_deref()),
            Self::Role(id) => dir
                .roles
                .iter()
                .find(|role| role.id == id && role.name_known)
                .map(|role| role.name.as_str()),
            Self::Broadcast(word) => return word.into(),
        };
        format!(
            "@{}",
            name.filter(|name| !name.is_empty())
                .unwrap_or(t!("mentions.unknown"))
        )
    }
}

pub fn can_mention_everyone(dir: &Directory, channel: Id, person: Id) -> bool {
    !dir.dms.iter().any(|dm| dm.channel == channel)
        && (dir.members.get(&person)).is_some_and(|member| {
            dir.roles.iter().any(|role| {
                member.roles.contains(&role.id)
                    && role.permissions.iter().any(|p| p == MENTION_EVERYONE)
            })
        })
}

pub(super) fn allowed_to_send(doc: &Document, wide: bool) -> bool {
    wide || doc
        .prose()
        .flat_map(tokens)
        .all(|(_, mention)| mention.allowed(false))
}

pub struct Message {
    pub(super) doc: Arc<Document>,
    wide: bool,
    /// Emoji of other servers the message carries as files: name and file blob ID.
    inline: Vec<(String, Id)>,
}

impl Message {
    pub fn new(msg: &Msg, dir: &Directory, channel: Id) -> Self {
        Self::from_msg(msg, dir, channel, |s| Arc::new(Document::parse(s)))
    }

    pub(super) fn cached(ui: &Ui, msg: &Msg, dir: &Directory, channel: Id) -> Self {
        Self::from_msg(msg, dir, channel, |s| markdown::cached(ui, s))
    }

    fn from_msg(
        msg: &Msg,
        dir: &Directory,
        channel: Id,
        parse: impl FnOnce(&str) -> Arc<Document>,
    ) -> Self {
        let doc = parse(match &msg.body {
            Body::Text(s) => s,
            _ => "",
        });
        let wide = !allowed_to_send(&doc, false) && can_mention_everyone(dir, channel, msg.person);
        let inline = (msg.files.iter())
            .filter_map(|file| match crate::sticker_files::of(file) {
                Some((crate::sticker_files::Kind::Emoji, name)) => Some((name, file.file.id)),
                _ => None,
            })
            .collect();
        Self { doc, wide, inline }
    }

    /// The picture of a `:name:` emoji from another server that this message carries.
    pub(super) fn inline_picture(
        &self,
        ctx: &eframe::egui::Context,
        name: &str,
    ) -> Option<crate::server::Picture> {
        let (_, blob) = (self.inline.iter()).find(|(n, _)| n.eq_ignore_ascii_case(name))?;
        crate::sticker_files::picture(ctx, blob)
    }

    pub(super) fn parts<'a>(&self, dir: &Directory, text: &'a str) -> Vec<(Cow<'a, str>, bool)> {
        let mut parts = Vec::new();
        let mut start = 0;
        for (range, mention) in tokens(text) {
            if start < range.start {
                parts.push((Cow::Borrowed(&text[start..range.start]), false));
            }
            parts.push((Cow::Owned(mention.label(dir)), mention.allowed(self.wide)));
            start = range.end;
        }
        parts.push((Cow::Borrowed(&text[start..]), false));
        parts
    }

    pub(super) fn resolve(&self, dir: &Directory, text: &str) -> String {
        self.parts(dir, text)
            .into_iter()
            .map(|(s, _)| emoji::plain(&s).into_owned())
            .collect()
    }

    pub fn plain(&self, dir: &Directory) -> String {
        self.doc.plain(|s| self.resolve(dir, s))
    }
}

/// Shared with notifications so they apply the same rule as the chat highlight.
pub fn mentions_me(message: &Message, me: &Id, my_roles: &[String]) -> bool {
    message.doc.prose().flat_map(tokens).any(|(_, mention)| {
        mention.allowed(message.wide)
            && match mention {
                Mention::Person(person) => person == *me,
                Mention::Role(role) => my_roles.iter().any(|id| id == role),
                Mention::Broadcast(_) => true,
            }
    })
}

/// Whether this person is named directly; muting @everyone never silences that.
pub fn names_me(message: &Message, me: &Id) -> bool {
    message
        .doc
        .prose()
        .flat_map(tokens)
        .any(|(_, mention)| mention == Mention::Person(*me))
}

/// The visible names chosen in one draft. Keep each alias bound to its ID for
/// the life of the draft, even if directory names change while it is open.
#[derive(Clone, Default)]
pub struct DraftMentions(Vec<(String, String)>);

impl DraftMentions {
    fn alias(&self, label: &str, token: &str, occupied: &[&str]) -> String {
        if let Some((alias, _)) = self.0.iter().find(|(_, value)| value == token) {
            return alias.clone();
        }
        let mut alias = label.to_owned();
        for suffix in 2.. {
            if !BROADCASTS.contains(&alias.as_str())
                && self.0.iter().all(|(used, _)| *used != alias)
                && occupied.iter().all(|text| {
                    !text.match_indices(&alias).any(|(start, _)| {
                        starts_at(text, start) && ends_at(text, start + alias.len())
                    })
                })
            {
                break;
            }
            alias = format!("{label} ({suffix})");
        }
        alias
    }

    fn remember(&mut self, alias: String, token: String) {
        if !self.0.iter().any(|(_, value)| *value == token) {
            self.0.push((alias, token));
        }
    }

    pub fn open(dir: &Directory, text: &str) -> (String, Self) {
        let mut aliases = Self::default();
        let draft = Document::parse(text).map_source(text, |prose| {
            let mut draft = String::new();
            let mut start = 0;
            for (range, mention) in tokens(prose) {
                let token = &prose[range.clone()];
                draft.push_str(&prose[start..range.start]);
                if !matches!(mention, Mention::Broadcast(_))
                    && starts_at(prose, range.start)
                    && ends_at(prose, range.end)
                {
                    let alias = aliases.alias(&alias_label(&mention.label(dir)), token, &[text]);
                    draft.push_str(&alias);
                    aliases.remember(alias, token.into());
                } else {
                    draft.push_str(token);
                }
                start = range.end;
            }
            draft.push_str(&prose[start..]);
            draft
        });
        (draft, aliases)
    }

    // A backtick in a selected display name is part of the alias. Mask it while
    // locating code, keeping the offsets in the original draft valid.
    pub(super) fn parser_input<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut source = Cow::Borrowed(text);
        if text.contains('`') {
            for (range, _) in self.ranges(text) {
                if text[range.clone()].contains('`') {
                    source
                        .to_mut()
                        .replace_range(range.clone(), &text[range].replace('`', "x"));
                }
            }
        }
        source
    }

    pub fn encode<'a>(&self, text: &'a str) -> Cow<'a, str> {
        if self.0.is_empty() {
            return Cow::Borrowed(text);
        }
        let mut encoded = String::new();
        let mut start = 0;
        for (range, token) in self.ranges(text) {
            encoded.push_str(&text[start..range.start]);
            encoded.push_str(token);
            start = range.end;
        }
        encoded.push_str(&text[start..]);
        Cow::Owned(encoded)
    }

    fn ranges<'a>(&'a self, text: &'a str) -> impl Iterator<Item = (Range<usize>, &'a str)> {
        let mut covered = 0;
        text.match_indices('@').filter_map(move |(offset, _)| {
            if offset < covered || !starts_at(text, offset) {
                return None;
            }
            let (alias, token) = self
                .0
                .iter()
                .filter(|(alias, _)| {
                    text[offset..].starts_with(alias) && ends_at(text, offset + alias.len())
                })
                .max_by_key(|(alias, _)| alias.len())?;
            covered = offset + alias.len();
            Some((offset..covered, token.as_str()))
        })
    }

    pub fn layout(&self, ui: &Ui, text: &str, width: f32, wide: bool) -> LayoutJob {
        let mut job = LayoutJob::default();
        job.wrap.max_width = width;
        let plain =
            egui::text::TextFormat::simple(emoji::font(ui::TEXT_BODY), ui.visuals().text_color());
        let chip = ui::mention_format(ui);
        let mut ranges: Vec<_> = self
            .ranges(text)
            .map(|(range, token)| (range, wide || !token.starts_with("<@&")))
            .collect();
        if wide {
            ranges.extend(tokens(text).filter_map(|(range, mention)| {
                matches!(mention, Mention::Broadcast(_)).then_some((range, true))
            }));
            ranges.sort_by_key(|(range, _)| range.start);
        }
        let mut start = 0;
        for (range, marked) in ranges {
            if range.start < start {
                continue;
            }
            job.append(&text[start..range.start], 0.0, plain.clone());
            job.append(
                &text[range.clone()],
                0.0,
                if marked { chip.clone() } else { plain.clone() },
            );
            start = range.end;
        }
        job.append(&text[start..], 0.0, plain);
        job
    }

    pub fn accepted(&mut self, inserted: &str, choices: Vec<Choice<'_>>) -> bool {
        let Some(choice) = choices
            .into_iter()
            .find(|choice| choice.insert() == inserted)
        else {
            return false;
        };
        if !BROADCASTS.contains(&choice.token.as_str()) {
            self.remember(choice.alias, choice.token);
        }
        true
    }
}

fn alias_label(label: &str) -> String {
    // Keep aliases on one line and prevent a name from mimicking wire syntax.
    label.replace(['\n', '\r', '<', '>'], " ")
}

pub struct Choice<'a> {
    alias: String,
    token: String,
    label: String,
    member: Option<&'a Member>,
    hint: Option<&'static str>,
}

impl Choice<'_> {
    fn insert(&self) -> String {
        format!("{} ", self.alias)
    }

    pub fn suggestion(&self) -> Suggestion {
        let label = emoji::text(&self.label);
        let mut suggestion = Suggestion::new(self.insert()).content(label.clone());
        if let Some(member) = self.member {
            let avatar = super::avatar_atom(member, &self.label[1..]);
            suggestion = suggestion.content((avatar, label));
        }
        if let Some(hint) = self.hint {
            suggestion = suggestion.description(hint);
        }
        suggestion
    }
}

pub fn choices<'a>(
    query: &CompletionQuery<'_>,
    dir: &'a Directory,
    channel: Id,
    wide: bool,
    aliases: &DraftMentions,
) -> Vec<Choice<'a>> {
    let Some(prefix) = query.word.strip_prefix('@') else {
        return Vec::new();
    };
    let before: String = query.text.chars().take(query.word_range.start.0).collect();
    if prefix.contains('@')
        || !starts_at(&before, before.len())
        || query
            .text
            .chars()
            .nth(query.word_range.end.0)
            .is_some_and(word_char)
    {
        return Vec::new();
    }
    let prefix = prefix.to_lowercase();
    let matches = |name: &str| name.to_lowercase().starts_with(&prefix);
    let dm = dir.dms.iter().find(|dm| dm.channel == channel);
    let mut choices = Vec::new();
    let mut aliases = aliases.clone();
    let tail: String = query.text.chars().skip(query.word_range.end.0).collect();
    let mut add = |label: String, token: String, member, hint| {
        let alias = if BROADCASTS.contains(&token.as_str()) {
            token.clone()
        } else {
            let alias = aliases.alias(&alias_label(&label), &token, &[&before, &tail]);
            aliases.remember(alias.clone(), token.clone());
            alias
        };
        choices.push(Choice {
            alias,
            token,
            label,
            member,
            hint,
        });
    };
    for member in &dir.members {
        if dm.is_some_and(|dm| member.person != dir.me && member.person != dm.peer) {
            continue;
        }
        let label = member.display_name();
        if matches(&label) {
            let token = format!("<@{}>", encode(&member.person));
            add(format!("@{label}"), token, Some(member), None);
        }
    }
    if wide {
        for role in &dir.roles {
            if matches(&role.name) {
                let token = format!("<@&{}>", role.id);
                let label = Mention::Role(&role.id).label(dir);
                add(label, token, None, Some(t!("mentions.role")));
            }
        }
        for word in BROADCASTS {
            if matches(&word[1..]) {
                add(word.into(), word.into(), None, None);
            }
        }
    }
    choices
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{Dm, Role};
    use egui::text::CharIndex;

    const ME: Id = [1; 32];
    const ADA: Id = [2; 32];
    const BOB: Id = [3; 32];
    const CHANNEL: Id = [4; 32];
    const DM: Id = [5; 32];

    fn directory() -> Directory {
        let member = |person, name: &str, role: &str| Member {
            person,
            name: Some(name.into()),
            roles: vec![role.into()],
            devices: vec![],
            avatar: None,
            presence: Default::default(),
            hide_offline: false,
            bio: None,
            voice_muted: false,
            timeout_until: None,
        };
        let role = |id: &str, name: &str, permissions| Role {
            id: id.into(),
            name: name.into(),
            name_known: true,
            rank: 0,
            permissions,
            custom: false,
            color: None,
            separate: false,
        };
        Directory {
            me: ME,
            members: vec![
                member(ADA, "Ada Lovelace", "admin"),
                member(BOB, "Bob", "member"),
                member(ME, "Jamie", "member"),
            ]
            .into(),
            roles: vec![
                role("admin", "Admins", vec![MENTION_EVERYONE.into()]),
                role("member", "Game Night", vec![]),
            ],
            dms: vec![Dm {
                channel: DM,
                peer: ADA,
            }],
            ..Default::default()
        }
    }

    fn msg(person: Id, text: impl Into<String>) -> Msg {
        Msg {
            backfilled: false,
            id: [0; 32],
            person,
            at: 0,
            reply_to: None,
            quoted_author: None,
            edit_of: None,
            body: Body::Text(text.into()),
            files: vec![],
            live: None,
            sticker: None,
            replicas: Vec::new(),
            copy_files: Vec::new(),
        }
    }

    fn person_token(person: Id) -> String {
        format!("<@{}>", encode(&person))
    }

    #[test]
    fn parses_only_valid_tokens_and_bounded_broadcasts() {
        let custom = encode(&[7; 16]);
        let me = person_token(ME);
        let text = format!(
            "🌻 {me}<@&{custom}> (@here), @everyone! hi@everyone @everyoneelse <@junk> <@&junk> <@here>"
        );
        let parsed: Vec<_> = tokens(&text)
            .map(|(range, token)| (&text[range], token))
            .collect();
        assert_eq!(
            parsed,
            [
                (me.as_str(), Mention::Person(ME)),
                (format!("<@&{custom}>").as_str(), Mention::Role(&custom)),
                ("@here", Mention::Broadcast("@here")),
                ("@everyone", Mention::Broadcast("@everyone")),
            ]
        );
    }

    #[test]
    fn rendering_resolves_current_names_and_unknowns() {
        let mut dir = directory();
        dir.members.edit()[0].name = Some("Ada Byron".into());
        dir.roles[1].name = "Friday Games".into();
        let msg = msg(
            ADA,
            format!(
                "Hi {}, <@&member>, {}, <@&owner>!",
                person_token(ADA),
                person_token([9; 32])
            ),
        );
        let message = Message::new(&msg, &dir, CHANNEL);
        assert_eq!(
            message.plain(&dir),
            "Hi @Ada Byron, @Friday Games, @unknown, @unknown!"
        );
        let chips: Vec<_> = message
            .parts(&dir, &message.doc.prose().collect::<String>())
            .into_iter()
            .filter_map(|(label, marked)| marked.then(|| label.into_owned()))
            .collect();
        assert_eq!(
            chips,
            ["@Ada Byron", "@Friday Games", "@unknown", "@unknown"]
        );
    }

    #[test]
    fn bold_mentions_ping_but_examples_in_code_do_not() {
        let dir = directory();
        let token = person_token(ME);
        let incoming = msg(ADA, format!("**{token}**"));
        let message = Message::new(&incoming, &dir, CHANNEL);
        assert!(names_me(&message, &ME));
        assert_eq!(message.plain(&dir), "@Jamie");
        let example = msg(
            ADA,
            format!("`{token}`\n\n```\n@everyone :wave: {token}\n```"),
        );
        let message = Message::new(&example, &dir, CHANNEL);
        assert!(!mentions_me(&message, &ME, &[]));
        assert!(allowed_to_send(&message.doc, false));
        assert!(message.plain(&dir).contains(&token));
    }

    #[test]
    fn selected_names_with_markdown_encode_only_outside_code() {
        let token = person_token(ME);
        for name in ["@*Star*", "@__init__", "@[", "@~~", "@Tick`", "@`ticks`"] {
            let mut aliases = DraftMentions::default();
            aliases.remember(name.to_owned(), token.clone());
            let draft =
                format!("**{name}** :wave: and ``{name} :wave:``\n\n```rust\n{name} :wave:\n```");
            let doc = Document::parse(&aliases.parser_input(&draft));
            let outgoing = doc.map_source(&draft, |raw| {
                emoji::replace(&aliases.encode(raw), &[]).into_owned()
            });
            assert_eq!(
                outgoing,
                format!("**{token}** 👋 and ``{name} :wave:``\n\n```rust\n{name} :wave:\n```")
            );
            let msg = msg(ADA, outgoing);
            assert!(names_me(&Message::new(&msg, &directory(), CHANNEL), &ME));
        }
    }
    #[test]
    fn permissions_gate_sending_and_receiving() {
        let mut dir = directory();
        let direct = person_token(ME);
        let can_send = |sender, channel, text| {
            allowed_to_send(
                &Document::parse(text),
                can_mention_everyone(&dir, channel, sender),
            )
        };
        for text in ["<@&member>", "@everyone", "@here"] {
            assert!(can_send(ADA, CHANNEL, text));
            assert!(!can_send(BOB, CHANNEL, text));
            assert!(!can_send(ADA, DM, text));
            assert!(!can_send([9; 32], CHANNEL, text));
            let incoming = msg(BOB, text);
            let message = Message::new(&incoming, &dir, CHANNEL);
            assert!(
                message
                    .parts(&dir, &message.doc.prose().collect::<String>())
                    .iter()
                    .all(|(_, marked)| !marked)
            );
            assert!(!mentions_me(&message, &ME, &["member".into()]));
        }
        assert!(can_send(BOB, CHANNEL, &direct));
        assert!(can_send(BOB, DM, &direct));
        let incoming = msg(ADA, "@here");
        assert!(!mentions_me(&Message::new(&incoming, &dir, DM), &ME, &[]));
        assert!(mentions_me(
            &Message::new(&incoming, &dir, CHANNEL),
            &ME,
            &[]
        ));
        dir.roles[0].permissions.clear();
        assert!(!mentions_me(
            &Message::new(&incoming, &dir, CHANNEL),
            &ME,
            &[]
        ));
    }

    #[test]
    fn mentions_me_matches_people_and_assigned_roles() {
        let dir = directory();
        let my_roles = ["member".into()];
        for (sender, text, expected) in [
            (BOB, person_token(ME), true),
            (BOB, person_token(ADA), false),
            (ADA, "<@&member>".into(), true),
            (ADA, "<@&admin>".into(), false),
            (ADA, "@everyone".into(), true),
            (ADA, "plain text".into(), false),
            (BOB, format!("@everyone {}", person_token(ME)), true),
        ] {
            let incoming = msg(sender, text);
            assert_eq!(
                mentions_me(&Message::new(&incoming, &dir, CHANNEL), &ME, &my_roles),
                expected
            );
        }
    }

    #[test]
    fn completion_orders_members_roles_and_broadcasts_and_filters() {
        let dir = directory();
        let aliases = DraftMentions::default();
        let labels = |word: &str, channel, sender| {
            let query = CompletionQuery {
                word,
                text: word,
                word_range: CharIndex(0)..CharIndex(word.chars().count()),
            };
            choices(
                &query,
                &dir,
                channel,
                can_mention_everyone(&dir, channel, sender),
                &aliases,
            )
            .into_iter()
            .map(|choice| choice.label)
            .collect::<Vec<_>>()
        };
        assert_eq!(
            labels("@", CHANNEL, ADA),
            [
                "@Ada Lovelace",
                "@Bob",
                "@Jamie",
                "@Admins",
                "@Game Night",
                "@everyone",
                "@here",
            ]
        );
        assert_eq!(labels("@aD", CHANNEL, ADA), ["@Ada Lovelace", "@Admins"]);
        assert_eq!(labels("@aD", CHANNEL, BOB), ["@Ada Lovelace"]);
        assert_eq!(labels("@", DM, ADA), ["@Ada Lovelace", "@Jamie"]);
        assert!(labels("hi@ada", CHANNEL, ADA).is_empty());
    }

    #[test]
    fn composer_keeps_selected_ids_with_duplicate_names_and_edits() {
        let mut dir = directory();
        let name = dir.members[0].name.clone();
        dir.members.edit()[1].name = name;
        let mut aliases = DraftMentions::default();
        let query = CompletionQuery {
            word: "@ada",
            text: "@ada",
            word_range: CharIndex(0)..CharIndex(4),
        };
        let options = choices(&query, &dir, CHANNEL, true, &aliases);
        let [first, second] = options.as_slice() else {
            panic!("expected both people")
        };
        let picked = second.insert();
        assert_ne!(first.insert(), picked);
        assert!(aliases.accepted(&picked, options));
        assert_eq!(aliases.encode(picked.trim()), person_token(BOB));

        let original = format!(
            "Literal @Ada Lovelace, two people: {}, {}.",
            person_token(ADA),
            person_token(BOB)
        );
        let (draft, aliases) = DraftMentions::open(&dir, &original);
        assert!(!draft.contains("<@"));
        assert_eq!(aliases.encode(&draft), original);
    }
}
