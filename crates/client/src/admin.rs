//! The server settings window (members, roles, invites, bans, audit log, joining) and channel
//! settings. Shown only to people whose roles allow some of it; the server decides.

use chrono::{DateTime, Local};
use eframe::egui::{
    self, CollapsingHeader, ComboBox, DragValue, Grid, RichText, Ui, accesskit::Role,
};

use rusttalk_core::{
    auth::decode,
    index::{https_url, validate_details},
};

use crate::{
    files::Ask,
    home::{ChannelEdit, Home, face, frame, initials},
    net::{Admin, AdminUpdate, AuditEntry, Ban, Command, Invite, PublicListing},
    server::{Id, Perms, Server, Status, permissions},
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
    /// The owner's public listing.
    Listing,
    /// Server emoji and stickers.
    Emoji,
    /// The settings of one channel.
    Channel(Id),
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
    /// Whether the server is listed and the details, once loaded (owner).
    listing: Option<(bool, PublicListing)>,
    member: Option<(Id, MemberAction)>,
    new_role: String,
    new_permissions: Vec<&'static str>,
    renaming: Option<(String, String)>,
    /// The custom role being edited: its ID, name and permissions.
    editing: Option<(String, String, Vec<&'static str>)>,
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
            AdminUpdate::Listing(on, saved) => {
                self.listing = Some((on, saved.unwrap_or_default()));
            }
            AdminUpdate::Done(text) => {
                self.member = None;
                self.renaming = None;
                self.editing = None;
                self.revoking = None;
                self.emoji_renaming = None;
                self.message = Some((text.into(), false));
            }
            AdminUpdate::Failed(text) => self.message = Some((text, true)),
        }
    }
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
        } else {
            Tab::Invites
        };
    }
    server.send(Command::Admin(Admin::Load {
        joining: perms.admin,
        audit: perms.audit,
        bans: perms.ban,
    }));
    if tab == Tab::Invites {
        server.send(Command::Admin(Admin::Invites));
    }
    home.server_settings = Some(Settings {
        server: index,
        tab,
        bans: Vec::new(),
        audit: Vec::new(),
        audit_shown: AUDIT_PAGE,
        audit_filter: None,
        joining: None,
        listing: None,
        member: None,
        new_role: String::new(),
        new_permissions: Vec::new(),
        renaming: None,
        editing: None,
        invites: Vec::new(),
        revoking: None,
        server_name: None,
        emoji_name: String::new(),
        emoji_sticker: false,
        emoji_renaming: None,
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
    let mut request = None;
    let mut icon = false;
    let mut upload = None;
    let mut channel_edit = None;
    let mut rename = None;
    let close = if let Tab::Channel(channel) = settings.tab {
        if !server.dir.perms.manage_channels {
            home.server_settings = None;
            return;
        }
        let title = channel_title(server, channel);
        let mut close = false;
        let ((), dismissed) = ui::dialog(ctx, "channel-settings", &title, |ui| {
            ui.set_width(560.0);
            request = channel_tab(ui, server, channel);
            if server.status == Status::Online && server.channel(&channel).is_some() {
                ui.separator();
                ui.horizontal(|ui| {
                    if ui::secondary(ui, t!("home.rename_channel")).clicked() {
                        channel_edit = Some((settings.server, channel, ChannelEdit::Rename));
                    }
                    if ui::danger(ui, t!("home.delete_channel")).clicked() {
                        channel_edit = Some((settings.server, channel, ChannelEdit::Delete));
                    }
                });
            }
            message(ui, settings);
            ui::footer(ui, |ui| {
                close = ui::secondary(ui, t!("common.close")).clicked()
            });
        });
        close || dismissed
    } else {
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
                        None
                    }
                    Tab::Members => members_tab(ui, server, settings),
                    Tab::Roles => roles_tab(ui, server, settings),
                    Tab::Invites => invites_tab(ui, server, settings),
                    Tab::Bans => bans_tab(ui, server, settings),
                    Tab::Audit => audit_tab(ui, server, settings),
                    Tab::Joining => joining_tab(ui, settings),
                    Tab::Listing => listing_tab(ui, server, settings),
                    Tab::Emoji => {
                        let (picked, request) = emoji_tab(ui, server, settings);
                        upload = picked;
                        request
                    }
                    Tab::Channel(_) => None,
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
                Tab::Invites => Some(Admin::Invites),
                _ => None,
            };
        } else {
            request = changed;
        }
        close
    };
    // Confirmations over the settings window.
    if let Some(asked) = role_editor(ctx, settings).or_else(|| revoke_dialog(ctx, settings)) {
        request = Some(asked);
    }
    if let Some(request) = request {
        settings.message = None;
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
    if let Some((name, sticker)) = upload {
        settings.message = None;
        settings.emoji_name.clear();
        home.files
            .ask(ctx, Ask::Emoji(server.address.clone(), name, sticker));
    }
    if close || channel_edit.is_some() {
        home.server_settings = None;
    }
    if let Some((index, id, edit)) = channel_edit {
        home.edit_channel(index, id, edit);
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
    /// t!("admin.change_icon") was clicked.
    icon: bool,
    /// The new server name to save.
    rename: Option<String>,
}

/// The server icon and name, for people who may manage the server.
fn overview_tab(ui: &mut Ui, server: &Server, settings: &mut Settings) -> Overview {
    let mut picked = Overview {
        icon: false,
        rename: None,
    };
    let saved = server.dir.name.clone().unwrap_or_default();
    ui::page_section(ui, t!("admin.server"));
    ui.horizontal_top(|ui| {
        ui.vertical(|ui| {
            server_icon(ui, server, ICON_SIZE);
            picked.icon = ui::secondary(ui, t!("admin.change_icon"))
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
    picked
}

/// The server's icon, or its initials on a rounded square.
fn server_icon(ui: &mut Ui, server: &Server, size: f32) {
    match &server.dir.icon {
        Some(icon) => {
            ui.add(
                egui::Image::new(&frame(ui, icon))
                    .fit_to_exact_size(egui::Vec2::splat(size))
                    .corner_radius(size / 4.0),
            );
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
    (2..=32).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Keep a typed name to the characters a `:name:` allows.
fn clean_emoji_name(name: &mut String) {
    *name = name
        .chars()
        .filter_map(|c| match c {
            ' ' | '-' => Some('_'),
            c if c.is_ascii_alphanumeric() || c == '_' => Some(c.to_ascii_lowercase()),
            _ => None,
        })
        .take(32)
        .collect();
}

/// Upload, rename and delete server emoji and stickers. Returns a name and kind to
/// pick a picture for, or a change to send.
fn emoji_tab(
    ui: &mut Ui,
    server: &Server,
    settings: &mut Settings,
) -> (Option<(String, bool)>, Option<Admin>) {
    let dir = &server.dir;
    let mut upload = None;
    let mut request = None;
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
        let ready = emoji_name_ok(name) && !taken && !full;
        let pick = ui
            .add_enabled_ui(ready, |ui| ui::primary(ui, t!("admin.pick_picture")))
            .inner
            .clicked();
        if pick || (ready && crate::app::submitted(ui, &field)) {
            upload = Some((name.to_owned(), sticker));
        }
        if taken {
            ui.label(RichText::new(t!("admin.name_taken")).color(ui::colors(ui).danger_text));
        } else if full {
            ui.label(RichText::new(t!("admin.emoji_full")).color(ui::colors(ui).danger_text));
        }
    });
    ui.weak(if sticker {
        t!("admin.sticker_help")
    } else {
        t!("admin.emoji_help")
    });
    for (sticker, title, max) in [
        (false, t!("admin.emoji"), dir.emoji_max.0),
        (true, t!("admin.stickers"), dir.emoji_max.1),
    ] {
        ui::page_section(
            ui,
            &t!(
                "admin.emoji_count",
                kind = title,
                count = count(sticker),
                max = max
            ),
        );
        let mut empty = true;
        for item in dir.emoji.iter().filter(|e| e.sticker == sticker) {
            empty = false;
            ui.push_id(item.id, |ui| {
                ui.horizontal(|ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::Vec2::splat(32.0), egui::Sense::hover());
                    if let Some(picture) = &item.picture {
                        egui::Image::new(&frame(ui, picture)).paint_at(ui, rect);
                    }
                    match &mut settings.emoji_renaming {
                        Some((id, name)) if *id == item.id => {
                            if ui.add(ui::field(name).char_limit(32)).changed() {
                                clean_emoji_name(name);
                            }
                            let taken = dir
                                .emoji
                                .iter()
                                .any(|e| e.sticker == sticker && e.name == *name && e.id != *id);
                            let ok = emoji_name_ok(name) && !taken;
                            if ui
                                .add_enabled_ui(ok, |ui| ui::primary(ui, t!("common.save")))
                                .inner
                                .clicked()
                            {
                                request = Some(Admin::RenameEmoji(*id, name.clone()));
                            }
                            if ui::secondary(ui, t!("common.cancel")).clicked() {
                                settings.emoji_renaming = None;
                            }
                        }
                        _ => {
                            ui.label(format!(":{}:", item.name));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if danger(ui, t!("common.delete")).clicked() {
                                        request = Some(Admin::DeleteEmoji(item.id));
                                    }
                                    if ui::secondary(ui, t!("common.rename")).clicked() {
                                        settings.emoji_renaming =
                                            Some((item.id, item.name.clone()));
                                    }
                                },
                            );
                        }
                    }
                });
            });
        }
        if empty {
            ui.weak(t!("admin.none_yet"));
        }
    }
    (upload, request)
}

fn role_names(server: &Server, roles: &[String]) -> String {
    let mut names: Vec<_> = server
        .dir
        .roles
        .iter()
        .filter(|role| roles.contains(&role.id))
        .map(|role| role.name.as_str())
        .collect();
    if names.is_empty() {
        names.push(t!("admin.no_role"));
    }
    names.join(", ")
}

fn members_tab(ui: &mut Ui, server: &Server, settings: &mut Settings) -> Option<Admin> {
    let perms = server.dir.perms;
    let mut request = None;
    ui::page_section(ui, t!("admin.members"));
    if let Some((person, action)) = &mut settings.member {
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
    ui.weak(t!("admin.members_help"));
    let mut pick = None;
    for member in &server.dir.members {
        let name = server.member_name(&member.person);
        let row = ui::row_button(ui, 44.0, &name, |ui| {
            face(ui, &server.dir, &member.person, &name, 30.0, false);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(ui::bold(&name));
                ui.label(ui::caption(role_names(server, &member.roles)));
            });
        });
        if member.person == server.dir.me {
            continue;
        }
        let mut choice = None;
        ui::context_menu(&row, |m| member_menu(m, &mut choice, perms));
        if let Some(choice) = choice {
            pick = Some((member.person, action(server, &member.person, choice)));
        }
    }
    if pick.is_some() {
        settings.member = pick;
        settings.message = None;
    }
    request
}

fn roles_tab(ui: &mut Ui, server: &Server, settings: &mut Settings) -> Option<Admin> {
    let dir = &server.dir;
    let mut request = None;
    let default = dir
        .roles
        .iter()
        .find(|role| role.id == dir.default_role)
        .map_or("", |role| role.name.as_str());
    ui::page_section(ui, t!("admin.new_members"));
    ui::setting_row(
        ui,
        t!("admin.default_role"),
        t!("admin.default_role_help"),
        |ui| {
            let combo = ComboBox::from_id_salt("default role")
                .selected_text(default)
                .show_ui(ui, |ui| {
                    for role in dir.roles.iter().filter(|r| r.id != "owner") {
                        if ui
                            .selectable_label(role.id == dir.default_role, &role.name)
                            .clicked()
                            && role.id != dir.default_role
                        {
                            request = Some(Admin::DefaultRole(role.id.clone()));
                        }
                    }
                });
            combo.response.widget_info(|| {
                egui::WidgetInfo::labeled(Role::ComboBox, true, t!("admin.default_role"))
            });
        },
    );
    ui::page_section(ui, t!("admin.roles"));
    for role in &dir.roles {
        CollapsingHeader::new(&role.name)
            .id_salt(&role.id)
            .show(ui, |ui| {
                for (permission, label) in permissions() {
                    let mut on = role.permissions.iter().any(|p| p == permission);
                    ui.add_enabled(false, ui::Checkbox::new(&mut on, label));
                }
                match &mut settings.renaming {
                    Some((id, name)) if *id == role.id => {
                        ui.horizontal(|ui| {
                            ui.add(ui::field(name).char_limit(100));
                            if ui::primary(ui, t!("common.save")).clicked()
                                && !name.trim().is_empty()
                            {
                                request = Some(Admin::RenameRole(id.clone(), name.trim().into()));
                            }
                        });
                    }
                    _ => {
                        ui.horizontal(|ui| {
                            if !role.custom {
                                if ui::secondary(ui, t!("common.rename")).clicked() {
                                    settings.renaming = Some((role.id.clone(), role.name.clone()));
                                }
                                return;
                            }
                            // Only roles below your own can be changed.
                            let below = role.rank < dir.perms.rank;
                            if ui
                                .add_enabled_ui(below, |ui| ui::secondary(ui, t!("common.edit")))
                                .inner
                                .clicked()
                            {
                                let permissions = permissions()
                                    .into_iter()
                                    .map(|(permission, _)| permission)
                                    .filter(|p| role.permissions.iter().any(|r| r == p))
                                    .collect();
                                settings.editing =
                                    Some((role.id.clone(), role.name.clone(), permissions));
                                settings.message = None;
                            }
                            if danger(ui, t!("common.delete")).clicked() {
                                request = Some(Admin::DeleteRole(role.id.clone()));
                            }
                        });
                    }
                }
            });
    }
    ui::page_section(ui, t!("admin.make_role"));
    let label = ui.label(t!("admin.role_name"));
    ui.add(ui::field(&mut settings.new_role).char_limit(100))
        .labelled_by(label.id);
    permission_boxes(ui, &mut settings.new_permissions);
    let name = settings.new_role.trim();
    if ui
        .add_enabled_ui(!name.is_empty(), |ui| {
            ui::primary(ui, t!("admin.create_role"))
        })
        .inner
        .clicked()
    {
        request = Some(Admin::CreateRole {
            name: name.into(),
            permissions: std::mem::take(&mut settings.new_permissions),
        });
        settings.new_role.clear();
    }
    request
}

/// A checkbox per permission, in two columns.
fn permission_boxes(ui: &mut Ui, chosen: &mut Vec<&'static str>) {
    ui.columns(2, |columns| {
        for (at, (permission, label)) in permissions().into_iter().enumerate() {
            let ui = &mut columns[at % 2];
            let mut on = chosen.contains(&permission);
            if ui::checkbox(ui, &mut on, label).changed() {
                chosen.retain(|p| *p != permission);
                if on {
                    chosen.push(permission);
                }
            }
        }
    });
}

/// The dialog to change a custom role's name and permissions.
fn role_editor(ctx: &egui::Context, settings: &mut Settings) -> Option<Admin> {
    let (id, name, permissions) = settings.editing.as_mut()?;
    let mut request = None;
    let mut cancel = false;
    let ((), dismissed) = ui::dialog(ctx, "edit-role", t!("admin.edit_role"), |ui| {
        let label = ui.label(ui::bold(t!("admin.role_name")));
        ui.add(
            ui::field(name)
                .char_limit(100)
                .desired_width(ui.available_width()),
        )
        .labelled_by(label.id);
        ui.add_space(ui::SPACE_S);
        ui.label(ui::bold(t!("admin.permissions")));
        permission_boxes(ui, permissions);
        if let Some((text, true)) = &settings.message {
            error_text(ui, Some(text));
        }
        ui::footer(ui, |ui| {
            let name = name.trim();
            if ui
                .add_enabled_ui(!name.is_empty(), |ui| ui::primary(ui, t!("common.save")))
                .inner
                .clicked()
            {
                request = Some(Admin::EditRole {
                    id: id.clone(),
                    name: name.into(),
                    permissions: permissions.clone(),
                });
            }
            cancel = ui::secondary(ui, t!("common.cancel")).clicked();
        });
    });
    if cancel || dismissed {
        settings.editing = None;
        settings.message = None;
    }
    request
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
        "left" => t!("audit.left", actor = actor),
        "member_roles_changed" => t!("audit.member_roles_changed", actor = actor, target = target),
        "ownership_transferred" => t!(
            "audit.ownership_transferred",
            actor = actor,
            target = target
        ),
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
        "invite_created" => t!("audit.invite_created", actor = actor),
        "invite_revoked" => t!("audit.invite_revoked", actor = actor),
        "open_joining_changed" => t!("audit.open_joining_changed", actor = actor),
        "message_deleted" => t!("audit.message_deleted", actor = actor),
        "file_deleted" => t!("audit.file_deleted", actor = actor),
        "emoji_added" => t!("audit.emoji_added", actor = actor),
        "emoji_renamed" => t!("audit.emoji_renamed", actor = actor),
        "emoji_deleted" => t!("audit.emoji_deleted", actor = actor),
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

/// Read, write, speak and video per role. Changes save right away.
fn channel_tab(ui: &mut Ui, server: &Server, channel: Id) -> Option<Admin> {
    let Some(found) = server.channel(&channel) else {
        ui.label(t!("admin.channel_deleted"));
        return None;
    };
    ui.label(t!("admin.who_can_use_channel"));
    let columns: &[(u8, &str)] = if found.voice {
        &[
            (1, t!("admin.flag_read")),
            (2, t!("admin.flag_write")),
            (4, t!("admin.flag_speak")),
            (8, t!("admin.flag_video")),
        ]
    } else {
        &[(1, t!("admin.flag_read")), (2, t!("admin.flag_write"))]
    };
    let mut request = None;
    Grid::new("channel-flags").striped(true).show(ui, |ui| {
        ui.label("");
        for (_, label) in columns {
            ui.label(ui::bold(*label));
        }
        ui.end_row();
        for role in &server.dir.roles {
            let flags = found
                .flags
                .iter()
                .find(|(id, _)| *id == role.id)
                .map_or(0, |(_, flags)| *flags);
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
                    request = Some(Admin::ChannelFlags(channel, role.id.clone(), flags));
                }
            }
            ui.end_row();
        }
    });
    request
}

/// The channel settings title, like "# general settings".
fn channel_title(server: &Server, channel: Id) -> String {
    server.channel(&channel).map_or_else(
        || t!("home.channel_settings").into(),
        |found| {
            let prefix = if found.voice { "🔊" } else { "#" };
            t!("admin.channel_title", icon = prefix, channel = found.name)
        },
    )
}

fn date(at: i64) -> String {
    DateTime::from_timestamp(at, 0).map_or_else(String::new, |at| {
        crate::i18n::time(at.with_timezone(&Local), "time.date_clock")
    })
}
