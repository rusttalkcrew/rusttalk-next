//! Unread marks, mention badges and desktop notifications (issue #67).
//!
//! What was read and the notification choices live in `notify.json` and sync to this
//! person's other devices with the other settings. Previews are decrypted on this device
//! and only handed to the system's notification service; nothing is sent anywhere else.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        mpsc::{Receiver, Sender, channel},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
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
fn mute_for() -> [(&'static str, i64); 6] {
    [
        (t!("notify.mute_15m"), 15 * 60),
        (t!("notify.mute_1h"), 60 * 60),
        (t!("notify.mute_3h"), 3 * 60 * 60),
        (t!("notify.mute_8h"), 8 * 60 * 60),
        (t!("notify.mute_24h"), 24 * 60 * 60),
        (t!("notify.mute_forever"), FOREVER),
    ]
}
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
    pub fn all() -> [(Self, &'static str); 3] {
        [
            (Self::All, t!("notify.level_all")),
            (Self::Mentions, t!("notify.level_mentions")),
            (Self::Nothing, t!("notify.level_nothing")),
        ]
    }
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
pub fn mention(message: &Message, me: &Id, my_roles: &[String]) -> Mention {
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

/// The "Mute" and t!("settings.notifications") submenus of a server (`channel` false) or channel.
/// Returns the changed rule.
pub fn rule_menu(m: &mut crate::ui::Menu, rule: &Rule, channel: bool, now: i64) -> Option<Rule> {
    use crate::ui::Item;
    let mut changed = None;
    if rule.muted(now) {
        let until = match rule.muted_until {
            Some(FOREVER) | None => t!("notify.until_back_on").to_owned(),
            Some(until) => {
                chrono::DateTime::from_timestamp(until, 0).map_or_else(String::new, |at| {
                    let at = at.with_timezone(&chrono::Local);
                    t!(
                        "notify.muted_until",
                        time = crate::i18n::time(at, "time.clock")
                    )
                })
            }
        };
        if m.add(
            Item::new(
                "🔔",
                if channel {
                    t!("notify.unmute_channel")
                } else {
                    t!("notify.unmute_server")
                },
            )
            .hint(&until),
        )
        .clicked()
        {
            changed = Some(Rule {
                muted_until: None,
                ..rule.clone()
            });
        }
    } else {
        m.submenu(
            "🔕",
            if channel {
                t!("notify.mute_channel")
            } else {
                t!("notify.mute_server")
            },
            |m| {
                for (label, seconds) in mute_for() {
                    if m.item("", label).clicked() {
                        changed = Some(Rule {
                            muted_until: Some(now.saturating_add(seconds)),
                            ..rule.clone()
                        });
                    }
                }
            },
        );
    }
    m.submenu("🔔", t!("settings.notifications"), |m| {
        if channel
            && m.add(Item::new("", t!("notify.same_as_server")).picked(rule.level.is_none()))
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
        for (level, label) in Level::all() {
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
        let suppress = Item::new("", t!("notify.mute_everyone"))
            .hint(t!("notify.mute_everyone_help"))
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

/// Open the clicked notification's chat and bring the window forward.
fn deliver_click(clicks: &Sender<Target>, ctx: &eframe::egui::Context, target: Target) {
    let _ = clicks.send(target);
    ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Focus);
    ctx.request_repaint();
}

/// Notifications shown and not clicked yet, by the system's identifier, for a backend
/// that reports a click by identifier only. Capped: most are never clicked.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Default)]
struct Pending(VecDeque<(String, Target)>);

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl Pending {
    const MAX: usize = 100;

    fn add(&mut self, id: String, target: Target) {
        if self.0.len() == Self::MAX {
            self.0.pop_front();
        }
        self.0.push_back((id, target));
    }

    /// The chat of a clicked notification, once.
    fn take(&mut self, id: &str) -> Option<Target> {
        let at = self.0.iter().position(|(known, _)| known == id)?;
        self.0.remove(at).map(|(_, target)| target)
    }
}

#[cfg(target_os = "macos")]
mod macos;

fn read_key(server: &Url, channel: &Id) -> String {
    format!("{server} {}", encode(channel))
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum Reminder {
    Seen { at: i64, remove_at: i64 },
    // Older clients saved only the reminder timestamp, under the bare event ID.
    Legacy(i64),
}

impl Reminder {
    fn at(&self) -> i64 {
        match *self {
            Self::Seen { at, .. } | Self::Legacy(at) => at,
        }
    }

    fn remove_at(&self) -> i64 {
        match *self {
            Self::Seen { remove_at, .. } => remove_at,
            Self::Legacy(at) => {
                at.saturating_add(rusttalk_core::calendar::HORIZON + rusttalk_core::calendar::GRACE)
            }
        }
    }
}

/// `notify.json`.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    prefs: Prefs,
    /// The newest message time read, by server and channel (`read_key`), or by
    /// channel ID alone as older versions saved it.
    read: BTreeMap<String, i64>,
    /// When this file was made: channels never opened count as read up to here, so
    /// old history doesn't light up every channel at once.
    since: i64,
    /// Consumed reminders, keyed by server and event (older files used just the event).
    reminded: BTreeMap<String, Reminder>,
}

/// How long read marks wait before they're written, so a busy channel doesn't write
/// `notify.json` (and wait for the disk) once per message.
const SAVE_DELAY: Duration = Duration::from_secs(3);

/// What the writer thread has to do.
#[derive(Default)]
struct Slot {
    /// The newest copy not written yet; a newer one replaces it.
    latest: Option<Saved>,
    /// Write `latest` now instead of after the delay.
    urgent: bool,
    /// Write what is left and end.
    stop: bool,
    /// A write is in progress.
    busy: bool,
    #[cfg(test)]
    writes: usize,
}

/// Writes `notify.json` off the UI thread, one write at a time, newest copy wins.
struct Writer {
    shared: Arc<(Mutex<Slot>, Condvar)>,
    thread: Option<JoinHandle<()>>,
    delay: Duration,
}

impl Writer {
    fn new(delay: Duration) -> Self {
        Self {
            shared: Arc::default(),
            thread: None,
            delay,
        }
    }

    fn slot(&self) -> MutexGuard<'_, Slot> {
        self.shared.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn save(&mut self, data: &Path, path: &Path, saved: &Saved, urgent: bool) {
        if self.slot().stop {
            return;
        }
        if self.thread.is_none() {
            let (shared, dir, file, delay) = (
                self.shared.clone(),
                data.to_owned(),
                path.to_owned(),
                self.delay,
            );
            let spawned = std::thread::Builder::new()
                .name("notify-save".into())
                .spawn(move || write_loop(&shared, &dir, &file, delay));
            match spawned {
                Ok(thread) => self.thread = Some(thread),
                Err(error) => {
                    tracing::warn!(%error, "starting the notification settings writer failed");
                    write(data, path, saved);
                    return;
                }
            }
        }
        let mut slot = self.slot();
        slot.latest = Some(saved.clone());
        slot.urgent |= urgent;
        drop(slot);
        self.shared.1.notify_all();
    }

    /// Write anything waiting without the delay, in the background.
    fn soon(&self) {
        let mut slot = self.slot();
        if slot.latest.is_some() {
            slot.urgent = true;
            drop(slot);
            self.shared.1.notify_all();
        }
    }

    /// Write anything waiting now, and return once it's on disk.
    #[cfg(test)]
    fn flush(&self) {
        let mut slot = self.slot();
        slot.urgent |= slot.latest.is_some();
        self.shared.1.notify_all();
        while self.thread.is_some() && (slot.latest.is_some() || slot.busy) {
            slot = (self.shared.1.wait(slot)).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// End the thread, first writing what is waiting unless `discard`.
    fn stop(&mut self, discard: bool) {
        let mut slot = self.slot();
        if discard {
            slot.latest = None;
        }
        slot.stop = true;
        drop(slot);
        self.shared.1.notify_all();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::warn!("the notification settings writer panicked");
        }
    }
}

fn write_loop(shared: &(Mutex<Slot>, Condvar), data: &Path, path: &Path, delay: Duration) {
    let (lock, wake) = shared;
    let mut slot = lock.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        while slot.latest.is_none() && !slot.stop {
            slot = wake.wait(slot).unwrap_or_else(PoisonError::into_inner);
        }
        // Wait out the delay from the first change, taking newer copies meanwhile.
        let due = Instant::now() + delay;
        while slot.latest.is_some() && !slot.urgent && !slot.stop {
            let left = due.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            slot = (wake.wait_timeout(slot, left))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        let Some(saved) = slot.latest.take() else {
            if slot.stop {
                return;
            }
            continue;
        };
        slot.urgent = false;
        slot.busy = true;
        drop(slot);
        write(data, path, &saved);
        slot = lock.lock().unwrap_or_else(PoisonError::into_inner);
        slot.busy = false;
        #[cfg(test)]
        {
            slot.writes += 1;
        }
        wake.notify_all();
    }
}

fn write(data: &Path, path: &Path, saved: &Saved) {
    if let Err(error) = save_json(data, path, saved) {
        tracing::warn!(error = %format_args!("{error:#}"), "saving notification settings failed");
    }
}

pub struct Notify {
    path: PathBuf,
    data: PathBuf,
    saved: Saved,
    writer: Writer,
    /// By server and channel: another server can list a channel under the ID of a
    /// DM here, and its messages must not mark the DM.
    pub activity: HashMap<(Url, Id), Activity>,
    /// Notifications clicked, to open their channel.
    clicks: (Sender<Target>, Receiver<Target>),
    /// The system's notification service failed once; logged once.
    failed: bool,
    /// Notifications shown; tests count them instead of showing any.
    #[cfg(test)]
    pub shown: usize,
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
        let mut notify = Self {
            path,
            data: data.to_owned(),
            saved,
            writer: Writer::new(SAVE_DELAY),
            activity: HashMap::new(),
            clicks: channel(),
            failed: false,
            #[cfg(test)]
            shown: 0,
        };
        notify.expire_reminders(now);
        notify
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

    /// Write choices and reminder marks right away, in the background.
    fn save(&mut self) {
        (self.writer).save(&self.data, &self.path, &self.saved, true);
    }

    /// Write read marks after a short delay, together with any that follow.
    fn save_later(&mut self) {
        (self.writer).save(&self.data, &self.path, &self.saved, false);
    }

    /// The window lost focus: write waiting read marks now, in the background.
    pub fn save_soon(&self) {
        self.writer.soon();
    }

    /// Write anything waiting and return once it's on disk.
    #[cfg(test)]
    fn flush(&self) {
        self.writer.flush();
    }

    /// Signing out: drop what is waiting and never write again.
    pub fn stop_saving(&mut self) {
        self.writer.stop(true);
    }

    fn read_until(&self, server: &Url, channel: &Id) -> i64 {
        (self.saved.read.get(&read_key(server, channel)))
            .or_else(|| self.saved.read.get(&encode(channel)))
            .copied()
            .unwrap_or(self.saved.since)
    }

    /// Something new from someone else that wasn't read yet.
    pub fn unread(&self, server: &Url, channel: &Id) -> bool {
        (self.activity.get(&(server.clone(), *channel)))
            .is_some_and(|a| a.latest > self.read_until(server, channel))
    }

    pub fn mentions(&self, server: &Url, channel: &Id) -> u32 {
        (self.activity.get(&(server.clone(), *channel))).map_or(0, |a| a.mentions)
    }

    /// Record a message from someone else, counting it as a mention when `mention`.
    pub fn arrived(&mut self, server: &Url, channel: Id, at: i64, mention: bool) {
        let read = self.read_until(server, &channel);
        let activity = self.activity.entry((server.clone(), channel)).or_default();
        activity.latest = activity.latest.max(at);
        if mention && at > read {
            activity.mentions += 1;
        }
    }

    /// The channel is on screen: everything in it is read.
    pub fn mark_read(&mut self, server: &Url, channel: &Id) {
        let Some(activity) = self.activity.get_mut(&(server.clone(), *channel)) else {
            return;
        };
        activity.mentions = 0;
        let latest = activity.latest;
        if latest > self.read_until(server, channel) {
            self.saved.read.insert(read_key(server, channel), latest);
            self.save_later();
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

    pub fn expire_reminders(&mut self, now: i64) {
        let count = self.saved.reminded.len();
        self.saved.reminded.retain(|_, mark| mark.remove_at() > now);
        if self.saved.reminded.len() != count {
            self.save();
        }
    }

    /// Consume the stamp even when focus suppresses the popup.
    pub fn new_reminder(
        &mut self,
        server: &Url,
        event: &Id,
        at: i64,
        remove_at: i64,
        focused: bool,
    ) -> bool {
        let key = read_key(server, event);
        let legacy = self.saved.reminded.remove(&encode(event));
        let previous = self.saved.reminded.get(&key);
        let seen = previous.into_iter().chain(&legacy).map(Reminder::at).max();
        let newest = seen.map_or(at, |seen| seen.max(at));
        if legacy.is_some()
            || previous.is_none_or(|mark| mark.at() != newest || mark.remove_at() != remove_at)
        {
            self.saved.reminded.insert(
                key,
                Reminder::Seen {
                    at: newest,
                    remove_at,
                },
            );
            self.save();
        }
        seen.is_none_or(|seen| at > seen) && !(focused && self.saved.prefs.unfocused_only)
    }

    /// A clicked notification's server and channel.
    pub fn clicked(&self) -> Option<Target> {
        self.clicks.1.try_recv().ok()
    }

    /// Show a desktop notification; a click on it comes back from `clicked`.
    pub fn show(&mut self, ctx: &eframe::egui::Context, title: &str, body: &str, target: Target) {
        let sound = self.saved.prefs.sound;
        self.present(ctx, title, body, target, sound);
    }

    /// `show` without the system's sound, for what makes its own.
    pub fn show_silent(
        &mut self,
        ctx: &eframe::egui::Context,
        title: &str,
        body: &str,
        target: Target,
    ) {
        self.present(ctx, title, body, target, false);
    }

    fn present(
        &mut self,
        ctx: &eframe::egui::Context,
        title: &str,
        body: &str,
        target: Target,
        sound: bool,
    ) {
        #[cfg(test)]
        {
            self.shown += 1;
        }
        if cfg!(test) {
            return;
        }
        let body: String = match body.chars().count() > PREVIEW_CHARS {
            true => body.chars().take(PREVIEW_CHARS).chain(['…']).collect(),
            false => body.to_owned(),
        };
        let clicks = self.clicks.0.clone();
        let ctx = ctx.clone();
        // Its center delivers the click to us; without an app bundle it can't, so fall
        // through to the pop-up.
        #[cfg(target_os = "macos")]
        if macos::show(&ctx, &self.clicks.0, title, &body, sound, &target) {
            return;
        }
        let (title, failed) = (title.to_owned(), self.failed);
        self.failed = true;
        // Showing talks to the system's service and waiting for a click blocks: own thread.
        let spawned = std::thread::Builder::new()
            .name("notification".into())
            .spawn(move || match popup(&title, &body, sound) {
                Ok(handle) => handle.wait_for_action(|action| {
                    if action != "__closed" {
                        deliver_click(&clicks, &ctx, target);
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

impl Drop for Notify {
    /// Quitting or closing the account: nothing waiting is lost.
    fn drop(&mut self) {
        self.writer.stop(false);
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
        .action("default", t!("notify.open"));
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
    fn a_click_finds_its_chat_once() {
        let mut pending = Pending::default();
        pending.add("a".into(), (url(), [1; 32]));
        pending.add("b".into(), (url(), [2; 32]));
        assert_eq!(pending.take("b"), Some((url(), [2; 32])));
        assert_eq!(pending.take("b"), None);
        assert_eq!(pending.take("unknown"), None);
        assert_eq!(pending.take("a"), Some((url(), [1; 32])));
    }

    #[test]
    fn pending_clicks_drop_the_oldest_past_the_cap() {
        let mut pending = Pending::default();
        for n in 0..=Pending::MAX {
            pending.add(n.to_string(), (url(), [0; 32]));
        }
        assert_eq!(pending.take("0"), None);
        assert!(pending.take("1").is_some());
        assert!(pending.take(&Pending::MAX.to_string()).is_some());
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
            hide_offline: false,
            bio: None,
            voice_muted: false,
            timeout_until: None,
        };
        let role = |id: &str, permissions| Role {
            id: id.into(),
            name: id.into(),
            name_known: true,
            rank: 0,
            permissions,
            custom: false,
            color: None,
            separate: false,
        };
        let dir = Directory {
            me,
            members: vec![
                member(ada, "admin"),
                member(bob, "member"),
                member(me, "member"),
            ]
            .into(),
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
                backfilled: false,
                id: [0; 32],
                person: from,
                at: 0,
                reply_to: None,
                quoted_author: None,
                edit_of: None,
                body: Body::Text(text),
                files: vec![],
                live: None,
                sticker: None,
                replicas: Vec::new(),
                copy_files: Vec::new(),
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
    fn reminder_marks_survive_busy_calendars_and_offline_restarts_until_event_end() {
        let dir = tempfile::tempdir().unwrap();
        let mut notify = Notify::open(dir.path(), NOW);
        let other: Url = "https://203.0.113.9/".parse().unwrap();
        let event = [0; 32];
        for id in 0..=200 {
            assert!(notify.new_reminder(&url(), &[id; 32], NOW, NOW + 10, false));
        }
        assert!(notify.new_reminder(&other, &event, NOW, NOW + 10, false));
        notify.flush();
        let mut notify = Notify::open(dir.path(), NOW + 9);
        for id in 0..=200 {
            assert!(!notify.new_reminder(&url(), &[id; 32], NOW, NOW + 10, false));
        }
        assert!(!notify.new_reminder(&other, &event, NOW, NOW + 20, false));
        notify.expire_reminders(NOW + 10);
        assert_eq!(notify.saved.reminded.len(), 1);
        assert!(!notify.new_reminder(&other, &event, NOW, NOW + 20, false));
        notify.flush();
        assert!(Notify::open(dir.path(), NOW + 20).saved.reminded.is_empty());
    }

    #[test]
    fn existing_marks_migrate_and_focused_reminders_stay_consumed() {
        let dir = tempfile::tempdir().unwrap();
        let event = [3; 32];
        let file = serde_json::json!({
            "prefs": {"unfocused_only": true},
            "reminded": {encode(&event): NOW}
        });
        std::fs::write(dir.path().join("notify.json"), file.to_string()).unwrap();
        let mut notify = Notify::open(dir.path(), NOW);
        assert!(notify.prefs().unfocused_only);
        assert!(!notify.new_reminder(&url(), &event, NOW, NOW + 50, false));
        assert!(!notify.new_reminder(&url(), &event, NOW + 1, NOW + 50, true));
        notify.flush();
        let mut notify = Notify::open(dir.path(), NOW + 2);
        assert!(!notify.new_reminder(&url(), &event, NOW + 1, NOW + 50, false));
        assert!(notify.new_reminder(&url(), &event, NOW + 2, NOW + 50, false));
        notify.flush();
        assert!(Notify::open(dir.path(), NOW + 50).saved.reminded.is_empty());
    }

    #[test]
    fn tracks_unread_and_mentions_until_read() {
        let dir = tempfile::tempdir().unwrap();
        let mut notify = Notify::open(dir.path(), NOW);
        let channel = [2; 32];
        // History from before this file existed counts as read.
        notify.arrived(&url(), channel, NOW - 10, true);
        assert!(!notify.unread(&url(), &channel));
        assert_eq!(notify.mentions(&url(), &channel), 0);

        notify.arrived(&url(), channel, NOW + 5, false);
        notify.arrived(&url(), channel, NOW + 6, true);
        notify.arrived(&url(), channel, NOW + 7, true);
        assert!(notify.unread(&url(), &channel));
        assert_eq!(notify.mentions(&url(), &channel), 2);
        notify.mark_read(&url(), &channel);
        assert!(!notify.unread(&url(), &channel));
        assert_eq!(notify.mentions(&url(), &channel), 0);
        // A late page of older messages doesn't light it up again.
        notify.arrived(&url(), channel, NOW + 6, true);
        assert!(!notify.unread(&url(), &channel));
        assert_eq!(notify.mentions(&url(), &channel), 0);

        // The read mark survives a restart.
        notify.flush();
        let mut again = Notify::open(dir.path(), NOW + 100);
        again.arrived(&url(), channel, NOW + 7, false);
        assert!(!again.unread(&url(), &channel));
        again.arrived(&url(), channel, NOW + 8, false);
        assert!(again.unread(&url(), &channel));
    }

    #[test]
    fn another_servers_channel_under_the_same_id_leaves_a_dm_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut notify = Notify::open(dir.path(), NOW);
        let (dm, other): (Url, Url) = (url(), "https://203.0.113.9/".parse().unwrap());
        let channel = [2; 32];
        notify.arrived(&other, channel, NOW + 5, true);
        assert!(notify.unread(&other, &channel) && !notify.unread(&dm, &channel));
        assert_eq!(notify.mentions(&dm, &channel), 0);
        // Reading it there doesn't read the DM either.
        notify.arrived(&dm, channel, NOW + 6, false);
        notify.mark_read(&other, &channel);
        assert!(notify.unread(&dm, &channel));
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
        laptop.arrived(&url(), read, NOW + 50, false);
        laptop.mark_read(&url(), &read);
        laptop.flush();
        assert!(Notify::open(one.path(), NOW).prefs() == &prefs);

        // The desktop read further in one channel and less in another.
        let mut desktop = Notify::open(two.path(), NOW);
        let other = [4; 32];
        desktop.arrived(&url(), other, NOW + 20, false);
        desktop.mark_read(&url(), &other);
        desktop.arrived(&url(), read, NOW + 10, false);
        desktop.mark_read(&url(), &read);
        let synced = laptop.synced();
        assert_eq!(
            synced.keys().map(String::as_str).collect::<Vec<_>>(),
            SYNCED
        );
        desktop.adopt(&synced);
        assert!(desktop.prefs() == &prefs);
        desktop.arrived(&url(), read, NOW + 50, false);
        assert!(
            !desktop.unread(&url(), &read),
            "the newer mark from the laptop wins"
        );
        desktop.arrived(&url(), other, NOW + 20, false);
        assert!(!desktop.unread(&url(), &other), "its own newer mark stays");
        // Adopted values are saved, and bad ones keep this device's.
        let mut bad = synced.clone();
        bad.insert(PREFS_KEY.into(), serde_json::json!("loud"));
        desktop.adopt(&bad);
        desktop.flush();
        assert!(Notify::open(two.path(), NOW).prefs() == &prefs);
    }

    fn writes(notify: &Notify) -> usize {
        notify.writer.slot().writes
    }

    /// Notify that waits `delay` before writing read marks.
    fn reading(dir: &Path, delay: Duration) -> Notify {
        let mut notify = Notify::open(dir, NOW);
        notify.writer.delay = delay;
        notify
    }

    fn read_marks_on_disk(dir: &Path) -> BTreeMap<String, i64> {
        let saved: Saved = crate::load_json::<Option<Saved>>(&dir.join("notify.json"))
            .unwrap()
            .unwrap_or_default();
        saved.read
    }

    #[test]
    fn a_burst_of_read_marks_is_written_once_after_the_delay() {
        let dir = tempfile::tempdir().unwrap();
        let delay = Duration::from_millis(300);
        let mut notify = reading(dir.path(), delay);
        let channel = [2; 32];
        let started = Instant::now();
        for at in 1..=100 {
            notify.arrived(&url(), channel, NOW + at, false);
            notify.mark_read(&url(), &channel);
        }
        assert_eq!(writes(&notify), 0, "nothing is written during the burst");
        assert!(!dir.path().join("notify.json").exists());
        while writes(&notify) == 0 {
            assert!(started.elapsed() < Duration::from_secs(10), "never written");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(started.elapsed() >= delay);
        std::thread::sleep(delay * 2);
        assert_eq!(writes(&notify), 1, "one write for the whole burst");
        let key = read_key(&url(), &channel);
        assert_eq!(read_marks_on_disk(dir.path())[&key], NOW + 100);
    }

    #[test]
    fn flushing_writes_waiting_read_marks_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut notify = reading(dir.path(), Duration::from_secs(3600));
        let channel = [2; 32];
        notify.arrived(&url(), channel, NOW + 5, false);
        notify.mark_read(&url(), &channel);
        assert_eq!(writes(&notify), 0);
        notify.flush();
        assert_eq!(writes(&notify), 1);
        let key = read_key(&url(), &channel);
        assert_eq!(read_marks_on_disk(dir.path())[&key], NOW + 5);
        // Nothing waiting: flushing again doesn't write.
        notify.flush();
        assert_eq!(writes(&notify), 1);

        // Losing focus writes without the delay too, in the background.
        notify.arrived(&url(), channel, NOW + 6, false);
        notify.mark_read(&url(), &channel);
        notify.save_soon();
        let started = Instant::now();
        while writes(&notify) < 2 {
            assert!(started.elapsed() < Duration::from_secs(10), "never written");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(read_marks_on_disk(dir.path())[&key], NOW + 6);
    }

    #[test]
    fn closing_keeps_waiting_read_marks_and_signing_out_drops_them() {
        let dir = tempfile::tempdir().unwrap();
        let channel = [2; 32];
        let key = read_key(&url(), &channel);
        let mut notify = reading(dir.path(), Duration::from_secs(3600));
        notify.arrived(&url(), channel, NOW + 5, false);
        notify.mark_read(&url(), &channel);
        drop(notify);
        assert_eq!(read_marks_on_disk(dir.path())[&key], NOW + 5);

        let mut notify = reading(dir.path(), Duration::from_secs(3600));
        notify.arrived(&url(), channel, NOW + 9, false);
        notify.mark_read(&url(), &channel);
        notify.stop_saving();
        std::fs::remove_file(dir.path().join("notify.json")).unwrap();
        notify.arrived(&url(), channel, NOW + 10, false);
        notify.mark_read(&url(), &channel);
        drop(notify);
        assert!(
            !dir.path().join("notify.json").exists(),
            "nothing after sign out"
        );
    }
}
