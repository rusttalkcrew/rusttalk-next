//! Unread marks, mention badges and desktop notifications (issue #67).
//!
//! What was read and the notification choices live in `notify.json` and sync to this
//! person's other devices with the other settings. Previews are decrypted on this device
//! and only handed to the system's notification service; nothing is sent anywhere else.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, Sender, channel},
};

use rusttalk_core::{auth::encode, client::save_json};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::{
    home::mentions::{self, Message},
    server::Id,
};

/// `muted_until` for "Until I turn it back on".
pub const FOREVER: i64 = i64::MAX;
/// The mute choices in the menus: label and length in seconds.
pub const MUTE_FOR: [(&str, i64); 6] = [
    ("For 15 minutes", 15 * 60),
    ("For 1 hour", 60 * 60),
    ("For 3 hours", 3 * 60 * 60),
    ("For 8 hours", 8 * 60 * 60),
    ("For 24 hours", 24 * 60 * 60),
    ("Until I turn it back on", FOREVER),
];
/// Synced settings keys.
const PREFS_KEY: &str = "notifications";
const READ_KEY: &str = "read";
pub const SYNCED: [&str; 2] = [PREFS_KEY, READ_KEY];
const PREVIEW_CHARS: usize = 120;

pub fn unix_now() -> i64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    now.map_or(0, |elapsed| elapsed.as_secs() as i64)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    All,
    Mentions,
    Nothing,
}

impl Level {
    pub const ALL: [(Self, &'static str); 3] = [
        (Self::All, "All messages"),
        (Self::Mentions, "Only @mentions"),
        (Self::Nothing, "Nothing"),
    ];
}

/// A server's or channel's own choices; unset fields follow the server, then the defaults.
#[derive(Clone, Default, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Rule {
    pub level: Option<Level>,
    pub suppress_everyone: bool,
    /// Unix seconds; `FOREVER` until turned back on.
    pub muted_until: Option<i64>,
}

impl Rule {
    pub fn muted(&self, now: i64) -> bool {
        self.muted_until.is_some_and(|until| until > now)
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub desktop: bool,
    pub sound: bool,
    pub unfocused_only: bool,
    /// By server address.
    pub servers: BTreeMap<String, Rule>,
    /// By channel ID (base64url), DMs included.
    pub channels: BTreeMap<String, Rule>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            desktop: true,
            sound: true,
            unfocused_only: false,
            servers: BTreeMap::new(),
            channels: BTreeMap::new(),
        }
    }
}

/// The choices that apply to one channel.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Effective {
    pub level: Level,
    pub muted: bool,
    pub suppress_everyone: bool,
}

impl Prefs {
    /// A DM follows only its own choices: muting a server doesn't mute its DMs.
    pub fn effective(&self, server: &Url, channel: &Id, dm: bool, now: i64) -> Effective {
        let none = Rule::default();
        let own = self.channels.get(&encode(channel)).unwrap_or(&none);
        let server = match dm {
            true => &none,
            false => self.servers.get(server.as_str()).unwrap_or(&none),
        };
        Effective {
            level: own.level.or(server.level).unwrap_or(Level::All),
            muted: own.muted(now) || server.muted(now),
            suppress_everyone: own.suppress_everyone || server.suppress_everyone,
        }
    }

    pub fn server(&self, server: &Url) -> Rule {
        self.servers
            .get(server.as_str())
            .cloned()
            .unwrap_or_default()
    }

    pub fn channel(&self, channel: &Id) -> Rule {
        self.channels
            .get(&encode(channel))
            .cloned()
            .unwrap_or_default()
    }

    /// Set a server's rule (`channel` `None`) or a channel's; defaults aren't stored.
    pub fn set(&mut self, server: &Url, channel: Option<&Id>, rule: Rule) {
        let (rules, key) = match channel {
            Some(channel) => (&mut self.channels, encode(channel)),
            None => (&mut self.servers, server.as_str().to_owned()),
        };
        if rule == Rule::default() {
            rules.remove(&key);
        } else {
            rules.insert(key, rule);
        }
    }
}

/// Who a message calls out.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mention {
    None,
    /// This person by name.
    Me,
    /// A permitted @everyone or @here, or one of this person's roles.
    Group,
}

/// How a message mentions this person, by the same rules as the chat highlight (#66).
pub fn mention(message: &Message<'_>, me: &Id, my_roles: &[String]) -> Mention {
    if mentions::names_me(message, me) {
        Mention::Me
    } else if mentions::mentions_me(message, me, my_roles) {
        Mention::Group
    } else {
        Mention::None
    }
}

/// A new message from someone else.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Arrival {
    pub dm: bool,
    pub mention: Mention,
}

/// Whether it adds to the mention badge.
pub fn counts(rule: Effective, arrival: Arrival) -> bool {
    let called = match arrival.mention {
        Mention::Me => true,
        Mention::Group => !rule.suppress_everyone,
        Mention::None => arrival.dm,
    };
    called && !rule.muted && rule.level != Level::Nothing
}

/// Whether to show a desktop notification. `viewing` is the channel on screen.
pub fn should_notify(
    prefs: &Prefs,
    rule: Effective,
    arrival: Arrival,
    focused: bool,
    viewing: bool,
) -> bool {
    if !prefs.desktop || rule.muted || (focused && (viewing || prefs.unfocused_only)) {
        return false;
    }
    match rule.level {
        Level::All => true,
        Level::Mentions => counts(rule, arrival),
        Level::Nothing => false,
    }
}

/// The "Mute" and "Notifications" submenus of a server (`channel` false) or channel.
/// Returns the changed rule.
pub fn rule_menu(m: &mut crate::ui::Menu, rule: &Rule, channel: bool, now: i64) -> Option<Rule> {
    use crate::ui::Item;
    let what = if channel { "channel" } else { "server" };
    let mut changed = None;
    if rule.muted(now) {
        let until = match rule.muted_until {
            Some(FOREVER) | None => "Until you turn it back on".to_owned(),
            Some(until) => {
                chrono::DateTime::from_timestamp(until, 0).map_or_else(String::new, |at| {
                    let at = at.with_timezone(&chrono::Local);
                    format!("Muted until {}", at.format("%H:%M"))
                })
            }
        };
        if m.add(Item::new("🔔", &format!("Unmute {what}")).hint(&until))
            .clicked()
        {
            changed = Some(Rule {
                muted_until: None,
                ..rule.clone()
            });
        }
    } else {
        m.submenu("🔕", &format!("Mute {what}"), |m| {
            for (label, seconds) in MUTE_FOR {
                if m.item("", label).clicked() {
                    changed = Some(Rule {
                        muted_until: Some(now.saturating_add(seconds)),
                        ..rule.clone()
                    });
                }
            }
        });
    }
    m.submenu("🔔", "Notifications", |m| {
        if channel
            && m.add(Item::new("", "Same as server").picked(rule.level.is_none()))
                .clicked()
        {
            changed = Some(Rule {
                level: None,
                ..rule.clone()
            });
        }
        let current = if channel {
            rule.level
        } else {
            Some(rule.level.unwrap_or(Level::All))
        };
        for (level, label) in Level::ALL {
            if m.add(Item::new("", label).picked(current == Some(level)))
                .clicked()
            {
                changed = Some(Rule {
                    level: Some(level),
                    ..rule.clone()
                });
            }
        }
        m.separator();
        let suppress = Item::new("", "Mute @everyone")
            .hint("And @here and @role")
            .checked(rule.suppress_everyone);
        if m.add(suppress).clicked() {
            changed = Some(Rule {
                suppress_everyone: !rule.suppress_everyone,
                ..rule.clone()
            });
        }
    });
    changed
}

/// Activity in one channel this session.
#[derive(Clone, Copy, Default)]
pub struct Activity {
    /// The newest message from someone else, unix seconds.
    pub latest: i64,
    pub mentions: u32,
}

/// A chat to open: server address and channel.
type Target = (Url, Id);

/// `notify.json`.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    prefs: Prefs,
    /// The newest message time read, by channel ID.
    read: BTreeMap<String, i64>,
    /// When this file was made: channels never opened count as read up to here, so
    /// old history doesn't light up every channel at once.
    since: i64,
}

pub struct Notify {
    path: PathBuf,
    data: PathBuf,
    saved: Saved,
    pub activity: HashMap<Id, Activity>,
    /// Notifications clicked, to open their channel.
    clicks: (Sender<Target>, Receiver<Target>),
    /// The system's notification service failed once; logged once.
    failed: bool,
}

impl Notify {
    pub fn open(data: &Path, now: i64) -> Self {
        let path = data.join("notify.json");
        let saved = match crate::load_json::<Option<Saved>>(&path) {
            Ok(Some(saved)) => saved,
            Ok(None) => Saved {
                since: now,
                ..Saved::default()
            },
            Err(error) => {
                tracing::warn!(error = %format_args!("{error:#}"), "notification settings unreadable, starting fresh");
                Saved {
                    since: now,
                    ..Saved::default()
                }
            }
        };
        Self {
            path,
            data: data.to_owned(),
            saved,
            activity: HashMap::new(),
            clicks: channel(),
            failed: false,
        }
    }

    pub fn prefs(&self) -> &Prefs {
        &self.saved.prefs
    }

    pub fn set_prefs(&mut self, prefs: Prefs) {
        if prefs != self.saved.prefs {
            self.saved.prefs = prefs;
            self.save();
        }
    }

    fn save(&self) {
        if let Err(error) = save_json(&self.data, &self.path, &self.saved) {
            tracing::warn!(error = %format_args!("{error:#}"), "saving notification settings failed");
        }
    }

    fn read_until(&self, channel: &Id) -> i64 {
        (self.saved.read.get(&encode(channel)).copied()).unwrap_or(self.saved.since)
    }

    /// Something new from someone else that wasn't read yet.
    pub fn unread(&self, channel: &Id) -> bool {
        (self.activity.get(channel)).is_some_and(|a| a.latest > self.read_until(channel))
    }

    pub fn mentions(&self, channel: &Id) -> u32 {
        self.activity.get(channel).map_or(0, |a| a.mentions)
    }

    /// Record a message from someone else, counting it as a mention when `mention`.
    pub fn arrived(&mut self, channel: Id, at: i64, mention: bool) {
        let read = self.read_until(&channel);
        let activity = self.activity.entry(channel).or_default();
        activity.latest = activity.latest.max(at);
        if mention && at > read {
            activity.mentions += 1;
        }
    }

    /// The channel is on screen: everything in it is read.
    pub fn mark_read(&mut self, channel: &Id) {
        let Some(activity) = self.activity.get_mut(channel) else {
            return;
        };
        activity.mentions = 0;
        let latest = activity.latest;
        if latest > self.read_until(channel) {
            self.saved.read.insert(encode(channel), latest);
            self.save();
        }
    }

    /// The synced values, keyed like the other synced settings.
    pub fn synced(&self) -> BTreeMap<String, Value> {
        let prefs = serde_json::to_value(&self.saved.prefs);
        let read = serde_json::to_value(&self.saved.read);
        BTreeMap::from([
            (
                PREFS_KEY.into(),
                prefs.expect("notification settings serialize"),
            ),
            (READ_KEY.into(), read.expect("read marks serialize")),
        ])
    }

    /// Take another device's choices; keep the newest read mark of each channel.
    pub fn adopt(&mut self, synced: &BTreeMap<String, Value>) {
        let mut changed = false;
        if let Some(prefs) = synced.get(PREFS_KEY).filter(|v| !v.is_null()) {
            match serde_json::from_value::<Prefs>(prefs.clone()) {
                Ok(prefs) => {
                    changed |= prefs != self.saved.prefs;
                    self.saved.prefs = prefs;
                }
                Err(error) => tracing::warn!(%error, "synced notification settings unreadable"),
            }
        }
        if let Some(read) = synced.get(READ_KEY).filter(|v| !v.is_null()) {
            match serde_json::from_value::<BTreeMap<String, i64>>(read.clone()) {
                Ok(read) => {
                    for (channel, at) in read {
                        let mine = self.saved.read.entry(channel).or_default();
                        if at > *mine {
                            *mine = at;
                            changed = true;
                        }
                    }
                }
                Err(error) => tracing::warn!(%error, "synced read marks unreadable"),
            }
        }
        if changed {
            self.save();
        }
    }

    /// A clicked notification's server and channel.
    pub fn clicked(&self) -> Option<Target> {
        self.clicks.1.try_recv().ok()
    }

    /// Show a desktop notification; a click on it comes back from `clicked`.
    pub fn show(&mut self, ctx: &eframe::egui::Context, title: &str, body: &str, target: Target) {
        let body: String = match body.chars().count() > PREVIEW_CHARS {
            true => body.chars().take(PREVIEW_CHARS).chain(['…']).collect(),
            false => body.to_owned(),
        };
        let sound = self.saved.prefs.sound;
        let clicks = self.clicks.0.clone();
        let ctx = ctx.clone();
        let (title, failed) = (title.to_owned(), self.failed);
        self.failed = true;
        // Showing talks to the system's service and waiting for a click blocks: own thread.
        let spawned = std::thread::Builder::new()
            .name("notification".into())
            .spawn(move || match popup(&title, &body, sound) {
                Ok(handle) => handle.wait_for_action(|action| {
                    if action != "__closed" {
                        let _ = clicks.send(target);
                        ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Focus);
                        ctx.request_repaint();
                    }
                }),
                Err(error) if !failed => {
                    tracing::warn!(%error, "showing a desktop notification failed");
                }
                Err(_) => {}
            });
        match spawned {
            // Logged once per session until one succeeds; then failures are logged again.
            Ok(_) => self.failed = failed,
            Err(error) => tracing::warn!(%error, "starting the notification thread failed"),
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn popup(
    title: &str,
    body: &str,
    sound: bool,
) -> notify_rust::error::Result<notify_rust::NotificationHandle> {
    use notify_rust::Hint;
    let mut note = notify_rust::Notification::new();
    note.appname("RustTalk")
        .summary(title)
        .body(body)
        .icon("rusttalk")
        .hint(Hint::DesktopEntry("rusttalk".into()))
        .action("default", "Open");
    if sound {
        note.hint(Hint::SoundName("message-new-instant".into()));
    } else {
        note.hint(Hint::SuppressSound(true));
    }
    note.show()
}

#[cfg(not(all(unix, not(target_os = "macos"))))]
fn popup(
    title: &str,
    body: &str,
    sound: bool,
) -> notify_rust::error::Result<notify_rust::NotificationHandle> {
    let mut note = notify_rust::Notification::new();
    note.appname("RustTalk").summary(title).body(body);
    if sound {
        note.sound_name("Default");
    }
    note.show()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;

    fn url() -> Url {
        "https://chat.example".parse().unwrap()
    }

    fn arrival(dm: bool, mention: Mention) -> Arrival {
        Arrival { dm, mention }
    }

    #[test]
    fn uses_the_mentions_from_chat() {
        use crate::server::{Body, Directory, Dm, MENTION_EVERYONE, Member, Msg, Role};
        let (me, ada, bob, channel, dm) = ([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
        let member = |person, role: &str| Member {
            person,
            name: Some("Jamie".into()),
            roles: vec![role.into()],
            devices: vec![],
            avatar: None,
            presence: Default::default(),
        };
        let role = |id: &str, permissions| Role {
            id: id.into(),
            name: id.into(),
            name_known: true,
            rank: 0,
            permissions,
            custom: false,
        };
        let dir = Directory {
            me,
            members: vec![
                member(ada, "admin"),
                member(bob, "member"),
                member(me, "member"),
            ],
            roles: vec![
                role("admin", vec![MENTION_EVERYONE.into()]),
                role("member", vec![]),
            ],
            dms: vec![Dm {
                channel: dm,
                peer: ada,
            }],
            ..Directory::default()
        };
        let roles = ["member".to_owned()];
        let check = |from, chat, text: String| {
            let msg = Msg {
                id: [0; 32],
                person: from,
                at: 0,
                reply_to: None,
                edit_of: None,
                body: Body::Text(text),
                files: vec![],
                live: None,
                sticker: None,
                replicas: Vec::new(),
            };
            mention(&Message::new(&msg, &dir, chat), &me, &roles)
        };
        let token = format!("<@{}>", encode(&me));
        assert_eq!(check(bob, channel, format!("hi {token}")), Mention::Me);
        assert_eq!(check(ada, dm, token.clone()), Mention::Me);
        for text in ["@everyone", "@here", "<@&member>"] {
            assert_eq!(check(ada, channel, text.into()), Mention::Group, "{text}");
            // Not permitted for Bob, and never in a DM.
            assert_eq!(check(bob, channel, text.into()), Mention::None, "{text}");
            assert_eq!(check(ada, dm, text.into()), Mention::None, "{text}");
        }
        // A direct mention wins, so muting @everyone doesn't hide it.
        assert_eq!(
            check(ada, channel, format!("@everyone {token}")),
            Mention::Me
        );
        assert_eq!(check(ada, channel, "<@&admin>".into()), Mention::None);
        // A plain name is no longer a mention.
        assert_eq!(check(ada, channel, "Jamie, dinner?".into()), Mention::None);
    }

    #[test]
    fn mutes_expire_and_channels_override_their_server() {
        let mut prefs = Prefs::default();
        let channel = [1; 32];
        prefs.set(
            &url(),
            None,
            Rule {
                level: Some(Level::Mentions),
                muted_until: Some(NOW + 60),
                ..Rule::default()
            },
        );
        let server = prefs.effective(&url(), &channel, false, NOW);
        assert_eq!((server.level, server.muted), (Level::Mentions, true));
        // The mute ends by itself; the level stays.
        let later = prefs.effective(&url(), &channel, false, NOW + 60);
        assert_eq!((later.level, later.muted), (Level::Mentions, false));
        // A DM on that server keeps its own choices.
        let dm = prefs.effective(&url(), &channel, true, NOW);
        assert_eq!((dm.level, dm.muted), (Level::All, false));

        prefs.set(
            &url(),
            Some(&channel),
            Rule {
                level: Some(Level::Nothing),
                suppress_everyone: true,
                muted_until: Some(FOREVER),
            },
        );
        let own = prefs.effective(&url(), &channel, false, i64::MAX - 1);
        assert_eq!(own.level, Level::Nothing);
        assert!(own.muted && own.suppress_everyone);
        // Back to the defaults: nothing is stored.
        prefs.set(&url(), Some(&channel), Rule::default());
        prefs.set(&url(), None, Rule::default());
        assert!(prefs.channels.is_empty() && prefs.servers.is_empty());
    }

    #[test]
    fn notifies_by_level_mute_focus_and_channel() {
        let prefs = Prefs::default();
        let rule = |level, muted| Effective {
            level,
            muted,
            suppress_everyone: true,
        };
        let plain = arrival(false, Mention::None);
        let me = arrival(false, Mention::Me);
        let everyone = arrival(false, Mention::Group);
        let dm = arrival(true, Mention::None);
        let all = rule(Level::All, false);
        // Level × mention, away from the window.
        assert!(should_notify(&prefs, all, plain, false, false));
        let mentions = rule(Level::Mentions, false);
        assert!(!should_notify(&prefs, mentions, plain, false, false));
        assert!(should_notify(&prefs, mentions, me, false, false));
        assert!(should_notify(&prefs, mentions, dm, false, false));
        assert!(!should_notify(&prefs, mentions, everyone, false, false));
        assert!(!should_notify(
            &prefs,
            rule(Level::Nothing, false),
            me,
            false,
            false
        ));
        // Muted wins over everything.
        assert!(!should_notify(
            &prefs,
            rule(Level::All, true),
            me,
            false,
            false
        ));
        // Focused: only for another channel than the one on screen.
        assert!(should_notify(&prefs, all, plain, true, false));
        assert!(!should_notify(&prefs, all, plain, true, true));
        assert!(should_notify(&prefs, all, plain, false, true));
        let quiet = Prefs {
            unfocused_only: true,
            ..Prefs::default()
        };
        assert!(!should_notify(&quiet, all, plain, true, false));
        assert!(should_notify(&quiet, all, plain, false, true));
        let off = Prefs {
            desktop: false,
            ..Prefs::default()
        };
        assert!(!should_notify(&off, all, me, false, false));

        // Badges follow the same choices, focus aside.
        assert!(counts(all, me) && counts(all, dm) && !counts(all, plain));
        assert!(!counts(all, everyone));
        let loud = Effective {
            suppress_everyone: false,
            ..all
        };
        assert!(counts(loud, everyone));
        assert!(!counts(rule(Level::All, true), me));
        assert!(!counts(rule(Level::Nothing, false), dm));
    }

    #[test]
    fn tracks_unread_and_mentions_until_read() {
        let dir = tempfile::tempdir().unwrap();
        let mut notify = Notify::open(dir.path(), NOW);
        let channel = [2; 32];
        // History from before this file existed counts as read.
        notify.arrived(channel, NOW - 10, true);
        assert!(!notify.unread(&channel));
        assert_eq!(notify.mentions(&channel), 0);

        notify.arrived(channel, NOW + 5, false);
        notify.arrived(channel, NOW + 6, true);
        notify.arrived(channel, NOW + 7, true);
        assert!(notify.unread(&channel));
        assert_eq!(notify.mentions(&channel), 2);
        notify.mark_read(&channel);
        assert!(!notify.unread(&channel));
        assert_eq!(notify.mentions(&channel), 0);
        // A late page of older messages doesn't light it up again.
        notify.arrived(channel, NOW + 6, true);
        assert!(!notify.unread(&channel));
        assert_eq!(notify.mentions(&channel), 0);

        // The read mark survives a restart.
        let mut again = Notify::open(dir.path(), NOW + 100);
        again.arrived(channel, NOW + 7, false);
        assert!(!again.unread(&channel));
        again.arrived(channel, NOW + 8, false);
        assert!(again.unread(&channel));
    }

    #[test]
    fn choices_persist_and_sync_between_devices() {
        let (one, two) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut laptop = Notify::open(one.path(), NOW);
        let mut prefs = laptop.prefs().clone();
        prefs.sound = false;
        prefs.set(
            &url(),
            None,
            Rule {
                muted_until: Some(FOREVER),
                ..Rule::default()
            },
        );
        laptop.set_prefs(prefs.clone());
        let read = [3; 32];
        laptop.arrived(read, NOW + 50, false);
        laptop.mark_read(&read);
        assert!(Notify::open(one.path(), NOW).prefs() == &prefs);

        // The desktop read further in one channel and less in another.
        let mut desktop = Notify::open(two.path(), NOW);
        let other = [4; 32];
        desktop.arrived(other, NOW + 20, false);
        desktop.mark_read(&other);
        desktop.arrived(read, NOW + 10, false);
        desktop.mark_read(&read);
        let synced = laptop.synced();
        assert_eq!(
            synced.keys().map(String::as_str).collect::<Vec<_>>(),
            SYNCED
        );
        desktop.adopt(&synced);
        assert!(desktop.prefs() == &prefs);
        desktop.arrived(read, NOW + 50, false);
        assert!(
            !desktop.unread(&read),
            "the newer mark from the laptop wins"
        );
        desktop.arrived(other, NOW + 20, false);
        assert!(!desktop.unread(&other), "its own newer mark stays");
        // Adopted values are saved, and bad ones keep this device's.
        let mut bad = synced.clone();
        bad.insert(PREFS_KEY.into(), serde_json::json!("loud"));
        desktop.adopt(&bad);
        assert!(Notify::open(two.path(), NOW).prefs() == &prefs);
    }
}
