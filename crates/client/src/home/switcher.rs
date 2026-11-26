//! The quick switcher: Ctrl+K (Cmd+K on macOS), or the search button on phones,
//! opens one box that finds people, channels and Settings pages and jumps there.
//! It searches only what this device already has; nothing is asked of a server.
//! Each kind of result is one `Kind`: a new kind is a variant, its `entries` and,
//! in `picture`, how its rows look.

use std::collections::{HashSet, VecDeque};

use eframe::egui::{self, Align2, FontId, Key, Label, Modifiers, Rect, Ui, Vec2, accesskit::Role};

use super::{Home, Tab, View, avatar, channel_icon, face, paint_hovered};
use crate::{friends::Relation, server::Id, ui};

/// Recent picks kept, shown while the box is empty.
const RECENT: usize = 5;
/// Results per kind, so one kind can't push the others out of sight.
const PER_KIND: usize = 6;
const ROW: f32 = 44.0;
const PICTURE: f32 = 28.0;

#[derive(Default)]
pub struct Switcher {
    pub(super) open: bool,
    /// Just opened: the search box takes the keyboard.
    opening: bool,
    query: String,
    /// The highlighted result, counted over all groups.
    selected: usize,
    /// Newest first; this session only.
    recent: VecDeque<Target>,
}

impl Switcher {
    pub fn open(&mut self) {
        *self = Self {
            open: true,
            opening: true,
            recent: std::mem::take(&mut self.recent),
            ..Self::default()
        };
    }
}

/// Where a result goes.
#[derive(Clone, PartialEq)]
pub enum Target {
    /// The DM with them, started when there is none.
    Person(Id),
    /// A channel on the server at this address.
    Channel(url::Url, Id),
    /// A Settings page, by its name or the name of one of its options.
    Setting(Tab, &'static str),
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    People,
    Channels,
    Settings,
}

impl Kind {
    const ALL: [Self; 3] = [Self::People, Self::Channels, Self::Settings];

    fn title(self) -> &'static str {
        match self {
            Self::People => t!("switcher.people"),
            Self::Channels => t!("switcher.channels"),
            Self::Settings => t!("settings.title"),
        }
    }

    /// Everything of this kind, in the order to show it before any typing.
    fn entries(self, home: &Home) -> Vec<Entry> {
        match self {
            Self::People => people(home),
            Self::Channels => channels(home),
            Self::Settings => settings(),
        }
    }
}

#[derive(Clone)]
struct Entry {
    target: Target,
    label: String,
    /// Under the label: shared servers, the server, or the Settings page.
    detail: String,
    /// The glyph before a channel's name, or a Settings page's icon.
    icon: &'static str,
}

/// A titled group of results.
struct Group {
    title: &'static str,
    entries: Vec<Entry>,
}

/// People with a DM first, then the members of every server, once each; never
/// this person, nor anyone they blocked.
fn people(home: &Home) -> Vec<Entry> {
    let me = *home.identity.certificate.person_id();
    let (dms, requests) = home.friends.split(&home.dm_peers(), home.friends_only());
    let mut seen: HashSet<Id> = home.servers.iter().map(|s| s.dir.me).collect();
    seen.insert(me);
    let mut people: Vec<Id> = (dms.into_iter().chain(requests))
        .filter(|person| seen.insert(*person))
        .collect();
    for server in &home.servers {
        for member in server.dir.members.iter() {
            if home.friends.relation(&member.person) != Relation::Blocked
                && seen.insert(member.person)
            {
                people.push(member.person);
            }
        }
    }
    (people.into_iter())
        .map(|person| Entry {
            label: home.peer_name(&person),
            detail: home.shared_names(&person),
            target: Target::Person(person),
            icon: "",
        })
        .collect()
}

/// Every channel of every server, as its channel list shows them: the server only
/// sends the channels this person may read.
fn channels(home: &Home) -> Vec<Entry> {
    let mut entries = Vec::new();
    for server in &home.servers {
        let channels = server.dir.categories.iter().flat_map(|c| &c.channels);
        for channel in channels {
            entries.push(Entry {
                target: Target::Channel(server.address.clone(), channel.id),
                label: channel.name.clone(),
                detail: server.name().to_owned(),
                icon: channel_icon(channel),
            });
        }
    }
    entries
}

/// The pages of Settings, then their main options; each opens its page.
fn settings() -> Vec<Entry> {
    let groups = crate::settings::groups(false);
    let pages: Vec<_> = groups.iter().flat_map(|group| &group.pages).collect();
    let mut entries: Vec<Entry> = (pages.iter())
        .map(|page| Entry {
            target: Target::Setting(page.id, page.label),
            label: page.label.to_owned(),
            detail: t!("settings.title").to_owned(),
            icon: page.icon,
        })
        .collect();
    for (tab, label) in options() {
        if let Some(page) = pages.iter().find(|page| page.id == tab) {
            entries.push(Entry {
                target: Target::Setting(tab, label),
                label: label.to_owned(),
                detail: page.label.to_owned(),
                icon: page.icon,
            });
        }
    }
    entries
}

/// The main options of each Settings page, by their section titles.
fn options() -> Vec<(Tab, &'static str)> {
    vec![
        (Tab::Profile, t!("settings.profile")),
        (Tab::Profile, t!("settings.your_id")),
        (Tab::Privacy, t!("keys.section")),
        (Tab::Privacy, t!("home.direct_messages")),
        (Tab::Privacy, t!("friends.blocked_section")),
        (Tab::Privacy, t!("home.member_lists")),
        (Tab::Privacy, t!("previews.section")),
        (Tab::Privacy, t!("home.server_directory")),
        (Tab::Devices, t!("settings.this_device")),
        (Tab::Devices, t!("settings.other_devices")),
        (Tab::Appearance, t!("settings.language")),
        (Tab::Appearance, t!("settings.mode")),
        (Tab::Appearance, t!("settings.accent")),
        (Tab::Appearance, t!("settings.motion")),
        (Tab::Voice, t!("call.devices")),
        (Tab::Voice, t!("call.input_mode")),
        (Tab::Voice, t!("call.push_to_talk")),
        (Tab::Voice, t!("call.voice_processing")),
        (Tab::Video, t!("camera.camera")),
        (Tab::Shortcuts, t!("hotkeys.section")),
        (Tab::Network, t!("route.connection_section")),
        (Tab::Notifications, t!("settings.desktop_notifications")),
        (Tab::Notifications, t!("sounds.section")),
    ]
}

/// How well `query` matches `text`, ignoring case and the spaces typed; `None`
/// when it doesn't. The query in one piece beats one spread over the text; within
/// each, at the start beats at a word start beats anywhere, and earlier and
/// tighter beats later and looser.
fn score(query: &str, text: &str) -> Option<u32> {
    let query: Vec<char> = (query.chars().flat_map(char::to_lowercase))
        .filter(|c| !c.is_whitespace())
        .collect();
    if query.is_empty() {
        return Some(0);
    }
    let text: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let word_start = |at: usize| at == 0 || !text[at - 1].is_alphanumeric();
    if let Some(at) = text.windows(query.len()).position(|piece| piece == query) {
        let exact = if text.len() == query.len() { 400 } else { 0 };
        let start = if at == 0 {
            300
        } else if word_start(at) {
            200
        } else {
            0
        };
        return Some(10_000 + exact + start - at.min(99) as u32);
    }
    // Each typed letter in order: letters that follow each other or start a word
    // count more. A jump to a word start is free ("vp" for "voice processing");
    // other gaps count against it, and too many make it no match at all, so a few
    // letters don't match every long text that happens to contain them.
    let mut total: i64 = 0;
    let mut gaps = 0;
    let mut last: Option<usize> = None;
    for wanted in &query {
        let from = last.map_or(0, |at| at + 1);
        let at = from + text[from..].iter().position(|c| c == wanted)?;
        total += 10;
        if word_start(at) {
            total += 15;
        } else if let Some(before) = last {
            if before + 1 == at {
                total += 20;
            } else {
                gaps += at - before - 1;
            }
        } else {
            gaps += at.min(20);
        }
        last = Some(at);
    }
    if gaps > 2 * query.len() {
        return None;
    }
    Some((total - gaps as i64).clamp(1, 9_999) as u32)
}

/// What to show for `query`: with an empty box the recent picks, then the start of
/// each kind; else the matches of each kind, the kind with the best match first.
fn rank(kinds: Vec<(Kind, Vec<Entry>)>, query: &str, recent: &VecDeque<Target>) -> Vec<Group> {
    if query.trim().is_empty() {
        let all: Vec<&Entry> = kinds.iter().flat_map(|(_, entries)| entries).collect();
        let recent: Vec<Entry> = (recent.iter())
            .filter_map(|target| all.iter().find(|entry| entry.target == *target))
            .map(|entry| (*entry).clone())
            .collect();
        let mut groups = Vec::new();
        for (kind, entries) in &kinds {
            let entries: Vec<Entry> = (entries.iter())
                .filter(|entry| !recent.iter().any(|r| r.target == entry.target))
                .take(PER_KIND)
                .cloned()
                .collect();
            groups.push(Group {
                title: kind.title(),
                entries,
            });
        }
        if !recent.is_empty() {
            groups.insert(
                0,
                Group {
                    title: t!("switcher.recent"),
                    entries: recent,
                },
            );
        }
        groups.retain(|group| !group.entries.is_empty());
        return groups;
    }
    let mut ranked: Vec<(u32, Group)> = Vec::new();
    for (kind, entries) in kinds {
        let mut found: Vec<(u32, Entry)> = (entries.into_iter())
            .filter_map(|entry| {
                // The label counts most; a match that needs the detail (the server,
                // the page) counts half.
                let both = format!("{} {}", entry.label, entry.detail);
                let score =
                    score(query, &entry.label).max(score(query, &both).map(|score| score / 2))?;
                Some((score, entry))
            })
            .collect();
        // Stable: equal matches keep their order, people with a DM first.
        found.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        found.truncate(PER_KIND);
        if let Some(best) = found.first().map(|(score, _)| *score) {
            let group = Group {
                title: kind.title(),
                entries: found.into_iter().map(|(_, entry)| entry).collect(),
            };
            ranked.push((best, group));
        }
    }
    ranked.sort_by_key(|(best, _)| std::cmp::Reverse(*best));
    ranked.into_iter().map(|(_, group)| group).collect()
}

fn results(home: &Home, query: &str, recent: &VecDeque<Target>) -> Vec<Group> {
    let kinds = Kind::ALL.map(|kind| (kind, kind.entries(home)));
    rank(kinds.into(), query, recent)
}

/// Ctrl+K, or Cmd+K on macOS: opens the switcher, or closes it again. Taken before
/// anything else sees it, so it works while typing a message too. The Settings
/// windows cover the app; it waits until they close.
pub(super) fn shortcut(ctx: &egui::Context, home: &mut Home) {
    if !ctx.input_mut(|input| input.consume_key(Modifiers::COMMAND, Key::K)) {
        return;
    }
    if home.switcher.open {
        home.switcher.open = false;
    } else if home.settings.is_none() && home.server_settings.is_none() {
        home.switcher.open();
    }
}

/// The switcher, when open: the search box, then the results by kind. Arrows move,
/// Enter or a click opens, Escape closes.
pub(super) fn show(ctx: &egui::Context, home: &mut Home) {
    if !home.switcher.open {
        return;
    }
    let now = ctx.input(|input| input.time);
    // Taken from the search box first, which would move its cursor or let go of it.
    let (down, up, enter) = ctx.input_mut(|input| {
        let down = input.count_and_consume_key(Modifiers::NONE, Key::ArrowDown);
        let up = input.count_and_consume_key(Modifiers::NONE, Key::ArrowUp);
        (down, up, input.consume_key(Modifiers::NONE, Key::Enter))
    });
    let mut switcher = std::mem::take(&mut home.switcher);
    let mut picked = None;
    let ((), close) = ui::dialog(ctx, "switcher", t!("switcher.title"), |ui| {
        let field = ui.add(
            ui::field(&mut switcher.query)
                .hint_text(t!("switcher.hint"))
                .desired_width(f32::INFINITY),
        );
        field.widget_info(|| {
            egui::WidgetInfo::labeled(Role::SearchInput, true, t!("switcher.title"))
        });
        if std::mem::take(&mut switcher.opening) {
            field.request_focus();
        }
        if field.changed() {
            switcher.selected = 0;
        }
        let groups = results(home, &switcher.query, &switcher.recent);
        let count: usize = groups.iter().map(|group| group.entries.len()).sum();
        if count > 0 {
            switcher.selected = (switcher.selected + count * up + down - up) % count;
        }
        if enter {
            picked = (groups.iter().flat_map(|group| &group.entries))
                .nth(switcher.selected)
                .map(|entry| entry.target.clone());
        }
        ui.add_space(ui::SPACE_S);
        if count == 0 {
            ui.weak(t!("settings.nothing_found"));
        }
        let moved = down + up > 0;
        let mut at = 0;
        for group in &groups {
            ui.add_space(ui::SPACE_XS);
            ui.horizontal(|ui| {
                ui.add_space(6.0);
                ui::caption_label(ui, group.title);
            });
            for entry in &group.entries {
                let selected = at == switcher.selected;
                if selected {
                    let rect =
                        Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), ROW));
                    ui.painter()
                        .rect_filled(rect, ui::RADIUS_ROW, ui::colors(ui).selected);
                }
                let row = ui::row_button(ui, ROW, &entry.label, |ui| {
                    picture(ui, home, entry);
                    ui.add_space(ui::SPACE_XS);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        let label = match entry.target {
                            Target::Channel(..) => format!("{} {}", entry.icon, entry.label),
                            _ => entry.label.clone(),
                        };
                        ui.add(Label::new(ui::bold(label)).truncate());
                        ui.add(Label::new(ui::caption(&entry.detail)).truncate());
                    });
                });
                if selected && moved {
                    row.scroll_to_me(None);
                }
                if row.clicked() {
                    picked = Some(entry.target.clone());
                }
                at += 1;
            }
        }
    });
    if close || picked.is_some() {
        switcher.open = false;
    }
    home.switcher = switcher;
    if let Some(target) = picked {
        let recent = &mut home.switcher.recent;
        recent.retain(|old| *old != target);
        recent.push_front(target.clone());
        recent.truncate(RECENT);
        go(home, &target, now);
    }
}

/// The picture before a result: the person's, the channel's server icon, or the
/// Settings page's icon.
fn picture(ui: &mut Ui, home: &Home, entry: &Entry) {
    match &entry.target {
        Target::Person(person) => {
            let none = crate::server::Directory::default();
            let dir = (home.dm_server(person)).map_or(&none, |index| &home.servers[index].dir);
            face(ui, dir, person, &entry.label, PICTURE, false);
        }
        Target::Channel(address, _) => {
            let Some(server) = home.servers.iter().find(|s| s.address == *address) else {
                return;
            };
            match &server.dir.icon {
                Some(icon) => {
                    let (rect, _) =
                        ui.allocate_exact_size(Vec2::splat(PICTURE), egui::Sense::hover());
                    paint_hovered(ui, rect, icon, f32::from(ui::RADIUS));
                }
                None => {
                    // A colour of its own per server, like a person's initials.
                    let seed = address.as_str().bytes().fold(0u8, u8::wrapping_add);
                    avatar(ui, &[seed; 32], server.name(), PICTURE);
                }
            }
        }
        Target::Setting(..) => {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(PICTURE), egui::Sense::hover());
            let c = ui::colors(ui);
            ui.painter().rect_filled(rect, ui::RADIUS, c.side);
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                entry.icon,
                FontId::proportional(ui::TEXT_ICON),
                c.text,
            );
        }
    }
}

/// Open what was picked: the DM (started on a shared server when there is none
/// yet), the channel on its server, or the Settings page.
fn go(home: &mut Home, target: &Target, now: f64) {
    match target {
        Target::Person(person) => home.open_dm(*person, now),
        Target::Channel(address, channel) => {
            let Some(index) = home.index(address) else {
                return;
            };
            if home.servers[index].channel(channel).is_some() {
                home.servers[index].open = Some(*channel);
                home.view = View::Server(index);
                home.layout.list = false;
            }
        }
        Target::Setting(tab, _) => home.settings = Some(*tab),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(target: Target, label: &str, detail: &str) -> Entry {
        Entry {
            target,
            label: label.into(),
            detail: detail.into(),
            icon: "",
        }
    }

    #[test]
    fn matches_rank_whole_words_before_scattered_letters() {
        assert_eq!(score("", "anything"), Some(0));
        assert_eq!(score("xyz", "general"), None);
        assert_eq!(score("lareneg", "general"), None);
        // Exact, then at the start, then at a word start, then inside a word.
        let exact = score("general", "General").unwrap();
        let start = score("gen", "general").unwrap();
        let word = score("gen", "off general").unwrap();
        let inside = score("ner", "general").unwrap();
        let scattered = score("gnl", "general").unwrap();
        assert!(exact > start && start > word && word > inside && inside > scattered);
        // Spaces typed don't matter; case in other scripts and Japanese still match.
        assert!(score("Voi ce", "voice").is_some());
        assert!(score("ÉT", "été").is_some());
        assert!(score("チャン", "チャンネル").is_some());
        // Letters at word starts beat the same letters scattered inside words.
        assert!(score("vp", "voice processing") > score("vp", "avoid lamp"));
    }

    #[test]
    fn results_group_by_kind_best_kind_first() {
        let person = |n: u8, name: &str| entry(Target::Person([n; 32]), name, "Camp");
        let channel = |n: u8, name: &str| {
            let address: url::Url = "https://camp.example.org/".parse().unwrap();
            entry(Target::Channel(address, [n; 32]), name, "Camp")
        };
        let voice = || entry(Target::Setting(Tab::Voice, "Voice"), "Voice", "Settings");
        let kinds = || {
            vec![
                (
                    Kind::People,
                    vec![person(1, "Ann"), person(2, "General Lee")],
                ),
                (
                    Kind::Channels,
                    vec![channel(3, "general"), channel(4, "games")],
                ),
                (Kind::Settings, vec![voice()]),
            ]
        };
        let titles = |groups: &[Group]| groups.iter().map(|g| g.title).collect::<Vec<_>>();
        let labels = |group: &Group| {
            (group.entries.iter())
                .map(|e| e.label.clone())
                .collect::<Vec<_>>()
        };

        // An exact channel name puts channels before people.
        let groups = rank(kinds(), "general", &VecDeque::new());
        assert_eq!(
            titles(&groups),
            [Kind::Channels.title(), Kind::People.title()]
        );
        assert_eq!(labels(&groups[0]), ["general"]);
        assert_eq!(labels(&groups[1]), ["General Lee"]);

        // Better matches first within a kind; the detail (the server) counts too.
        let groups = rank(kinds(), "ga", &VecDeque::new());
        assert_eq!(labels(&groups[0]), ["games", "general"]);
        let groups = rank(kinds(), "camp", &VecDeque::new());
        assert_eq!(
            titles(&groups),
            [Kind::People.title(), Kind::Channels.title()]
        );
        assert!(rank(kinds(), "zzz", &VecDeque::new()).is_empty());

        // An empty box: recent picks on top, not again below; the rest in order.
        let recent = VecDeque::from([voice().target, Target::Person([2; 32])]);
        let groups = rank(kinds(), " ", &recent);
        let expected = [
            t!("switcher.recent"),
            Kind::People.title(),
            Kind::Channels.title(),
        ];
        assert_eq!(titles(&groups), expected);
        assert_eq!(labels(&groups[0]), ["Voice", "General Lee"]);
        assert_eq!(labels(&groups[1]), ["Ann"]);
    }
}
