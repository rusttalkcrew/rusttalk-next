//! Server and channel settings. The server decides who can make each change.

mod file_limits;

mod import;

mod members;

mod roles;

use chrono::{DateTime, Local};
use eframe::egui::{
    self, CollapsingHeader, ComboBox, DragValue, Grid, RichText, Ui, accesskit::Role,
};

use rusttalk_core::{
    auth::{ServerUpdateOutcome, ServerUpdates, decode},
    index::{https_url, validate_details},
};

use crate::{
    files::Ask,
    home::{
        Home, initials, paint_hovered,
        person::{self, Person, Pick},
    },
    net::{Admin, AdminUpdate, AuditEntry, Ban, Command, Invite, PublicListing},
    server::{Channel, Id, Perms, Server},
    settings::{self as panel, Group, Page},
    ui::{self, Item, danger, error_text},
};

const ICON_SIZE: f32 = 80.0;
const HEADER_ICON: f32 = 40.0;

/// Audit log entries shown per t!("admin.show_older").
const AUDIT_PAGE: usize = 50;

/// Audit log filters: API action and label.
fn filters() -> [(&'static str, &'static str); 9] {
    [
        ("member_banned", t!("audit.filter_bans")),
        ("member_unbanned", t!("audit.filter_unbans")),
        ("member_kicked", t!("audit.filter_kicks")),
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
    /// The owner's public listing.
    Listing,
    /// Server emoji and stickers.
    Emoji,
}

enum MemberAction {
    Kick,
    Ban { network: bool },
    Roles(Vec<String>),
}

pub struct Settings {
    pub server: usize,
    tab: Tab,
    bans: Vec<Ban>,
    audit: Vec<AuditEntry>,
    audit_shown: usize,
    audit_filter: Option<&'static str>,
    /// Open joining and member limit, once loaded (owners and admins).
    joining: Option<(bool, u32)>,
    files: Option<file_limits::Form>,
    /// Whether the server is listed and the details, once loaded (owner).
    listing: Option<(bool, PublicListing)>,
    updates: ServerUpdates,
    member: Option<(Id, MemberAction)>,
    /// The kick, ban or roles question was already on screen last frame.
    member_shown: bool,
    members: members::View,
    roles: roles::RolesPage,
    invites: Vec<Invite>,
    /// The invite waiting for t!("admin.revoke") to be confirmed.
    revoking: Option<String>,
    /// The server name being typed in Overview; `None` until edited.
    server_name: Option<String>,
    /// The name for the next emoji or (`emoji_sticker`) sticker upload.
    emoji_name: String,
    emoji_sticker: bool,
    /// An emoji or sticker being renamed, and the new name.
    emoji_renaming: Option<(Id, String)>,
    /// The pack the next upload goes to; the first when `None`.
    emoji_pack: Option<Id>,
    /// The name typed for a new pack.
    new_pack: String,
    /// A pack being renamed, and the new name.
    pack_renaming: Option<(Id, String)>,
    /// The pack waiting for its delete to be confirmed.
    pack_deleting: Option<Id>,
    /// "Import pack from .zip", while its dialog is open.
    import: Option<import::Import>,
    /// The last result: text and whether it went wrong.
    message: Option<(String, bool)>,
}

impl Settings {
    pub fn apply(&mut self, update: AdminUpdate) {
        match update {
            AdminUpdate::Bans(bans) => self.bans = bans,
            AdminUpdate::Audit(entries) => {
                self.audit = entries;
                self.audit_shown = AUDIT_PAGE;
            }
            AdminUpdate::Invites(invites) => self.invites = invites,
            AdminUpdate::Joining(open, cap) => self.joining = Some((open, cap)),
            AdminUpdate::Files(settings) => self.files = Some(file_limits::Form::new(settings)),
            AdminUpdate::Listing(on, saved) => {
                self.listing = Some((on, saved.unwrap_or_default()));
            }
            AdminUpdate::ServerUpdates(updates) => self.updates = updates,
            AdminUpdate::Progress(done, total) => {
                if let Some(import) = &mut self.import {
                    import.progress(done, total);
                }
            }
            AdminUpdate::Done(text) => {
                self.member = None;
                self.roles.done();
                self.revoking = None;
                self.emoji_renaming = None;
                self.pack_renaming = None;
                self.pack_deleting = None;
                self.import = None;
                self.message = Some((text.into(), false));
            }
            AdminUpdate::Failed(text) => {
                self.roles.failed();
                if let Some(import) = &mut self.import {
                    import.failed();
                }
                self.message = Some((text, true));
            }
        }
    }
}

/// Start "Import pack from .zip" on the open server settings, with the picked file.
pub fn start_import(home: &mut Home, path: &std::path::Path, ctx: &egui::Context) {
    let Some(settings) = &mut home.server_settings else {
        return;
    };
    let dir = &home.servers[settings.server].dir;
    let max = dir.emoji_max.0.max(dir.emoji_max.1);
    settings.import = Some(import::Import::start(path, max, ctx));
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
        } else {
            Tab::Files
        };
    }
    server.send(Command::Admin(Admin::Load {
        joining: perms.admin,
        audit: perms.audit,
        bans: perms.ban,
    }));
    if tab == Tab::Overview && perms.owner {
        server.send(Command::Admin(Admin::LoadUpdates));
    }
    if tab == Tab::Invites {
        server.send(Command::Admin(Admin::Invites));
    }
    if tab == Tab::Files {
        server.send(Command::Admin(Admin::LoadFiles));
    }
    home.server_settings = Some(Settings {
        server: index,
        tab,
        bans: Vec::new(),
        audit: Vec::new(),
        audit_shown: AUDIT_PAGE,
        audit_filter: None,
        joining: None,
        files: None,
        listing: None,
        updates: ServerUpdates::default(),
        member: None,
        member_shown: false,
        members: members::View::default(),
        roles: roles::RolesPage::default(),
        invites: Vec::new(),
        revoking: None,
        server_name: None,
        emoji_name: String::new(),
        emoji_sticker: false,
        emoji_renaming: None,
        emoji_pack: None,
        new_pack: String::new(),
        pack_renaming: None,
        pack_deleting: None,
        import: None,
        message: None,
    });
}

#[derive(Clone, Copy)]
pub enum Moderate {
    Roles,
    Kick,
    Ban,
}

/// Menu items to kick, ban or change the roles of someone else, as allowed.
pub fn member_menu(m: &mut ui::Menu, choice: &mut Option<Moderate>, perms: Perms) {
    if perms.manage_roles && m.item("🏷", t!("admin.change_roles")).clicked() {
        *choice = Some(Moderate::Roles);
    }
    if perms.kick && m.add(Item::new("👢", t!("admin.kick")).danger()).clicked() {
        *choice = Some(Moderate::Kick);
    }
    if perms.ban && m.add(Item::new("🚫", t!("admin.ban")).danger()).clicked() {
        *choice = Some(Moderate::Ban);
    }
}

/// Show the server settings with a kick, ban or role change ready to confirm.
pub fn moderate(home: &mut Home, index: usize, person: Id, choice: Moderate) {
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
        Moderate::Kick => MemberAction::Kick,
        Moderate::Ban => MemberAction::Ban { network: false },
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
    let index = settings.server;
    // `Person` reads these two parts of `home` while `settings` borrows another.
    let (friends, voice) = (&home.friends, &home.voice);
    let mut person_pick = None;
    let mut request;
    let mut icon = false;
    let mut upload = None;
    let mut zip = false;
    let mut rename = None;
    let mut batch = Vec::new();
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
                    page(
                        Tab::Emoji,
                        "😀",
                        t!("admin.emoji_stickers"),
                        perms.manage_emoji,
                    ),
                    page(Tab::Listing, "🌐", t!("admin.public_listing"), perms.owner),
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
                vec![page(Tab::Audit, "📜", t!("admin.audit_log"), perms.audit)],
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
                    Tab::Listing => listing_tab(ui, server, settings),
                    Tab::Emoji => {
                        let asked = emoji_tab(ui, server, settings);
                        (upload, zip) = (asked.upload, asked.zip);
                        asked.request
                    }
                };
                message(ui, settings);
                changed
            },
        );
        if tab != before {
            settings.tab = tab;
            settings.message = None;
            // Show what changed since the window opened.
            request = match settings.tab {
                Tab::Audit => Some(Admin::Audit(settings.audit_filter.map(str::to_owned))),
                Tab::Bans => Some(Admin::Load {
                    joining: false,
                    audit: false,
                    bans: true,
                }),
                Tab::Listing => Some(Admin::LoadListing),
                Tab::Files => Some(Admin::LoadFiles),
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
    if let Some((name, sticker, pack)) = upload {
        settings.message = None;
        settings.emoji_name.clear();
        home.files
            .ask(ctx, Ask::Emoji(server.address.clone(), name, sticker, pack));
    }
    if zip {
        settings.message = None;
        home.files.ask(ctx, Ask::EmojiZip(server.address.clone()));
    }
    if close {
        home.server_settings = None;
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
    match &server.dir.icon {
        Some(icon) => {
            let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::hover());
            crate::home::paint_hovered(ui, rect, icon, size / 4.0);
        }
        None => {
            let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::hover());
            let painter = ui.painter();
            painter.rect_filled(rect, size / 4.0, ui::accent(ui).base);
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

/// Names people type as `:name:`: 2 to 32 lowercase letters, digits or `_`.
fn emoji_name_ok(name: &str) -> bool {
    crate::emoji::name_ok(name)
}

/// Keep a typed name to the characters a `:name:` allows.
fn clean_emoji_name(name: &mut String) {
    *name = crate::emoji::clean_name(name);
}

/// What the Emoji & stickers tab asks for this frame.
#[derive(Default)]
struct EmojiAsk {
    /// A name, kind and pack to pick a picture for.
    upload: Option<(String, bool, Option<Id>)>,
    request: Option<Admin>,
    /// Pick a .zip to import as a pack.
    zip: bool,
}

/// Upload, import, rename, move and delete server emoji and stickers, and manage the
/// packs they are in.
fn emoji_tab(ui: &mut Ui, server: &Server, settings: &mut Settings) -> EmojiAsk {
    let dir = &server.dir;
    let mut ask = EmojiAsk::default();
    let count = |sticker: bool| dir.emoji.iter().filter(|e| e.sticker == sticker).count();
    ui::page_section(ui, t!("admin.upload"));
    ui.horizontal(|ui| {
        for (sticker, label) in [(false, t!("admin.emoji")), (true, t!("admin.sticker"))] {
            if ui::choice_button(
                ui,
                settings.emoji_sticker == sticker,
                label,
                egui::Vec2::ZERO,
            )
            .clicked()
            {
                settings.emoji_sticker = sticker;
            }
        }
    });
    let sticker = settings.emoji_sticker;
    let pack = (settings.emoji_pack)
        .filter(|id| dir.packs.iter().any(|pack| pack.id == *id))
        .or_else(|| dir.packs.first().map(|pack| pack.id));
    let label = ui.label(ui::bold(t!("common.name")));
    ui.horizontal(|ui| {
        let field = ui
            .add(ui::field(&mut settings.emoji_name).hint_text("party_parrot"))
            .labelled_by(label.id);
        if field.changed() {
            clean_emoji_name(&mut settings.emoji_name);
        }
        let name = settings.emoji_name.as_str();
        let taken = dir
            .emoji
            .iter()
            .any(|e| e.sticker == sticker && e.name == name);
        let max = if sticker {
            dir.emoji_max.1
        } else {
            dir.emoji_max.0
        };
        let full = count(sticker) >= max;
        let ready = emoji_name_ok(name) && !taken && !full && crate::files::PICKER;
        let pick = ui
            .add_enabled_ui(ready, |ui| ui::primary(ui, t!("admin.pick_picture")))
            .inner
            .clicked();
        if pick || (ready && crate::app::submitted(ui, &field)) {
            ask.upload = Some((name.to_owned(), sticker, pack));
        }
        if taken {
            ui.label(RichText::new(t!("admin.name_taken")).color(ui::colors(ui).danger_text));
        } else if full {
            ui.label(RichText::new(t!("admin.emoji_full")).color(ui::colors(ui).danger_text));
        }
    });
    if dir.packs.len() > 1 {
        ui.horizontal(|ui| {
            let label = ui.label(ui::bold(t!("admin.pack")));
            let shown = pack
                .and_then(|id| dir.packs.iter().find(|p| p.id == id))
                .map(|pack| pack.title())
                .unwrap_or_default();
            let combo = ComboBox::from_id_salt("emoji-pack")
                .selected_text(shown)
                .show_ui(ui, |ui| {
                    for item in &dir.packs {
                        let on = Some(item.id) == pack;
                        if ui.selectable_label(on, item.title()).clicked() {
                            settings.emoji_pack = Some(item.id);
                        }
                    }
                });
            combo.response.labelled_by(label.id);
        });
    }
    ui.weak(if sticker {
        t!("admin.sticker_help")
    } else {
        t!("admin.emoji_help")
    });
    ui::page_section(ui, t!("admin.import_section"));
    ui.weak(t!("admin.import_help"));
    let room = dir.packs.len() < dir.packs_max && crate::files::PICKER;
    if ui
        .add_enabled_ui(room, |ui| ui::secondary(ui, t!("admin.import_button")))
        .inner
        .clicked()
    {
        ask.zip = true;
    }
    ui::page_section(ui, t!("admin.packs"));
    ui.weak(t!(
        "admin.emoji_count",
        kind = t!("admin.emoji"),
        count = count(false),
        max = dir.emoji_max.0
    ));
    ui.weak(t!(
        "admin.emoji_count",
        kind = t!("admin.stickers"),
        count = count(true),
        max = dir.emoji_max.1
    ));
    ui.horizontal(|ui| {
        let field = ui.add(
            ui::field(&mut settings.new_pack)
                .char_limit(32)
                .hint_text(t!("admin.pack_name_hint")),
        );
        let ready = !settings.new_pack.trim().is_empty() && dir.packs.len() < dir.packs_max;
        let add = ui
            .add_enabled_ui(ready, |ui| ui::primary(ui, t!("admin.pack_add")))
            .inner
            .clicked();
        if add || (ready && crate::app::submitted(ui, &field)) {
            ask.request = Some(Admin::AddPack(settings.new_pack.trim().to_owned()));
            settings.new_pack.clear();
        }
        if dir.packs.len() >= dir.packs_max {
            ui.label(RichText::new(t!("admin.pack_limit")).color(ui::colors(ui).danger_text));
        }
    });
    for (at, pack) in dir.packs.iter().enumerate() {
        ui.push_id(pack.id, |ui| {
            pack_block(ui, server, settings, (at, pack), &mut ask);
        });
    }
    ask
}

/// One pack: its header with rename, reorder and delete, and its emoji and stickers.
fn pack_block(
    ui: &mut Ui,
    server: &Server,
    settings: &mut Settings,
    (at, pack): (usize, &crate::server::EmojiPack),
    ask: &mut EmojiAsk,
) {
    let dir = &server.dir;
    ui.add_space(ui::SPACE_S);
    ui.horizontal(|ui| match &mut settings.pack_renaming {
        Some((id, name)) if *id == pack.id => {
            ui.add(ui::field(name).char_limit(32));
            let ok = !name.trim().is_empty();
            if ui
                .add_enabled_ui(ok, |ui| ui::primary(ui, t!("common.save")))
                .inner
                .clicked()
            {
                ask.request = Some(Admin::RenamePack(*id, name.trim().to_owned()));
            }
            if ui::secondary(ui, t!("common.cancel")).clicked() {
                settings.pack_renaming = None;
            }
        }
        _ => {
            ui.label(ui::title(pack.title()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let only = dir.packs.len() <= 1;
                if ui
                    .add_enabled_ui(!only, |ui| danger(ui, t!("common.delete")))
                    .inner
                    .clicked()
                {
                    settings.pack_deleting = Some(pack.id);
                }
                if ui::secondary(ui, t!("common.rename")).clicked() {
                    settings.pack_renaming = Some((pack.id, pack.name.clone().unwrap_or_default()));
                }
                let size = egui::Vec2::splat(ui::ICON_BUTTON);
                for (glyph, tip, step) in [
                    ("↓", t!("admin.move_down"), 1isize),
                    ("↑", t!("admin.move_up"), -1),
                ] {
                    let to = at
                        .checked_add_signed(step)
                        .filter(|to| *to < dir.packs.len());
                    let moved = ui
                        .add_enabled_ui(to.is_some(), |ui| {
                            ui::icon_button(ui, glyph, tip, ui::Look::Ghost, size)
                        })
                        .inner
                        .clicked();
                    if let (true, Some(to)) = (moved, to) {
                        let mut ids: Vec<Id> = dir.packs.iter().map(|p| p.id).collect();
                        ids.swap(at, to);
                        ask.request = Some(Admin::OrderPacks(ids));
                    }
                }
            });
        }
    });
    let items: Vec<_> = dir.emoji.iter().filter(|e| e.pack == pack.id).collect();
    if settings.pack_deleting == Some(pack.id) {
        ui::card(ui, |ui| {
            ui.label(t!("admin.pack_delete_help", count = items.len()));
            ui.horizontal(|ui| {
                if danger(ui, t!("admin.pack_delete")).clicked() {
                    ask.request = Some(Admin::DeletePack(pack.id));
                }
                if ui::secondary(ui, t!("common.cancel")).clicked() {
                    settings.pack_deleting = None;
                }
            });
        });
    }
    if items.is_empty() {
        ui.weak(t!("admin.none_yet"));
    }
    for item in items {
        ui.push_id(item.id, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) =
                    ui.allocate_exact_size(egui::Vec2::splat(32.0), egui::Sense::hover());
                if let Some(picture) = &item.picture {
                    paint_hovered(ui, rect, picture, 0.0);
                }
                match &mut settings.emoji_renaming {
                    Some((id, name)) if *id == item.id => {
                        if ui.add(ui::field(name).char_limit(32)).changed() {
                            clean_emoji_name(name);
                        }
                        let taken = dir
                            .emoji
                            .iter()
                            .any(|e| e.sticker == item.sticker && e.name == *name && e.id != *id);
                        let ok = emoji_name_ok(name) && !taken;
                        if ui
                            .add_enabled_ui(ok, |ui| ui::primary(ui, t!("common.save")))
                            .inner
                            .clicked()
                        {
                            ask.request = Some(Admin::RenameEmoji(*id, name.clone()));
                        }
                        if ui::secondary(ui, t!("common.cancel")).clicked() {
                            settings.emoji_renaming = None;
                        }
                    }
                    _ => {
                        ui.label(format!(":{}:", item.name));
                        if item.sticker {
                            ui.label(ui::caption(t!("admin.sticker")));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if danger(ui, t!("common.delete")).clicked() {
                                ask.request = Some(Admin::DeleteEmoji(item.id));
                            }
                            if ui::secondary(ui, t!("common.rename")).clicked() {
                                settings.emoji_renaming = Some((item.id, item.name.clone()));
                            }
                            if dir.packs.len() > 1 {
                                let combo = ComboBox::from_id_salt("move-emoji")
                                    .selected_text(t!("admin.move_to"))
                                    .width(90.0)
                                    .show_ui(ui, |ui| {
                                        for other in dir.packs.iter().filter(|p| p.id != pack.id) {
                                            if ui.selectable_label(false, other.title()).clicked() {
                                                ask.request =
                                                    Some(Admin::MoveEmoji(item.id, other.id));
                                            }
                                        }
                                    });
                                combo.response.widget_info(|| {
                                    egui::WidgetInfo::labeled(
                                        Role::ComboBox,
                                        true,
                                        t!("admin.move_to"),
                                    )
                                });
                            }
                        });
                    }
                }
            });
        });
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
                MemberAction::Kick => {
                    ui.label(ui::bold(t!("admin.kick_title", name = name)));
                    ui.label(t!("admin.kick_help"));
                    ui.horizontal(|ui| {
                        if danger(ui, t!("admin.kick")).clicked() {
                            request = Some(Admin::Kick(person));
                        }
                        cancel = ui::secondary(ui, t!("common.cancel")).clicked();
                    });
                }
                MemberAction::Ban { network } => {
                    ui.label(ui::bold(t!("admin.ban_title", name = name)));
                    ui.label(t!("admin.ban_help"));
                    ui::checkbox(ui, network, t!("admin.ban_network"));
                    ui.weak(t!("admin.ban_network_help"));
                    ui.horizontal(|ui| {
                        if danger(ui, t!("admin.ban")).clicked() {
                            request = Some(Admin::Ban(person, *network));
                        }
                        cancel = ui::secondary(ui, t!("common.cancel")).clicked();
                    });
                }
                MemberAction::Roles(chosen) => {
                    ui.label(ui::bold(t!("admin.roles_for", name = name)));
                    for role in server.dir.roles.iter().filter(|r| r.id != "owner") {
                        let mut on = chosen.contains(&role.id);
                        // Only roles below your own can be given or taken away.
                        let allowed = role.rank < perms.rank;
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
                if ui::secondary(ui, t!("admin.unban")).clicked() {
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
                    request = Some(Admin::Audit(filter.map(str::to_owned)));
                }
            });
        combo
            .response
            .widget_info(|| egui::WidgetInfo::labeled(Role::ComboBox, true, t!("admin.show")));
    });
    if settings.audit.is_empty() {
        ui.weak(t!("admin.nothing_yet"));
        return request;
    }
    for entry in settings.audit.iter().take(settings.audit_shown) {
        ui.horizontal_wrapped(|ui| {
            ui.label(sentence(server, entry));
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
    if settings.audit.len() > settings.audit_shown
        && ui::secondary(ui, t!("admin.show_older")).clicked()
    {
        settings.audit_shown += AUDIT_PAGE;
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
        "voice_unmuted" => t!("audit.voice_unmuted", actor = actor, target = target),
        "voice_removed" => t!("audit.voice_removed", actor = actor, target = target),
        "voice_moved" => t!("audit.voice_moved", actor = actor, target = target),
        "left" => t!("audit.left", actor = actor),
        "member_roles_changed" => t!("audit.member_roles_changed", actor = actor, target = target),
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
        "channel_permissions_changed" => t!("audit.channel_permissions_changed", actor = actor),
        "category_created" => t!("audit.category_created", actor = actor),
        "category_updated" => t!("audit.category_updated", actor = actor),
        "category_deleted" => t!("audit.category_deleted", actor = actor),
        "channels_reordered" => t!("audit.channels_reordered", actor = actor),
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
            ui.add(DragValue::new(cap).range(1..=100_000))
                .widget_info(|| {
                    egui::WidgetInfo::labeled(Role::SpinButton, true, t!("admin.member_limit"))
                });
        },
    );
    ui::primary(ui, t!("common.save"))
        .clicked()
        .then_some(Admin::Joining(*open, *cap))
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

/// Read, write, speak and video per role, for the Edit channel dialog. Changes wait in
/// `pending` (role and flags) until the dialog is saved.
pub(crate) fn channel_flags(
    ui: &mut Ui,
    server: &Server,
    channel: &Channel,
    pending: &mut Vec<(String, u8)>,
) {
    ui.label(t!("admin.who_can_use_channel"));
    let columns: &[(u8, &str)] = if channel.voice {
        &[
            (1, t!("admin.flag_read")),
            (2, t!("admin.flag_write")),
            (4, t!("admin.flag_speak")),
            (8, t!("admin.flag_video")),
        ]
    } else if channel.page {
        // Writing in a page channel is changing the page.
        &[(1, t!("admin.flag_read")), (2, t!("admin.flag_edit"))]
    } else {
        &[(1, t!("admin.flag_read")), (2, t!("admin.flag_write"))]
    };
    Grid::new("channel-flags").striped(true).show(ui, |ui| {
        ui.label("");
        for (_, label) in columns {
            ui.label(ui::bold(*label));
        }
        ui.end_row();
        for role in &server.dir.roles {
            let flags = pending
                .iter()
                .find(|(id, _)| *id == role.id)
                .map(|(_, flags)| *flags)
                .unwrap_or_else(|| channel_role_flags(channel, &role.id));
            ui.label(&role.name);
            for (bit, _) in columns {
                let mut on = flags & bit != 0;
                if ui::checkbox(ui, &mut on, "").changed() {
                    // Everything else needs Read; taking Read away takes the rest too.
                    let flags = match (on, *bit) {
                        (false, 1) => 0,
                        (true, bit) => flags | bit | 1,
                        (false, bit) => flags & !bit,
                    };
                    pending.retain(|(id, _)| *id != role.id);
                    pending.push((role.id.clone(), flags));
                }
            }
            ui.end_row();
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
