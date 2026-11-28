//! Server and channel settings. The server decides who can make each change.

mod emoji;

mod file_limits;

mod history;
mod new_members;
mod rate_limits;
pub(crate) mod reports;
mod stats;

mod import;

mod lockdown;

mod members;

mod roles;

pub mod takedown;

pub mod timeout;

use chrono::{DateTime, Local};
use eframe::egui::{self, CollapsingHeader, ComboBox, DragValue, RichText, Ui, accesskit::Role};

use rusttalk_core::{
    auth::{ServerUpdateOutcome, ServerUpdates, decode},
    index::{https_url, validate_details},
};

use crate::{
    files::Ask,
    home::{
        Home, initials,
        person::{self, Person, Pick},
    },
    net::{
        Admin, AdminUpdate, AuditEntry, Ban, Command, Delisted, Invite, PublicListing, Takedown,
    },
    server::{Channel, Id, Perms, Server},
    settings::{self as panel, Group, Page},
    ui::{self, Item, danger, error_text},
};

const ICON_SIZE: f32 = 80.0;
const HEADER_ICON: f32 = 40.0;

/// Audit log filters: API action and label.
fn filters() -> [(&'static str, &'static str); 11] {
    [
        ("member_banned", t!("audit.filter_bans")),
        ("member_unbanned", t!("audit.filter_unbans")),
        ("member_kicked", t!("audit.filter_kicks")),
        ("member_timed_out", t!("audit.filter_timeouts")),
        ("report_resolved", t!("audit.filter_reports")),
        ("left", t!("audit.filter_leaves")),
        ("member_roles_changed", t!("audit.filter_roles")),
        ("joined", t!("audit.filter_joins")),
        ("message_deleted", t!("audit.filter_deleted")),
        ("invite_created", t!("audit.filter_invites")),
        (
            "channel_permissions_changed",
            t!("audit.filter_channel_permissions"),
        ),
    ]
}

#[derive(Clone, Copy, PartialEq)]
pub enum Tab {
    /// The server icon.
    Overview,
    Members,
    Roles,
    Invites,
    Bans,
    Audit,
    Joining,
    Files,
    /// How many messages each channel keeps.
    History,
    /// How fast members may post, upload and so on.
    RateLimits,
    /// How long the new member rules last.
    NewMembers,
    /// The owner's public listing.
    Listing,
    /// Server emoji.
    Emoji,
    /// Server stickers.
    Stickers,
    /// Stop new people joining.
    Lockdown,
    /// The owner's takedowns: remove content for a legal reason, and the record of it.
    Takedowns,
    /// Reported messages to review.
    Reports,
    /// Server statistics: counts and sizes, refreshed while open.
    Stats,
}

enum MemberAction {
    /// `remove`: also remove what they posted in this many seconds before, 0 for nothing.
    Ban {
        network: bool,
        remove: i64,
    },
    Roles(Vec<String>),
}

pub struct Settings {
    pub server: usize,
    tab: Tab,
    bans: Vec<Ban>,
    /// The first ban and invite lists arrived.
    bans_loaded: bool,
    invites_loaded: bool,
    /// Newest first; `None` while the first page loads.
    audit: Option<Vec<AuditEntry>>,
    /// Where t!("admin.show_older") continues, when there may be more.
    audit_next: Option<i64>,
    audit_error: Option<String>,
    audit_filter: Option<&'static str>,
    /// Open joining and member limit, once loaded (owners and admins).
    joining: Option<(bool, u32)>,
    files: Option<file_limits::Form>,
    history: Option<history::Form>,
    rate_limits: Option<rate_limits::Form>,
    new_members: Option<new_members::Form>,
    lockdown: Option<lockdown::Form>,
    stats: stats::View,
    /// Newest first; `None` until loaded (owner).
    takedowns: Option<Vec<Takedown>>,
    takedown: takedown::Form,
    /// The open reports, once loaded.
    reports: Option<Vec<rusttalk_core::client::ReceivedReport>>,
    report_confirm: Option<reports::Confirm>,
    /// Whether the server is listed and the details, once loaded (owner).
    listing: Option<(bool, PublicListing)>,
    /// What the server has for `joining` and `listing`, to tell unsaved edits apart.
    saved_joining: Option<(bool, u32)>,
    saved_listing: Option<(bool, PublicListing)>,
    /// The directory removed the server, as it last told the server.
    delisted: Option<Delisted>,
    /// A Joining or Listing save on its way; it counts as saved once it went through.
    saving: Option<Saving>,
    updates: ServerUpdates,
    member: Option<(Id, MemberAction)>,
    /// The kick, ban or roles question was already on screen last frame.
    member_shown: bool,
    /// "Also block them for me" on the ban question, and whom to block once the ban
    /// went through.
    also_block: bool,
    block_after_ban: Option<Id>,
    members: members::View,
    roles: roles::RolesPage,
    invites: Vec<Invite>,
    /// The invite waiting for t!("admin.revoke") to be confirmed.
    revoking: Option<String>,
    /// The server name being typed in Overview; `None` until edited.
    server_name: Option<String>,
    /// The Emoji and Stickers pages.
    emoji: emoji::State,
    /// "Import pack from .zip", while its dialog is open.
    import: Option<import::Import>,
    /// The last result: text and whether it went wrong.
    message: Option<(String, bool)>,
}

enum Saving {
    Joining((bool, u32)),
    Listing((bool, PublicListing)),
}

impl Settings {
    pub fn apply(&mut self, update: AdminUpdate) {
        match update {
            AdminUpdate::Bans(bans) => (self.bans, self.bans_loaded) = (bans, true),
            AdminUpdate::Takedowns(list) => self.takedowns = Some(list),
            AdminUpdate::Reports(list) => self.reports = Some(list),
            AdminUpdate::Audit {
                entries,
                older,
                next,
            } => {
                match &mut self.audit {
                    Some(shown) if older => shown.extend(entries),
                    // An older page for a filter that has since changed.
                    None if older => return,
                    audit => *audit = Some(entries),
                }
                self.audit_next = next;
                self.audit_error = None;
            }
            AdminUpdate::AuditFailed(text) => self.audit_error = Some(text),
            AdminUpdate::Stats(update) => self.stats.apply(update),
            AdminUpdate::Invites(invites) => (self.invites, self.invites_loaded) = (invites, true),
            AdminUpdate::Joining(open, cap) => {
                if !self.joining_unsaved() {
                    self.joining = Some((open, cap));
                }
                self.saved_joining = Some((open, cap));
            }
            // Edits waiting for Save stay when the page loads again; only what's saved
            // refreshes.
            AdminUpdate::Files(settings) => match &mut self.files {
                Some(form) if form.unsaved() => form.saved(settings),
                files => *files = Some(file_limits::Form::new(settings)),
            },
            AdminUpdate::History(saved) => match &mut self.history {
                Some(form) if form.unsaved() => form.saved(saved),
                history => *history = Some(history::Form::new(saved)),
            },
            AdminUpdate::RateLimits(saved) => match &mut self.rate_limits {
                Some(form) if form.unsaved() => form.saved(saved),
                limits => *limits = Some(rate_limits::Form::new(saved)),
            },
            AdminUpdate::NewMembers(saved) => match &mut self.new_members {
                Some(form) if form.unsaved() => form.saved(saved),
                rules => *rules = Some(new_members::Form::new(saved)),
            },
            AdminUpdate::Listing(on, saved, delisted) => {
                self.delisted = delisted;
                let saved = (on, saved.unwrap_or_default());
                if !self.listing_unsaved() {
                    self.listing = Some(saved.clone());
                }
                self.saved_listing = Some(saved);
            }
            AdminUpdate::ServerUpdates(updates) => self.updates = updates,
            AdminUpdate::Progress(done, total) => {
                if let Some(import) = &mut self.import {
                    import.progress(done, total);
                }
            }
            // Only now: a cancelled pick or a refused upload keeps the picked picture.
            AdminUpdate::EmojiAdded(name) => self.emoji.added(&name),
            AdminUpdate::Done(text) => {
                match self.saving.take() {
                    Some(Saving::Joining(saved)) => self.saved_joining = Some(saved),
                    Some(Saving::Listing(saved)) => self.saved_listing = Some(saved),
                    None => {}
                }
                self.member = None;
                self.roles.done();
                self.revoking = None;
                self.emoji.done();
                self.import = None;
                if self.tab == Tab::Takedowns {
                    self.takedown = takedown::Form::default();
                }
                self.message = Some((text.into(), false));
            }
            // Only Roles saves in batches.
            AdminUpdate::PartlySaved { failed, text } => {
                self.roles.partly_saved(&failed);
                self.emoji.failed();
                self.message = Some((text, true));
            }
            AdminUpdate::Failed(text) => {
                self.saving = None;
                self.block_after_ban = None;
                self.roles.failed();
                self.emoji.failed();
                if let Some(import) = &mut self.import {
                    import.failed();
                }
                self.message = Some((text, true));
            }
        }
    }

    /// Whom to block now that a change went through: the person banned with
    /// "Also block them for me".
    pub fn banned_to_block(&mut self) -> Option<Id> {
        self.block_after_ban.take()
    }

    fn joining_unsaved(&self) -> bool {
        self.joining.is_some() && self.joining != self.saved_joining
    }

    /// Details count only while listed: switched off, they aren't saved.
    fn listing_unsaved(&self) -> bool {
        match (&self.listing, &self.saved_listing) {
            (Some((on, listing)), Some((saved_on, saved))) => {
                on != saved_on || (*on && listing != saved)
            }
            _ => false,
        }
    }

    /// Changes on any page that wait for Save, which closing would throw away.
    fn unsaved(&self, server: &Server) -> bool {
        let name = server.dir.name.as_deref().unwrap_or_default();
        self.roles.dirty(&server.dir)
            || (self.server_name.as_deref()).is_some_and(|typed| typed.trim() != name)
            || self.files.as_ref().is_some_and(file_limits::Form::unsaved)
            || self.history.as_ref().is_some_and(history::Form::unsaved)
            || (self.rate_limits.as_ref()).is_some_and(rate_limits::Form::unsaved)
            || (self.new_members.as_ref()).is_some_and(new_members::Form::unsaved)
            || self.lockdown.as_ref().is_some_and(lockdown::Form::unsaved)
            || self.joining_unsaved()
            || self.listing_unsaved()
            || self.emoji.unsaved()
    }
}

/// Start "Import pack from .zip" on the open server settings, with the picked file.
pub fn start_import(home: &mut Home, path: &std::path::Path, ctx: &egui::Context) {
    let Some(settings) = &mut home.server_settings else {
        return;
    };
    let dir = &home.servers[settings.server].dir;
    let max = dir.emoji_max.0.max(dir.emoji_max.1);
    let sticker = settings.tab == Tab::Stickers;
    settings.import = Some(import::Import::start(path, max, sticker, ctx));
}

/// Pictures picked for the Emoji (or `sticker`, Stickers) page, to show before Upload.
pub fn stage(home: &mut Home, paths: Vec<std::path::PathBuf>, sticker: bool, ctx: &egui::Context) {
    if let Some(settings) = &mut home.server_settings {
        settings.emoji.stage(sticker, paths, ctx);
    }
}

/// A picked picture cropped again by hand.
pub fn cropped(home: &mut Home, slot: u64, bytes: Vec<u8>) {
    if let Some(settings) = &mut home.server_settings {
        settings.emoji.cropped(slot, bytes);
    }
}

/// On the Emoji and Stickers pages, files dropped on the window are pictures to add,
/// not attachments for the chat under the settings: take them before the chat sees
/// them. Call before anything else reads this frame's input.
pub fn take_drops(ctx: &egui::Context, home: &mut Home) {
    let Some(settings) = &mut home.server_settings else {
        return;
    };
    let sticker = match settings.tab {
        Tab::Emoji => false,
        Tab::Stickers => true,
        _ => return,
    };
    if !crate::files::PICKER {
        return;
    }
    let (hovering, dropped) = ctx.input_mut(|input| {
        let hovering = !std::mem::take(&mut input.raw.hovered_files).is_empty();
        let dropped: Vec<_> = std::mem::take(&mut input.raw.dropped_files)
            .iter()
            .map(|file| file.path().to_owned())
            .collect();
        (hovering, dropped)
    });
    settings.emoji.hovering = hovering;
    settings.emoji.stage(sticker, dropped, ctx);
}

/// Open server settings on a tab, loading what that person may see.
pub fn open(home: &mut Home, index: usize, mut tab: Tab) {
    let server = &home.servers[index];
    let perms = server.dir.perms;
    if tab == Tab::Members && !(perms.kick || perms.ban || perms.manage_roles) {
        tab = if perms.manage_server {
            Tab::Overview
        } else if perms.manage_emoji {
            Tab::Emoji
        } else if perms.audit {
            Tab::Audit
        } else if perms.admin {
            Tab::Joining
        } else if perms.invite {
            Tab::Invites
        } else if perms.lock_down {
            Tab::Lockdown
        } else if perms.review_reports {
            Tab::Reports
        } else {
            Tab::Files
        };
    }
    server.send(Command::Admin(Admin::Load {
        joining: perms.admin,
        bans: perms.ban,
    }));
    if tab == Tab::Audit {
        server.send(Command::Admin(Admin::Audit {
            action: None,
            before: None,
        }));
    }
    if tab == Tab::Overview && perms.owner {
        server.send(Command::Admin(Admin::LoadUpdates));
    }
    if tab == Tab::Invites {
        server.send(Command::Admin(Admin::Invites));
    }
    if tab == Tab::Files {
        server.send(Command::Admin(Admin::LoadFiles));
    }
    if tab == Tab::History {
        server.send(Command::Admin(Admin::LoadHistory));
    }
    if tab == Tab::RateLimits {
        server.send(Command::Admin(Admin::LoadRateLimits));
    }
    if tab == Tab::NewMembers {
        server.send(Command::Admin(Admin::LoadNewMembers));
    }
    if tab == Tab::Takedowns {
        server.send(Command::Admin(Admin::LoadTakedowns));
    }
    if tab == Tab::Reports {
        server.send(Command::Admin(Admin::LoadReports));
    }
    home.server_settings = Some(Settings {
        server: index,
        tab,
        bans: Vec::new(),
        bans_loaded: false,
        invites_loaded: false,
        audit: None,
        audit_next: None,
        audit_error: None,
        audit_filter: None,
        joining: None,
        files: None,
        history: None,
        rate_limits: None,
        new_members: None,
        lockdown: None,
        stats: stats::View::default(),
        takedowns: None,
        takedown: takedown::Form::default(),
        reports: None,
        report_confirm: None,
        listing: None,
        saved_joining: None,
        saved_listing: None,
        delisted: None,
        saving: None,
        updates: ServerUpdates::default(),
        member: None,
        member_shown: false,
        also_block: false,
        block_after_ban: None,
        members: members::View::default(),
        roles: roles::RolesPage::default(),
        invites: Vec::new(),
        revoking: None,
        server_name: None,
        emoji: emoji::State::default(),
        import: None,
        message: None,
    });
}

/// While the server is locked down, tell the people who may lift it, with a button.
pub fn lockdown_banner(ui: &mut Ui, home: &mut Home, index: usize) {
    let server = &home.servers[index];
    let Some(lockdown) = (server.dir.lockdown.as_ref()).filter(|lockdown| lockdown.locked) else {
        return;
    };
    if !server.dir.perms.lock_down || server.dir.perms.preview {
        return;
    }
    let text = match lockdown.automatic {
        true => t!("lockdown.banner_automatic"),
        false => t!("lockdown.banner"),
    };
    let online = server.status == crate::server::Status::Online;
    let (lift, open_page) = ui::card(ui, |ui| {
        ui.add(egui::Label::new(ui::bold(text)).wrap());
        ui.horizontal_wrapped(|ui| {
            let lift = ui
                .add_enabled_ui(online, |ui| ui::primary(ui, t!("lockdown.lift")))
                .inner
                .clicked();
            (lift, ui::secondary(ui, t!("lockdown.settings")).clicked())
        })
        .inner
    });
    ui.add_space(ui::SPACE_S);
    if lift {
        let lifted = crate::server::Lockdown {
            locked: false,
            ..lockdown.clone()
        };
        server.send(Command::Admin(Admin::SaveLockdown(lifted)));
    }
    if open_page {
        open(home, index, Tab::Lockdown);
    }
}

#[derive(Clone, Copy)]
pub enum Moderate {
    Roles,
    Timeout,
    EndTimeout,
    Kick,
    Ban,
}

/// Menu items to change the roles of, time out, kick or ban someone else, as allowed.
/// `timed_out`: they are in a timeout now, so it can be ended instead.
pub fn member_menu(m: &mut ui::Menu, choice: &mut Option<Moderate>, perms: Perms, timed_out: bool) {
    if perms.manage_roles && m.item("🏷", t!("admin.change_roles")).clicked() {
        *choice = Some(Moderate::Roles);
    }
    if perms.timeout_members {
        if !timed_out && m.item("⏳", t!("timeout.menu")).clicked() {
            *choice = Some(Moderate::Timeout);
        }
        if timed_out && m.item("⌛", t!("timeout.end")).clicked() {
            *choice = Some(Moderate::EndTimeout);
        }
    }
    if perms.kick && m.add(Item::new("👢", t!("admin.kick")).danger()).clicked() {
        *choice = Some(Moderate::Kick);
    }
    if perms.ban && m.add(Item::new("🚫", t!("admin.ban")).danger()).clicked() {
        *choice = Some(Moderate::Ban);
    }
}

/// Ask before kicking, or open settings for a ban or role change.
pub fn moderate(home: &mut Home, index: usize, person: Id, choice: Moderate) {
    match choice {
        Moderate::Timeout => return timeout::open(home, index, person),
        Moderate::EndTimeout => {
            return home.servers[index].send(Command::Admin(Admin::EndTimeout(person)));
        }
        _ => {}
    }
    if matches!(choice, Moderate::Kick) {
        if home.kick.is_none() {
            let server = &home.servers[index];
            if can_kick(home, server, person) {
                home.kick = Some(Kick {
                    server: server.known.server_id,
                    person,
                    name: server.member_name(&person),
                    commands: server.commands.clone(),
                    opened_frame: None,
                });
            }
        }
        return;
    }
    if home
        .server_settings
        .as_ref()
        .is_none_or(|settings| settings.server != index)
    {
        open(home, index, Tab::Members);
    }
    let action = action(&home.servers[index], &person, choice);
    if let Some(settings) = &mut home.server_settings {
        settings.tab = Tab::Members;
        settings.member = Some((person, action));
        settings.message = None;
        settings.also_block = false;
    }
}

/// Frozen identity and connection: neither a reordered list nor a reconnect may retarget it.
pub struct Kick {
    server: Id,
    person: Id,
    name: String,
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
    // A sizing pass must not arm confirmation in the frame that opened it.
    opened_frame: Option<u64>,
}

impl Kick {
    pub(crate) fn belongs_to(&self, server: &Server) -> bool {
        self.server == server.known.server_id && self.commands.same_channel(&server.commands)
    }
}

fn can_kick(home: &Home, server: &Server, person: Id) -> bool {
    server.status == crate::server::Status::Online
        && server.dir.perms.kick
        && server.member(&server.dir.me).is_some()
        && person != server.dir.me
        && Person::of(home, server, person).outranked()
}

pub fn kick_dialog(ctx: &egui::Context, home: &mut Home) {
    let Some(mut kick) = home.kick.take() else {
        return;
    };
    let Some(server) = home.servers.iter().find(|server| kick.belongs_to(server)) else {
        return;
    };
    if !can_kick(home, server, kick.person) {
        return;
    }
    let frame = ctx.cumulative_frame_nr();
    let ready = *kick.opened_frame.get_or_insert(frame) < frame;
    let mut confirm = false;
    let mut cancel = false;
    let (_, dismissed) = ui::dialog(
        ctx,
        "kick-member",
        &t!("admin.kick_title", name = kick.name),
        |ui| {
            ui.label(t!("admin.kick_help"));
            ui.horizontal(|ui| {
                confirm = ui
                    .add_enabled_ui(ready, |ui| danger(ui, t!("admin.kick")))
                    .inner
                    .clicked();
                cancel = ui::secondary(ui, t!("common.cancel")).clicked();
            });
        },
    );
    if confirm && !cancel && !dismissed {
        server.send(Command::Admin(Admin::Kick(kick.person)));
    } else if !cancel && !dismissed {
        home.kick = Some(kick);
    }
}

fn action(server: &Server, person: &Id, choice: Moderate) -> MemberAction {
    match choice {
        Moderate::Roles => MemberAction::Roles(
            (server.dir.members.iter())
                .find(|m| m.person == *person)
                .map(|m| m.roles.clone())
                .unwrap_or_default(),
        ),
        Moderate::Kick | Moderate::Timeout | Moderate::EndTimeout => {
            unreachable!("handled in `moderate`")
        }
        Moderate::Ban => MemberAction::Ban {
            network: false,
            remove: 0,
        },
    }
}

pub fn window(ctx: &egui::Context, home: &mut Home) {
    let Some(settings) = &mut home.server_settings else {
        return;
    };
    let Some(server) = home.servers.get(settings.server) else {
        home.server_settings = None;
        return;
    };
    // Nothing here may change while the server is previewed as other roles.
    if server.view_as.is_some() {
        home.server_settings = None;
        return;
    }
    let index = settings.server;
    // `Person` reads these two parts of `home` while `settings` borrows another.
    let (friends, voice) = (&home.friends, &home.voice);
    let mut person_pick = None;
    let mut request;
    let mut icon = false;
    let mut emoji_asked = emoji::Asked::default();
    let mut rename = None;
    let mut batch = Vec::new();
    let mut save_reported = None;
    let close = {
        let perms = server.dir.perms;
        let page = |id, icon, label, allowed: bool| allowed.then_some(Page { id, icon, label });
        let group = |title, pages: Vec<Option<Page<Tab>>>| Group {
            title,
            pages: pages.into_iter().flatten().collect(),
        };
        let groups = [
            group(
                t!("admin.server"),
                vec![
                    page(
                        Tab::Overview,
                        "🖼",
                        t!("admin.overview"),
                        perms.manage_server,
                    ),
                    page(Tab::Joining, "🚪", t!("admin.joining"), perms.admin),
                    page(Tab::Files, "📎", t!("admin.files"), true),
                    page(Tab::History, "💬", t!("admin.history"), true),
                    page(Tab::RateLimits, "⏱", t!("admin.rate_limits"), true),
                    page(Tab::Emoji, "😀", t!("admin.emoji"), perms.manage_emoji),
                    page(Tab::Stickers, "🏷", t!("admin.stickers"), perms.manage_emoji),
                    page(Tab::Listing, "🌐", t!("admin.public_listing"), perms.owner),
                    page(Tab::Stats, "📊", t!("admin.stats"), perms.stats),
                ],
            ),
            group(
                t!("admin.people"),
                vec![
                    page(
                        Tab::Members,
                        "👥",
                        t!("admin.members"),
                        perms.kick || perms.ban || perms.manage_roles,
                    ),
                    page(Tab::Roles, "🏷", t!("admin.roles"), perms.manage_roles),
                    page(Tab::Invites, "✉", t!("admin.invites"), perms.invite),
                    page(Tab::Bans, "🚫", t!("admin.bans"), perms.ban),
                ],
            ),
            group(
                t!("admin.moderation"),
                vec![
                    page(
                        Tab::NewMembers,
                        "🌱",
                        t!("admin.new_members"),
                        perms.manage_server,
                    ),
                    page(
                        Tab::Lockdown,
                        "🔒",
                        t!("lockdown.title"),
                        perms.lock_down && server.dir.lockdown.is_some(),
                    ),
                    page(Tab::Takedowns, "⚖", t!("takedown.title"), perms.owner),
                    // Only servers that know reports gave the owner role this.
                    page(Tab::Reports, "⚑", t!("reports.page"), perms.review_reports),
                    page(Tab::Audit, "📜", t!("admin.audit_log"), perms.audit),
                ],
            ),
        ];
        let before = settings.tab;
        let mut tab = before;
        let (changed, close) = panel::window(
            ctx,
            "server-settings",
            t!("home.server_settings"),
            &groups,
            &mut tab,
            settings,
            |ui, _| {
                panel::sidebar_header(ui, server.name(), t!("home.server_settings"), |ui| {
                    server_icon(ui, server, HEADER_ICON);
                })
            },
            |ui, settings| {
                let changed = match before {
                    Tab::Overview => {
                        let picked = overview_tab(ui, server, settings);
                        icon = picked.icon;
                        rename = picked.rename;
                        picked
                            .events
                            .map(Admin::Events)
                            .or_else(|| picked.system_messages.map(Admin::SystemMessages))
                    }
                    Tab::Members => {
                        let target = |person| Person::with(friends, voice, server, person);
                        let (request, picked) = members_tab(ui, server, settings, target);
                        person_pick = picked;
                        request
                    }
                    Tab::Roles => {
                        batch = roles::tab(ui, server, &mut settings.roles, &mut settings.message);
                        None
                    }
                    Tab::Invites => invites_tab(ui, server, settings),
                    Tab::Bans => bans_tab(ui, server, settings),
                    Tab::Audit => audit_tab(ui, server, settings),
                    Tab::Joining => joining_tab(ui, settings),
                    Tab::Files => {
                        if let Some(form) = &mut settings.files {
                            form.show(ui, perms.manage_server).map(Admin::SaveFiles)
                        } else {
                            if settings.message.is_none() {
                                ui.spinner();
                            }
                            None
                        }
                    }
                    Tab::History => {
                        if let Some(form) = &mut settings.history {
                            form.show(ui, perms.manage_server).map(Admin::SaveHistory)
                        } else {
                            loading(ui, settings);
                            None
                        }
                    }
                    Tab::RateLimits => {
                        if let Some(form) = &mut settings.rate_limits {
                            form.show(ui, perms.manage_server)
                                .map(Admin::SaveRateLimits)
                        } else {
                            loading(ui, settings);
                            None
                        }
                    }
                    Tab::NewMembers => {
                        if let Some(form) = &mut settings.new_members {
                            form.show(ui).map(Admin::SaveNewMembers)
                        } else {
                            loading(ui, settings);
                            None
                        }
                    }
                    Tab::Listing => listing_tab(ui, server, settings),
                    Tab::Reports => match settings.reports.as_deref() {
                        // A failed load shows its error instead of a spinner.
                        None => {
                            loading(ui, settings);
                            None
                        }
                        Some(list) => {
                            match reports::page(ui, server, list, &mut settings.report_confirm) {
                                Some(reports::Asked::Request(request)) => Some(request),
                                Some(reports::Asked::Save(file)) => {
                                    save_reported = Some(file);
                                    None
                                }
                                None => None,
                            }
                        }
                    },
                    Tab::Stats => settings.stats.show(ui),
                    Tab::Lockdown => match &server.dir.lockdown {
                        Some(saved) => {
                            let form = (settings.lockdown)
                                .get_or_insert_with(|| lockdown::Form::new(saved.clone()));
                            form.sync(saved);
                            form.show(ui).map(Admin::SaveLockdown)
                        }
                        None => {
                            loading(ui, settings);
                            None
                        }
                    },
                    Tab::Takedowns => takedown::page(
                        ui,
                        server,
                        &mut settings.takedown,
                        settings.takedowns.as_deref(),
                    ),
                    Tab::Emoji | Tab::Stickers => {
                        let sticker = before == Tab::Stickers;
                        let picker = crate::files::PICKER;
                        emoji_asked =
                            emoji::page(ui, &server.dir, &mut settings.emoji, sticker, picker);
                        emoji_asked.request.take()
                    }
                };
                message(ui, settings);
                changed
            },
            |settings| settings.unsaved(server),
        );
        if tab != before {
            settings.tab = tab;
            settings.message = None;
            // Show what changed since the window opened.
            request = match settings.tab {
                Tab::Audit => Some(Admin::Audit {
                    action: settings.audit_filter.map(str::to_owned),
                    before: None,
                }),
                Tab::Bans => Some(Admin::Load {
                    joining: false,
                    bans: true,
                }),
                // Unsaved edits stay; only what's saved refreshes.
                Tab::Joining => Some(Admin::Load {
                    joining: true,
                    bans: false,
                }),
                Tab::Listing => Some(Admin::LoadListing),
                Tab::Files => Some(Admin::LoadFiles),
                Tab::History => Some(Admin::LoadHistory),
                Tab::RateLimits => Some(Admin::LoadRateLimits),
                Tab::NewMembers => Some(Admin::LoadNewMembers),
                Tab::Takedowns => Some(Admin::LoadTakedowns),
                Tab::Reports => Some(Admin::LoadReports),
                Tab::Overview if perms.owner => Some(Admin::LoadUpdates),
                Tab::Invites => Some(Admin::Invites),
                _ => None,
            };
        } else {
            request = changed;
        }
        close
    };
    // Confirmations over the settings window.
    if let Some(asked) = roles::delete_dialog(ctx, server, &mut settings.roles, &settings.message)
        .or_else(|| revoke_dialog(ctx, settings))
        .or_else(|| reports::confirm_dialog(ctx, &mut settings.report_confirm))
        .or_else(|| {
            let error = (settings.message.as_ref())
                .filter(|(_, error)| *error)
                .map(|(text, _)| text.as_str());
            let sticker = settings.tab == Tab::Stickers;
            emoji::dialogs(ctx, &server.dir, &mut settings.emoji, sticker, error)
        })
        .or_else(|| {
            (settings.history.as_mut())
                .and_then(|form| form.confirm(ctx))
                .map(Admin::SaveHistory)
        })
    {
        request = Some(asked);
    }
    if let Some(import) = &mut settings.import {
        match import.show(ctx, &server.dir) {
            import::Outcome::Open => {}
            import::Outcome::Close => settings.import = None,
            import::Outcome::Start(start) => request = Some(start),
        }
    }
    if let Some(request) = request {
        settings.message = None;
        settings.saving = match &request {
            Admin::Joining(open, cap) => Some(Saving::Joining((*open, *cap))),
            Admin::SaveListing(_) => settings.listing.clone().map(Saving::Listing),
            _ => settings.saving.take(),
        };
        server.send(Command::Admin(request));
    }
    for request in batch {
        server.send(Command::Admin(request));
    }
    if let Some(name) = rename {
        // The same change as "Rename server" in the server menu.
        server.send(Command::SetServerName(name));
    }
    if icon {
        settings.message = None;
        home.files.ask(ctx, Ask::Icon(server.address.clone()));
    }
    if let Some(sticker) = emoji_asked.pick {
        settings.message = None;
        home.files
            .ask(ctx, Ask::Emoji(server.address.clone(), sticker));
    }
    if let Some((slot, path, sticker)) = emoji_asked.crop {
        let target = crate::crop::Target::Emoji { sticker, slot };
        match crate::crop::Crop::open(target, &path) {
            Ok(crop) => home.crop = Some(crop),
            Err(problem) => settings.message = Some((problem.into(), true)),
        }
    }
    if let Some(file) = save_reported {
        settings.message = None;
        home.files.save_as_asked(ctx, &server.address, &file);
    }
    if emoji_asked.zip {
        settings.message = None;
        home.files.ask(ctx, Ask::EmojiZip(server.address.clone()));
    }
    let view_as = settings.roles.view_as.take();
    if close {
        home.server_settings = None;
    }
    if let Some(role) = view_as {
        home.server_settings = None;
        let server = &mut home.servers[index];
        crate::home::view_as::show_as(server, vec![role]);
        // Phones: the channel list first, where most of the difference shows.
        home.layout.list = true;
    }
    if let Some((person, pick)) = person_pick {
        // Their card, a poke or a chat belongs on top of the page, not under it.
        if !matches!(
            pick,
            Pick::Moderate(_) | Pick::Voice(_) | Pick::Befriend(_) | Pick::ShowVideo(_)
        ) {
            home.server_settings = None;
        }
        let now = ctx.input(|input| input.time);
        person::apply(home, index, person, pick, now);
    }
}

/// A spinner until the page's list arrives, unless loading it failed.
fn loading(ui: &mut Ui, settings: &Settings) {
    if settings.message.is_none() {
        ui.spinner();
    }
}

/// The last result, in red when it went wrong.
fn message(ui: &mut Ui, settings: &Settings) {
    if let Some((text, error)) = &settings.message {
        if *error {
            error_text(ui, Some(text));
        } else {
            ui.label(RichText::new(text).color(ui::colors(ui).success));
        }
    }
}

/// What was asked for in Overview.
struct Overview {
    system_messages: Option<crate::server::SystemMessages>,
    /// t!("admin.change_icon") was clicked.
    icon: bool,
    /// The new server name to save.
    rename: Option<String>,
    /// Events switched on or off.
    events: Option<bool>,
}

/// The server icon and name, for people who may manage the server.
fn overview_tab(ui: &mut Ui, server: &Server, settings: &mut Settings) -> Overview {
    let mut picked = Overview {
        system_messages: None,
        icon: false,
        rename: None,
        events: None,
    };
    let saved = server.dir.name.clone().unwrap_or_default();
    ui::page_section(ui, t!("admin.server"));
    let layout = if ui.available_width() < 220.0 {
        egui::Layout::top_down(egui::Align::Min)
    } else {
        egui::Layout::left_to_right(egui::Align::Min)
    };
    ui.with_layout(layout, |ui| {
        ui.vertical(|ui| {
            server_icon(ui, server, ICON_SIZE);
            picked.icon = ui
                .add_enabled_ui(crate::files::PICKER, |ui| {
                    ui::secondary(ui, t!("admin.change_icon"))
                })
                .inner
                .on_hover_text(t!("admin.icon_types"))
                .clicked();
        });
        ui.add_space(ui::SPACE_L);
        ui.vertical(|ui| {
            let label = ui.label(ui::bold(t!("home.server_name")));
            let name = settings.server_name.get_or_insert_with(|| saved.clone());
            let field = ui
                .add(
                    ui::field(name)
                        .char_limit(100)
                        .desired_width(ui.available_width()),
                )
                .labelled_by(label.id);
            let trimmed = name.trim();
            let changed = !trimmed.is_empty() && trimmed != saved;
            ui.weak(t!("admin.server_name_help"));
            let save = ui
                .add_enabled_ui(changed, |ui| ui::primary(ui, t!("common.save")))
                .inner
                .clicked();
            if changed && (save || crate::app::submitted(ui, &field)) {
                picked.rename = Some(trimmed.to_owned());
            }
        });
    });
    if let Some(calendar) = &server.dir.calendar {
        ui::page_section(ui, t!("events.title"));
        let mut on = calendar.enabled;
        if ui::toggle_row(ui, &mut on, t!("admin.events"), t!("admin.events_help")).changed() {
            picked.events = Some(on);
        }
    }
    ui.add_space(ui::SPACE_L);
    picked.system_messages = system_messages_row(ui, &server.dir);
    if server.dir.perms.owner {
        update_notice(ui, &settings.updates);
    }
    picked
}

fn system_messages_row(
    ui: &mut Ui,
    dir: &crate::server::Directory,
) -> Option<crate::server::SystemMessages> {
    use crate::server::SystemMessages;
    let label = t!("admin.system_messages");
    let help = if dir.system_messages_supported {
        t!("admin.system_help")
    } else {
        t!("admin.system_update")
    };
    let mut choice = dir.system_messages;
    let title = match choice {
        SystemMessages::First => t!("admin.system_first"),
        SystemMessages::None => t!("admin.system_none"),
        SystemMessages::Channel(id) => dir
            .text_channels()
            .find(|c| c.id == id)
            .map_or(t!("admin.system_missing"), |c| c.name.as_str()),
    };
    let narrow = ui.available_width() < 220.0;
    let width = if narrow {
        ui.available_width()
    } else {
        (ui.available_width() * 0.5).min(180.0)
    };
    let mut select = |ui: &mut Ui| {
        ui.add_enabled_ui(dir.system_messages_supported, |ui| {
            let combo = ComboBox::from_id_salt("system messages")
                .width(width)
                .truncate()
                .selected_text(title)
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut choice,
                        SystemMessages::First,
                        t!("admin.system_first"),
                    );
                    ui.selectable_value(&mut choice, SystemMessages::None, t!("admin.system_none"));
                    for ch in dir.text_channels() {
                        ui.selectable_value(&mut choice, SystemMessages::Channel(ch.id), &ch.name);
                    }
                });
            combo.response.widget_info(|| {
                egui::WidgetInfo::labeled(Role::ComboBox, dir.system_messages_supported, label)
            });
        });
    };
    if narrow {
        ui::setting_row(ui, label, help, |_| {});
        select(ui);
    } else {
        ui::setting_row(ui, label, help, select);
    }
    (choice != dir.system_messages).then_some(choice)
}

fn update_notice(ui: &mut Ui, updates: &ServerUpdates) {
    if updates.pending.is_none() && updates.last.is_none() {
        return;
    }
    ui::page_section(ui, t!("admin.server_updates"));
    if let Some(pending) = &updates.pending {
        ui.label(t!("admin.update_waiting", version = pending.version));
    }
    if let Some(last) = &updates.last {
        let date = date(last.at);
        ui.label(match last.outcome {
            ServerUpdateOutcome::Updated => {
                t!("admin.update_applied", version = last.version, date = date)
            }
            ServerUpdateOutcome::Failed => {
                t!("admin.update_failed", version = last.version, date = date)
            }
        });
    }
}

/// The server's icon, or its initials on a rounded square.
fn server_icon(ui: &mut Ui, server: &Server, size: f32) {
    let radius = ui::avatar_radius(size);
    match &server.dir.icon {
        Some(icon) => {
            let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::hover());
            crate::home::paint_hovered(ui, rect, icon, radius);
        }
        None => {
            let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::hover());
            let painter = ui.painter();
            painter.rect_filled(rect, radius, ui::accent(ui).base);
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                initials(server.name()),
                egui::FontId::new(size * 0.38, ui::bold_family()),
                ui::ON_FILL,
            );
        }
    }
}

fn members_tab<'a>(
    ui: &mut Ui,
    server: &Server,
    settings: &mut Settings,
    target: impl Fn(Id) -> Person<'a>,
) -> (Option<Admin>, Option<(Id, Pick)>) {
    let perms = server.dir.perms;
    let mut request = None;
    if let Some((person, action)) = &mut settings.member {
        // The page may be scrolled far down a long list: bring the question into view.
        if !settings.member_shown {
            ui.scroll_to_cursor(Some(egui::Align::TOP));
        }
        let name = server.member_name(person);
        let person = *person;
        let mut cancel = false;
        ui::card(ui, |ui| {
            match action {
                MemberAction::Ban { network, remove } => {
                    ui.label(ui::bold(t!("admin.ban_title", name = name)));
                    ui.label(t!("admin.ban_help"));
                    ui::checkbox(ui, network, t!("admin.ban_network"));
                    ui.weak(t!("admin.ban_network_help"));
                    ui::checkbox(ui, &mut settings.also_block, t!("admin.ban_also_block"));
                    if perms.remove_recent_messages {
                        remove_choice(ui, remove);
                    }
                    ui.horizontal(|ui| {
                        if danger(ui, t!("admin.ban")).clicked() {
                            let remove = (*remove > 0).then_some(*remove);
                            request = Some(Admin::Ban(person, *network, remove));
                            settings.block_after_ban = settings.also_block.then_some(person);
                        }
                        cancel = ui::secondary(ui, t!("common.cancel")).clicked();
                    });
                }
                MemberAction::Roles(chosen) => {
                    ui.label(ui::bold(t!("admin.roles_for", name = name)));
                    for role in server.dir.roles.iter().filter(|r| r.id != "owner") {
                        let mut on = chosen.contains(&role.id);
                        // Only roles below your own, with permissions you hold,
                        // can be given or taken away.
                        let allowed = role.rank < perms.rank
                            && (role.permissions.iter()).all(|p| roles::holds(&server.dir, p));
                        if ui
                            .add_enabled(allowed, ui::Checkbox::new(&mut on, &role.name))
                            .changed()
                        {
                            chosen.retain(|id| *id != role.id);
                            if on {
                                chosen.push(role.id.clone());
                            }
                        }
                    }
                    ui.horizontal(|ui| {
                        if ui::primary(ui, t!("common.save")).clicked() {
                            request = Some(Admin::SetRoles(person, chosen.clone()));
                        }
                        cancel = ui::secondary(ui, t!("common.cancel")).clicked();
                    });
                }
            }
        });
        if cancel {
            settings.member = None;
        }
    }
    settings.member_shown = settings.member.is_some();
    let pick = members::show(ui, &server.dir, &mut settings.members, target);
    (request, pick)
}

/// Active invites with who made them, uses left and expiry, each with t!("admin.revoke").
fn invites_tab(ui: &mut Ui, server: &Server, settings: &mut Settings) -> Option<Admin> {
    ui::page_section(ui, t!("admin.active_invites"));
    if !settings.invites_loaded {
        loading(ui, settings);
        return None;
    }
    if settings.invites.is_empty() {
        ui.weak(t!("admin.no_invites"));
        return None;
    }
    for invite in &settings.invites {
        let by = invite.creator.map_or_else(
            || t!("admin.someone").into(),
            |person| server.member_name(&person),
        );
        let help = t!(
            "admin.invite_uses",
            count = invite.uses,
            date = date(invite.expires_at)
        );
        ui.push_id(&invite.id, |ui| {
            ui::setting_row(ui, &t!("admin.made_by", name = by), &help, |ui| {
                if danger(ui, t!("admin.revoke")).clicked() {
                    settings.revoking = Some(invite.id.clone());
                    settings.message = None;
                }
            });
        });
    }
    None
}

/// t!("admin.revoke") asks first: the link stops working for everyone it was sent to.
fn revoke_dialog(ctx: &egui::Context, settings: &mut Settings) -> Option<Admin> {
    let id = settings.revoking.clone()?;
    let mut request = None;
    let mut cancel = false;
    let ((), dismissed) = ui::dialog(ctx, "revoke-invite", t!("admin.revoke_title"), |ui| {
        ui.label(t!("admin.revoke_help"));
        if let Some((text, true)) = &settings.message {
            error_text(ui, Some(text));
        }
        ui::footer(ui, |ui| {
            if danger(ui, t!("admin.revoke")).clicked() {
                request = Some(Admin::RevokeInvite(id));
            }
            cancel = ui::secondary(ui, t!("common.cancel")).clicked();
        });
    });
    if cancel || dismissed {
        settings.revoking = None;
        settings.message = None;
    }
    request
}

fn bans_tab(ui: &mut Ui, server: &Server, settings: &Settings) -> Option<Admin> {
    ui::page_section(ui, t!("admin.bans"));
    if !settings.bans_loaded {
        loading(ui, settings);
        return None;
    }
    if settings.bans.is_empty() {
        ui.weak(t!("admin.no_bans"));
        return None;
    }
    let mut request = None;
    for ban in &settings.bans {
        let when = if ban.network {
            t!("admin.banned_network", date = date(ban.at))
        } else {
            t!("admin.banned_at", date = date(ban.at))
        };
        let name = server.member_name(&ban.person);
        ui.push_id(ban.person, |ui| {
            ui::setting_row(ui, &name, &when, |ui| {
                let unban = ui
                    .add_enabled_ui(ban.can_lift, |ui| ui::secondary(ui, t!("admin.unban")))
                    .inner
                    .on_disabled_hover_text(t!("admin.unban_outranked"));
                if unban.clicked() {
                    request = Some(Admin::Unban(ban.person));
                }
            });
        });
    }
    request
}

fn audit_tab(ui: &mut Ui, server: &Server, settings: &mut Settings) -> Option<Admin> {
    let mut request = None;
    let shown = settings
        .audit_filter
        .and_then(|action| filters().into_iter().find(|(a, _)| *a == action))
        .map_or(t!("admin.everything"), |(_, label)| label);
    ui::page_section(ui, t!("admin.audit_log"));
    ui::setting_row(ui, t!("admin.show"), t!("admin.audit_help"), |ui| {
        let combo = ComboBox::from_id_salt("audit filter")
            .selected_text(shown)
            .show_ui(ui, |ui| {
                let mut filter = settings.audit_filter;
                ui.selectable_value(&mut filter, None, t!("admin.everything"));
                for (action, label) in filters() {
                    ui.selectable_value(&mut filter, Some(action), label);
                }
                if filter != settings.audit_filter {
                    settings.audit_filter = filter;
                    (settings.audit, settings.audit_next) = (None, None);
                    request = Some(Admin::Audit {
                        action: filter.map(str::to_owned),
                        before: None,
                    });
                }
            });
        combo
            .response
            .widget_info(|| egui::WidgetInfo::labeled(Role::ComboBox, true, t!("admin.show")));
    });
    if let Some(text) = &settings.audit_error {
        error_text(ui, Some(text));
    }
    let Some(audit) = &settings.audit else {
        if settings.audit_error.is_none() {
            ui.spinner();
        }
        return request;
    };
    if audit.is_empty() {
        ui.weak(t!("admin.nothing_yet"));
        return request;
    }
    for entry in audit {
        ui.horizontal_wrapped(|ui| {
            ui.label(sentence(server, entry));
            if let Some(details) = details(server, entry) {
                ui.weak(details);
            }
            if matches!(entry.action.as_str(), "device_added" | "device_revoked")
                && let Some(id) = entry.target_id
            {
                ui.weak(t!(
                    "audit.device_id",
                    fingerprint = rusttalk_core::identity::fingerprint(&id)
                ));
            }
            ui.weak(date(entry.at));
        });
    }
    if let Some(before) = settings.audit_next
        && ui::secondary(ui, t!("admin.show_older")).clicked()
    {
        // Hidden until the page arrives, so a second click can't fetch it twice.
        settings.audit_next = None;
        request = Some(Admin::Audit {
            action: settings.audit_filter.map(str::to_owned),
            before: Some(before),
        });
    }
    request
}

/// One audit entry as a plain sentence, with names decrypted on this device.
fn sentence(server: &Server, entry: &AuditEntry) -> String {
    let actor = server.member_name(&entry.actor);
    let target = entry.target.map_or_else(
        || t!("admin.someone_lower").into(),
        |person| server.member_name(&person),
    );
    match entry.action.as_str() {
        "member_banned" => t!("audit.member_banned", actor = actor, target = target),
        "member_unbanned" => t!("audit.member_unbanned", actor = actor, target = target),
        "member_kicked" => t!("audit.member_kicked", actor = actor, target = target),
        "voice_muted" => t!("audit.voice_muted", actor = actor, target = target),
        "member_timed_out" => match entry.details["seconds"].as_i64() {
            Some(seconds) => t!(
                "audit.member_timed_out",
                actor = actor,
                target = target,
                length = timeout::label(seconds)
            ),
            None => t!(
                "audit.member_timed_out_plain",
                actor = actor,
                target = target
            ),
        },
        "member_timeout_ended" => {
            t!("audit.member_timeout_ended", actor = actor, target = target)
        }
        "voice_unmuted" => t!("audit.voice_unmuted", actor = actor, target = target),
        "voice_removed" => t!("audit.voice_removed", actor = actor, target = target),
        "voice_moved" => t!("audit.voice_moved", actor = actor, target = target),
        "left" => t!("audit.left", actor = actor),
        "member_roles_changed" => t!("audit.member_roles_changed", actor = actor, target = target),
        "server_updated" => t!("audit.server_updated", actor = actor),
        "file_limits_changed" => t!("audit.file_limits_changed", actor = actor),
        "new_member_rules_changed" => t!("audit.new_member_rules_changed", actor = actor),
        "listing_changed" if entry.details["enabled"] == true => t!("audit.listed", actor = actor),
        "listing_changed" => t!("audit.unlisted", actor = actor),
        "reports_dismissed" | "dm_reports_dismissed" => t!(
            "audit.reports_dismissed",
            actor = actor,
            target = target,
            count = entry.details["reports"].as_i64().unwrap_or_default()
        ),
        "report_resolved" | "dm_report_resolved" => match entry.details["resolution"].as_str() {
            Some("dismissed") => t!("audit.report_dismissed", actor = actor, target = target),
            Some("deleted") => t!("audit.report_deleted", actor = actor, target = target),
            Some("timed_out") => t!("audit.report_timed_out", actor = actor, target = target),
            Some("kicked") => t!("audit.report_kicked", actor = actor, target = target),
            _ => t!("audit.report_banned", actor = actor, target = target),
        },
        "lockdown_started" => t!("audit.lockdown_started", actor = actor),
        "lockdown_ended" => t!("audit.lockdown_ended", actor = actor),
        "lockdown_message_changed" => t!("audit.lockdown_message_changed", actor = actor),
        "auto_lockdown_changed" => t!("audit.auto_lockdown_changed", actor = actor),
        "lockdown_auto_started" => t!("audit.lockdown_auto_started").into(),
        "content_taken_down" => match entry.target {
            Some(_) => t!("audit.content_taken_down", actor = actor, target = target),
            None => t!("audit.content_taken_down_many", actor = actor),
        },
        "recent_messages_removed" => {
            t!(
                "audit.recent_messages_removed",
                actor = actor,
                target = target
            )
        }
        "ownership_transferred" => t!(
            "audit.ownership_transferred",
            actor = actor,
            target = target
        ),
        "device_added" => t!("notice.added_device", name = actor),
        "device_revoked" => t!("notice.removed_device", name = actor),
        "joined" => t!("audit.joined", actor = actor),
        "role_created" => t!("audit.role_created", actor = actor),
        "role_renamed" => t!("audit.role_renamed", actor = actor),
        "role_changed" => t!("audit.role_changed", actor = actor),
        "role_deleted" => t!("audit.role_deleted", actor = actor),
        "default_role_changed" => t!("audit.default_role_changed", actor = actor),
        "channel_created" => t!("audit.channel_created", actor = actor),
        "channel_updated" => t!("audit.channel_updated", actor = actor),
        "channel_deleted" => t!("audit.channel_deleted", actor = actor),
        "channel_permissions_changed" => match entry.details["role_id"].as_str() {
            Some(role) => t!(
                "audit.channel_role_permissions",
                actor = actor,
                role = role_name(server, role),
                channel = entry
                    .target_id
                    .and_then(|id| server.channel(&id))
                    .map_or_else(|| t!("audit.a_channel").into(), |c| format!("#{}", c.name))
            ),
            None => t!("audit.channel_permissions_changed", actor = actor),
        },
        "slow_mode_changed" => t!(
            "audit.slow_mode_changed",
            actor = actor,
            channel = entry
                .target_id
                .and_then(|id| server.channel(&id))
                .map_or_else(|| t!("audit.a_channel").into(), |c| format!("#{}", c.name))
        ),
        "nsfw_changed" => {
            let channel = entry
                .target_id
                .and_then(|id| server.channel(&id))
                .map_or_else(|| t!("audit.a_channel").into(), |c| format!("#{}", c.name));
            if entry.details["after"] == true {
                t!("audit.nsfw_on", actor = actor, channel = channel)
            } else {
                t!("audit.nsfw_off", actor = actor, channel = channel)
            }
        }
        "category_created" => t!("audit.category_created", actor = actor),
        "category_updated" => t!("audit.category_updated", actor = actor),
        "category_deleted" => t!("audit.category_deleted", actor = actor),
        "channels_reordered" => t!("audit.channels_reordered", actor = actor),
        "focus_started" => t!("audit.focus_started", actor = actor),
        "focus_ended" => t!("audit.focus_ended", actor = actor),
        "events_enabled" => t!("audit.events_enabled", actor = actor),
        "events_disabled" => t!("audit.events_disabled", actor = actor),
        "event_created" => t!("audit.event_created", actor = actor),
        "event_approved" => t!("audit.event_approved", actor = actor, target = target),
        "event_declined" => t!("audit.event_declined", actor = actor, target = target),
        "event_cancelled" => t!("audit.event_cancelled", actor = actor),
        "invite_created" => t!("audit.invite_created", actor = actor),
        "invite_revoked" => t!("audit.invite_revoked", actor = actor),
        "open_joining_changed" => t!("audit.open_joining_changed", actor = actor),
        "message_deleted" => t!("audit.message_deleted", actor = actor),
        "file_deleted" => t!("audit.file_deleted", actor = actor),
        "emoji_added" => t!("audit.emoji_added", actor = actor),
        "emoji_renamed" => t!("audit.emoji_renamed", actor = actor),
        "emoji_deleted" => t!("audit.emoji_deleted", actor = actor),
        "emoji_pack_changed" => t!("audit.emoji_pack_changed", actor = actor),
        "device_certificate_changed" => t!("audit.device_certificate_changed", actor = actor),
        "owner_claimed" => t!("audit.owner_claimed", actor = actor),
        other => format!("{actor}: {}", other.replace('_', " ")),
    }
}

/// What an entry changed, from the IDs, flags and numbers the server recorded with it.
fn details(server: &Server, entry: &AuditEntry) -> Option<String> {
    let details = &entry.details;
    let roles = |key: &str| -> Vec<String> {
        details[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|id| id.as_str())
            .map(|id| role_name(server, id))
            .collect()
    };
    let parts: Vec<String> = match entry.action.as_str() {
        "member_roles_changed" => changed(roles("added"), roles("removed")),
        "channel_permissions_changed" => {
            let flags = |key: &str| details[key].as_u64().map(|flags| flags as u8);
            let (before, after) = (flags("before")?, flags("after")?);
            let page = entry
                .target_id
                .and_then(|id| server.channel(&id))
                .is_some_and(|channel| channel.page);
            let names = |flags: u8| -> Vec<String> {
                [
                    (1, t!("admin.flag_read")),
                    (
                        2,
                        if page {
                            t!("admin.flag_edit")
                        } else {
                            t!("admin.flag_write")
                        },
                    ),
                    (4, t!("admin.flag_speak")),
                    (8, t!("admin.flag_video")),
                ]
                .into_iter()
                .filter(|(bit, _)| flags & bit != 0)
                .map(|(_, name)| name.to_owned())
                .collect()
            };
            changed(names(after & !before), names(before & !after))
        }
        "server_updated" => details["changed"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|part| match part.as_str()? {
                "name" => Some(t!("home.server_name")),
                "description" => Some(t!("audit.part_description")),
                "icon" => Some(t!("audit.part_icon")),
                "system_messages" => Some(t!("admin.system_messages")),
                _ => None,
            })
            .map(str::to_owned)
            .collect(),
        "slow_mode_changed" => {
            let secs = |key: &str| details[key].as_u64().map(|secs| secs as u32);
            let (before, after) = (secs("before")?, secs("after")?);
            let label = crate::home::slow_mode::label;
            vec![format!("{} → {}", label(before), label(after))]
        }
        "file_limits_changed" => {
            let limits = |key: &str| serde_json::from_value(details[key].clone()).ok();
            file_limits::changes(limits("before")?, limits("after")?)
        }
        "new_member_rules_changed" => {
            let (before, after) = (details["before"].as_i64()?, details["after"].as_i64()?);
            let label = new_members::label;
            vec![format!("{} → {}", label(before), label(after))]
        }
        "auto_lockdown_changed" => {
            let spike = |key: &str| spike(&details[key]);
            vec![format!("{} → {}", spike("before")?, spike("after")?)]
        }
        "lockdown_auto_started" => vec![spike(details)?],
        "content_taken_down" => vec![t!(
            "audit.removed_counts",
            messages = details["messages"].as_u64()?,
            files = details["files"].as_u64()?
        )],
        "recent_messages_removed" => vec![t!(
            "audit.removed_counts",
            messages = details["messages"].as_u64()?,
            files = details["files"].as_u64()?
        )],
        _ => Vec::new(),
    };
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// Automatic lockdown's `{joins, secs}` in words: "10 joins in 2 min", or "Off".
fn spike(value: &serde_json::Value) -> Option<String> {
    let joins = value["joins"].as_u64()?;
    let minutes = value["secs"].as_u64()? / 60;
    Some(match joins {
        0 => t!("lockdown.auto_off").into(),
        _ => t!("lockdown.spike", joins = joins, minutes = minutes),
    })
}

/// "+ added, − removed".
fn changed(added: Vec<String>, removed: Vec<String>) -> Vec<String> {
    let added = added.into_iter().map(|name| format!("+ {name}"));
    added
        .chain(removed.into_iter().map(|name| format!("− {name}")))
        .collect()
}

fn role_name(server: &Server, id: &str) -> String {
    server
        .dir
        .roles
        .iter()
        .find(|role| role.id == id && role.name_known)
        .map_or_else(|| t!("audit.unknown_role").into(), |role| role.name.clone())
}

/// "Remove their messages from the last": nothing, an hour, a day or a week, in seconds.
fn remove_choice(ui: &mut Ui, remove: &mut i64) {
    let choices = [
        (0, t!("admin.ban_remove_none")),
        (3_600, t!("admin.ban_remove_hour")),
        (86_400, t!("admin.ban_remove_day")),
        (604_800, t!("admin.ban_remove_week")),
    ];
    let shown = (choices.iter())
        .find(|(secs, _)| secs == remove)
        .map_or(choices[0].1, |(_, label)| label);
    let label = ui.label(t!("admin.ban_remove"));
    ComboBox::from_id_salt("ban-remove-recent")
        .selected_text(shown)
        .show_ui(ui, |ui| {
            for (secs, label) in choices {
                ui.selectable_value(remove, secs, label);
            }
        })
        .response
        .labelled_by(label.id);
    ui.weak(t!("admin.ban_remove_help"));
}

fn joining_tab(ui: &mut Ui, settings: &mut Settings) -> Option<Admin> {
    ui::page_section(ui, t!("admin.joining"));
    let Some((open, cap)) = &mut settings.joining else {
        ui.spinner();
        return None;
    };
    ui::toggle_row(
        ui,
        open,
        t!("admin.open_joining"),
        t!("admin.open_joining_help"),
    );
    ui::setting_row(
        ui,
        t!("admin.member_limit"),
        t!("admin.member_limit_help"),
        |ui| {
            // The server takes any limit from 1 up; u32 is what the app can send.
            ui.add(DragValue::new(cap).range(1..=u32::MAX))
                .widget_info(|| {
                    egui::WidgetInfo::labeled(Role::SpinButton, true, t!("admin.member_limit"))
                });
        },
    );
    let request = Admin::Joining(*open, *cap);
    let unsaved = settings.joining_unsaved();
    ui.add_enabled_ui(unsaved, |ui| ui::primary(ui, t!("common.save")))
        .inner
        .clicked()
        .then_some(request)
}

fn listing_tab(ui: &mut Ui, server: &Server, settings: &mut Settings) -> Option<Admin> {
    ui::page_section(ui, t!("admin.public_listing"));
    let Some((on, listing)) = &mut settings.listing else {
        ui.spinner();
        return None;
    };
    let help = if *on {
        t!("admin.listed_on")
    } else {
        t!("admin.listed_off")
    };
    ui::toggle_row(ui, on, t!("admin.list_publicly"), help);
    ui.hyperlink_to(
        t!("admin.directory_rules"),
        crate::official::DIRECTORY_RULES,
    );
    ui.add_space(ui::SPACE_S);
    if let Some(delisted) = settings.delisted.as_ref().filter(|_| *on) {
        removed_notice(ui, delisted);
    }
    if !*on {
        return ui::primary(ui, t!("common.save"))
            .clicked()
            .then_some(Admin::SaveListing(None));
    }
    // Start from what people already see.
    if listing.name.is_empty() && listing.address.is_empty() {
        listing.name = server.dir.name.clone().unwrap_or_default();
        listing.address = server.address.to_string();
        listing.index_url = crate::official::DIRECTORY.into();
    }
    ui::banner(ui, "public-listing", t!("admin.listing_banner"));
    ui::page_section(ui, t!("common.details"));
    let field = |ui: &mut Ui, label: &str, text: &mut String, limit: usize, lines: bool| {
        let label = ui.label(label);
        let edit = if lines {
            ui::text_area(text).desired_rows(3)
        } else {
            ui::field(text)
        };
        ui.add(edit.char_limit(limit).desired_width(f32::INFINITY))
            .labelled_by(label.id);
    };
    field(ui, t!("admin.public_name"), &mut listing.name, 100, false);
    field(
        ui,
        t!("admin.public_description"),
        &mut listing.description,
        1_000,
        true,
    );
    field(
        ui,
        t!("admin.server_address"),
        &mut listing.address,
        512,
        false,
    );
    field(
        ui,
        t!("home.directory_address"),
        &mut listing.index_url,
        512,
        false,
    );
    CollapsingHeader::new(t!("common.details")).show(ui, |ui| {
        field(
            ui,
            t!("admin.directory_fingerprint"),
            &mut listing.index_fingerprint,
            64,
            false,
        );
    });
    if !ui::primary(ui, t!("common.save")).clicked() {
        return None;
    }
    match check_listing(listing) {
        Ok(()) => Some(Admin::SaveListing(Some(listing.clone()))),
        Err(problem) => {
            settings.message = Some((problem.into(), true));
            None
        }
    }
}

/// The directory took this server out: why, and whom to ask to undo it.
fn removed_notice(ui: &mut Ui, delisted: &Delisted) {
    let c = ui::colors(ui);
    egui::Frame::new()
        .stroke(egui::Stroke::new(1.0, c.danger))
        .corner_radius(ui::RADIUS)
        .inner_margin(egui::Margin::same(ui::SPACE_M as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(ui::bold(t!("admin.delisted_title")).color(c.danger_text));
            ui.add(egui::Label::new(t!("admin.delisted_help")).wrap());
            if let Some(reason) = &delisted.reason {
                ui.add(egui::Label::new(t!("admin.delisted_reason", reason = reason)).wrap());
            }
            let appeal = match &delisted.contact {
                Some(contact) => t!("admin.delisted_contact", contact = contact),
                None => t!("admin.delisted_no_contact").to_owned(),
            };
            ui.add(egui::Label::new(appeal).wrap().selectable(true));
        });
    ui.add_space(ui::SPACE_S);
}

/// The same checks the directory makes, so mistakes show before saving.
fn check_listing(listing: &mut PublicListing) -> Result<(), &'static str> {
    for text in [
        &mut listing.name,
        &mut listing.address,
        &mut listing.index_url,
        &mut listing.index_fingerprint,
    ] {
        *text = text.trim().to_owned();
    }
    validate_details(&listing.name, &listing.description, &listing.address)
        .map_err(|_| t!("admin.listing_invalid"))?;
    https_url(&listing.index_url).ok_or(t!("admin.directory_https"))?;
    if !listing.index_fingerprint.is_empty() && decode::<32>(&listing.index_fingerprint).is_err() {
        return Err(t!("home.bad_fingerprint"));
    }
    Ok(())
}

/// Role names line up so the toggles after them form columns.
const ROLE_NAME_WIDTH: f32 = 104.0;
const CHIP_HEIGHT: f32 = 26.0;

/// Who sees a channel and what each role may do there, for the Edit channel dialog.
/// Changes wait in `pending` (role and flags) until the dialog is saved.
pub(crate) fn channel_flags(
    ui: &mut Ui,
    server: &Server,
    channel: &Channel,
    pending: &mut Vec<(String, u8)>,
    height: f32,
) {
    let top = ui.cursor().top();
    let flags_of = |pending: &Vec<(String, u8)>, role: &str| {
        pending
            .iter()
            .find(|(id, _)| id == role)
            .map_or_else(|| channel_role_flags(channel, role), |(_, flags)| *flags)
    };
    let set = |pending: &mut Vec<(String, u8)>, role: &str, flags: u8| {
        pending.retain(|(id, _)| id != role);
        pending.push((role.to_owned(), flags));
    };
    // (bit, glyph, label): what the channel's kind offers.
    let columns: &[(u8, &str, &str)] = if channel.voice {
        &[
            (1, "👁", t!("admin.flag_read")),
            (2, "✏", t!("admin.flag_write")),
            (4, "🎤", t!("admin.flag_speak")),
            (8, "📷", t!("admin.flag_video")),
        ]
    } else if channel.page {
        // Writing in a page channel is changing the page.
        &[
            (1, "👁", t!("admin.flag_read")),
            (2, "✏", t!("admin.flag_edit")),
        ]
    } else {
        &[
            (1, "👁", t!("admin.flag_read")),
            (2, "✏", t!("admin.flag_write")),
        ]
    };
    // The default role is everyone, so whether it can read is whether the channel is open.
    let everyone = &server.dir.default_role;
    let mut open = flags_of(pending, everyone) & 1 != 0;
    let was_open = open;
    ui.label(ui::bold(t!("admin.who_can_see_channel")));
    ui.add_space(ui::SPACE_XS);
    let choices = [
        (
            true,
            t!("admin.channel_everyone"),
            t!("admin.channel_everyone_hint"),
        ),
        (
            false,
            t!("admin.channel_some_roles"),
            t!("admin.channel_some_roles_hint"),
        ),
    ];
    if ui::segmented(ui, &mut open, &choices) && open != was_open {
        // Opening gives everyone what a new channel gives them; closing takes it all.
        let flags = match open {
            false => 0,
            true if channel.page => 1,
            true => columns.iter().fold(0, |all, (bit, ..)| all | bit),
        };
        set(pending, everyone, flags);
    }
    ui.add_space(ui::SPACE_M);
    ui::caption_label(ui, t!("admin.what_roles_can_do"));
    ui.add_space(ui::SPACE_XS);
    let room = (height - (ui.cursor().top() - top)).max(120.0);
    egui::ScrollArea::vertical()
        .max_height(room)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for role in &server.dir.roles {
                let flags = flags_of(pending, &role.id);
                ui::card_frame(ui)
                    .inner_margin(egui::Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = ui::SPACE_XS;
                            ui.spacing_mut().button_padding = egui::vec2(7.0, 2.0);
                            let size = egui::vec2(ROLE_NAME_WIDTH, CHIP_HEIGHT);
                            let centred = egui::Layout::left_to_right(egui::Align::Center);
                            ui.allocate_ui_with_layout(size, centred, |ui| {
                                ui.set_min_size(size);
                                ui::color_dot(ui, role.color);
                                // A role that can't see the channel is greyed out.
                                let mut name = ui::bold(&role.name);
                                name = match role.color {
                                    _ if flags & 1 == 0 => name.color(ui::colors(ui).muted),
                                    Some(color) => name.color(color),
                                    None => name,
                                };
                                ui.add(egui::Label::new(name).truncate());
                            });
                            for (bit, glyph, label) in columns {
                                let on = flags & bit != 0;
                                let text =
                                    RichText::new(format!("{glyph} {label}")).size(ui::TEXT_SMALL);
                                let chip =
                                    ui::choice_button(ui, on, text, egui::vec2(0.0, CHIP_HEIGHT))
                                        .on_hover_text(format!("{} · {label}", role.name));
                                if chip.clicked() {
                                    // Everything else needs Read; taking Read away takes the rest too.
                                    let flags = match (on, *bit) {
                                        (true, 1) => 0,
                                        (false, bit) => flags | bit | 1,
                                        (true, bit) => flags & !bit,
                                    };
                                    set(pending, &role.id, flags);
                                }
                            }
                        });
                        if flags & 1 == 0 {
                            ui.label(ui::caption(t!("admin.role_cannot_see")));
                        }
                    });
                ui.add_space(ui::SPACE_XS);
            }
        });
}

/// What a role may do in a channel, as saved on the server.
pub(crate) fn channel_role_flags(channel: &Channel, role: &str) -> u8 {
    channel
        .flags
        .iter()
        .find(|(id, _)| id == role)
        .map_or(0, |(_, flags)| *flags)
}

fn date(at: i64) -> String {
    DateTime::from_timestamp(at, 0).map_or_else(String::new, |at| {
        crate::i18n::time(at.with_timezone(&Local), "time.date_clock")
    })
}
