//! The server settings window (members, roles, bans, audit log, joining) and channel
//! settings. Shown only to people whose roles allow some of it; the server decides.

use chrono::{DateTime, Local};
use eframe::egui::{
    self, Button, CollapsingHeader, ComboBox, DragValue, Grid, Id as UiId, Modal, RichText,
    ScrollArea, TextEdit, Ui,
};

use rusttalk_core::{
    auth::decode,
    index::{https_url, validate_details},
};

use crate::{
    app::error_text,
    files::Ask,
    home::{Home, face, frame, section},
    net::{Admin, AdminUpdate, AuditEntry, Ban, Command, PublicListing},
    server::{Id, PERMISSIONS, Perms, Server},
};

/// Audit log entries shown per "Show older".
const AUDIT_PAGE: usize = 50;

/// Audit log filters: API action and label.
const FILTERS: [(&str, &str); 9] = [
    ("member_banned", "Bans"),
    ("member_unbanned", "Unbans"),
    ("member_kicked", "Kicks"),
    ("left", "Leaves"),
    ("member_roles_changed", "Role changes"),
    ("joined", "Joins without an invite"),
    ("message_deleted", "Deleted messages"),
    ("invite_created", "New invites"),
    ("channel_permissions_changed", "Channel permissions"),
];

#[derive(Clone, Copy, PartialEq)]
pub enum Tab {
    /// The server icon.
    Overview,
    Members,
    Roles,
    Bans,
    Audit,
    Joining,
    /// The owner's public listing.
    Listing,
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
            AdminUpdate::Joining(open, cap) => self.joining = Some((open, cap)),
            AdminUpdate::Listing(on, saved) => {
                self.listing = Some((on, saved.unwrap_or_default()));
            }
            AdminUpdate::Done(text) => {
                self.member = None;
                self.renaming = None;
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
        } else if perms.audit {
            Tab::Audit
        } else {
            Tab::Joining
        };
    }
    server.send(Command::Admin(Admin::Load {
        joining: perms.admin,
        audit: perms.audit,
        bans: perms.ban,
    }));
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
pub fn member_menu(ui: &mut Ui, choice: &mut Option<Moderate>, perms: Perms) {
    if perms.manage_roles && ui.button("Change roles").clicked() {
        *choice = Some(Moderate::Roles);
    }
    if perms.kick && ui.button("Kick").clicked() {
        *choice = Some(Moderate::Kick);
    }
    if perms.ban && ui.button("Ban").clicked() {
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
    let mut close = false;
    let mut request = None;
    let mut icon = false;
    let modal = Modal::new(UiId::unique("server-settings")).show(ctx, |ui| {
        ui.set_width(560.0);
        let perms = server.dir.perms;
        if let Tab::Channel(channel) = settings.tab {
            request = channel_tab(ui, server, channel);
        } else {
            ui.heading(format!("{} settings", server.name()));
            ui.horizontal(|ui| {
                let tabs = [
                    (Tab::Overview, "Overview", perms.manage_server),
                    (
                        Tab::Members,
                        "Members",
                        perms.kick || perms.ban || perms.manage_roles,
                    ),
                    (Tab::Roles, "Roles", perms.manage_roles),
                    (Tab::Bans, "Bans", perms.ban),
                    (Tab::Audit, "Audit log", perms.audit),
                    (Tab::Joining, "Joining", perms.admin),
                    (Tab::Listing, "Public listing", perms.owner),
                ];
                for (tab, label, allowed) in tabs {
                    if allowed && ui.selectable_value(&mut settings.tab, tab, label).clicked() {
                        settings.message = None;
                        // Show what changed since the window opened.
                        request = match tab {
                            Tab::Audit => {
                                Some(Admin::Audit(settings.audit_filter.map(str::to_owned)))
                            }
                            Tab::Bans => Some(Admin::Load {
                                joining: false,
                                audit: false,
                                bans: true,
                            }),
                            Tab::Listing => Some(Admin::LoadListing),
                            _ => None,
                        };
                    }
                }
            });
            ui.separator();
            let changed = match settings.tab {
                Tab::Overview => {
                    icon = overview_tab(ui, server);
                    None
                }
                Tab::Members => members_tab(ui, server, settings),
                Tab::Roles => roles_tab(ui, server, settings),
                Tab::Bans => bans_tab(ui, server, settings),
                Tab::Audit => audit_tab(ui, server, settings),
                Tab::Joining => joining_tab(ui, settings),
                Tab::Listing => listing_tab(ui, server, settings),
                Tab::Channel(_) => None,
            };
            request = request.take().or(changed);
        }
        if let Some((text, error)) = &settings.message {
            if *error {
                error_text(ui, Some(text));
            } else {
                ui.label(RichText::new(text).color(ui.visuals().selection.stroke.color));
            }
        }
        ui.separator();
        close = ui.button("Close").clicked();
    });
    if let Some(request) = request {
        settings.message = None;
        server.send(Command::Admin(request));
    }
    if icon {
        settings.message = None;
        home.files.ask(ctx, Ask::Icon(server.address.clone()));
    }
    if close || modal.should_close() {
        home.server_settings = None;
    }
}

/// The server icon, for people who may manage the server. True when "Change icon" was clicked.
fn overview_tab(ui: &mut Ui, server: &Server) -> bool {
    section(ui, "Server icon");
    ui.horizontal(|ui| {
        match &server.dir.icon {
            Some(icon) => {
                ui.add(
                    egui::Image::new(&frame(ui, icon))
                        .fit_to_exact_size(egui::Vec2::splat(64.0))
                        .corner_radius(16),
                );
            }
            None => {
                ui.weak("No icon yet. The server's initials show instead.");
            }
        }
        ui.vertical(|ui| {
            ui.label("Shown in everyone's server list.");
            ui.button("Change icon")
                .on_hover_text("PNG, JPG or GIF. GIFs play.")
                .clicked()
        })
        .inner
    })
    .inner
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
        names.push("No role");
    }
    names.join(", ")
}

fn members_tab(ui: &mut Ui, server: &Server, settings: &mut Settings) -> Option<Admin> {
    let perms = server.dir.perms;
    let mut request = None;
    if let Some((person, action)) = &mut settings.member {
        let name = server.member_name(person);
        let person = *person;
        let mut cancel = false;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            match action {
                MemberAction::Kick => {
                    ui.strong(format!("Kick {name}?"));
                    ui.label("They leave the server now. They can come back with a new invite.");
                    ui.horizontal(|ui| {
                        if danger(ui, "Kick").clicked() {
                            request = Some(Admin::Kick(person));
                        }
                        cancel = ui.button("Cancel").clicked();
                    });
                }
                MemberAction::Ban { network } => {
                    ui.strong(format!("Ban {name}?"));
                    ui.label("They leave the server and can't come back until you unban them.");
                    ui.checkbox(network, "Also block their network");
                    ui.weak("Stops anyone on their internet connection from joining too.");
                    ui.horizontal(|ui| {
                        if danger(ui, "Ban").clicked() {
                            request = Some(Admin::Ban(person, *network));
                        }
                        cancel = ui.button("Cancel").clicked();
                    });
                }
                MemberAction::Roles(chosen) => {
                    ui.strong(format!("Roles for {name}"));
                    for role in server.dir.roles.iter().filter(|r| r.id != "owner") {
                        let mut on = chosen.contains(&role.id);
                        // Only roles below your own can be given or taken away.
                        let allowed = role.rank < perms.rank;
                        if ui
                            .add_enabled(allowed, egui::Checkbox::new(&mut on, &role.name))
                            .changed()
                        {
                            chosen.retain(|id| *id != role.id);
                            if on {
                                chosen.push(role.id.clone());
                            }
                        }
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() {
                            request = Some(Admin::SetRoles(person, chosen.clone()));
                        }
                        cancel = ui.button("Cancel").clicked();
                    });
                }
            }
        });
        if cancel {
            settings.member = None;
        }
    }
    ui.weak("Right-click someone to kick, ban or change their roles.");
    let mut pick = None;
    ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
        for member in &server.dir.members {
            let name = server.member_name(&member.person);
            let row = ui
                .horizontal(|ui| {
                    face(ui, &server.dir, &member.person, &name, 28.0, false);
                    ui.vertical(|ui| {
                        let row = ui.add(
                            Button::selectable(false, &name)
                                .frame_when_inactive(false)
                                .min_size(egui::vec2(ui.available_width(), 0.0)),
                        );
                        ui.label(
                            RichText::new(role_names(server, &member.roles))
                                .small()
                                .weak(),
                        );
                        row
                    })
                    .inner
                })
                .inner;
            if member.person == server.dir.me {
                continue;
            }
            let mut choice = None;
            row.context_menu(|ui| member_menu(ui, &mut choice, perms));
            if let Some(choice) = choice {
                pick = Some((member.person, action(server, &member.person, choice)));
            }
        }
    });
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
    ComboBox::from_label("New members get")
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
    ui.weak("What a role can do is set when it's made. To change it, make a new role.");
    ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
        for role in &dir.roles {
            CollapsingHeader::new(&role.name)
                .id_salt(&role.id)
                .show(ui, |ui| {
                    for (permission, label) in PERMISSIONS {
                        let mut on = role.permissions.iter().any(|p| p == permission);
                        ui.add_enabled(false, egui::Checkbox::new(&mut on, label));
                    }
                    match &mut settings.renaming {
                        Some((id, name)) if *id == role.id => {
                            ui.horizontal(|ui| {
                                ui.add(TextEdit::singleline(name).char_limit(100));
                                if ui.button("Save").clicked() && !name.trim().is_empty() {
                                    request =
                                        Some(Admin::RenameRole(id.clone(), name.trim().into()));
                                }
                            });
                        }
                        _ => {
                            ui.horizontal(|ui| {
                                if ui.button("Rename").clicked() {
                                    settings.renaming = Some((role.id.clone(), role.name.clone()));
                                }
                                if role.custom && danger(ui, "Delete").clicked() {
                                    request = Some(Admin::DeleteRole(role.id.clone()));
                                }
                            });
                        }
                    }
                });
        }
    });
    section(ui, "Make a role");
    let label = ui.label("Role name");
    ui.add(TextEdit::singleline(&mut settings.new_role).char_limit(100))
        .labelled_by(label.id);
    ui.columns(2, |columns| {
        for (at, (permission, label)) in PERMISSIONS.into_iter().enumerate() {
            let ui = &mut columns[at % 2];
            let mut on = settings.new_permissions.contains(&permission);
            if ui.checkbox(&mut on, label).changed() {
                settings.new_permissions.retain(|p| *p != permission);
                if on {
                    settings.new_permissions.push(permission);
                }
            }
        }
    });
    let name = settings.new_role.trim();
    if ui
        .add_enabled(!name.is_empty(), Button::new("Create role"))
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

fn bans_tab(ui: &mut Ui, server: &Server, settings: &Settings) -> Option<Admin> {
    if settings.bans.is_empty() {
        ui.weak("Nobody is banned.");
        return None;
    }
    let mut request = None;
    ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
        Grid::new("bans")
            .num_columns(3)
            .striped(true)
            .show(ui, |ui| {
                for ban in &settings.bans {
                    ui.label(server.member_name(&ban.person));
                    let mut when = format!("Banned {}", date(ban.at));
                    if ban.network {
                        when.push_str(" · network blocked");
                    }
                    ui.weak(when);
                    if ui.button("Unban").clicked() {
                        request = Some(Admin::Unban(ban.person));
                    }
                    ui.end_row();
                }
            });
    });
    request
}

fn audit_tab(ui: &mut Ui, server: &Server, settings: &mut Settings) -> Option<Admin> {
    let mut request = None;
    let shown = settings
        .audit_filter
        .and_then(|action| FILTERS.iter().find(|(a, _)| *a == action))
        .map_or("Everything", |(_, label)| label);
    ComboBox::from_label("Show")
        .selected_text(shown)
        .show_ui(ui, |ui| {
            let mut filter = settings.audit_filter;
            ui.selectable_value(&mut filter, None, "Everything");
            for (action, label) in FILTERS {
                ui.selectable_value(&mut filter, Some(action), label);
            }
            if filter != settings.audit_filter {
                settings.audit_filter = filter;
                request = Some(Admin::Audit(filter.map(str::to_owned)));
            }
        });
    if settings.audit.is_empty() {
        ui.weak("Nothing here yet.");
        return request;
    }
    ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
        for entry in settings.audit.iter().take(settings.audit_shown) {
            ui.horizontal_wrapped(|ui| {
                ui.label(sentence(server, entry));
                ui.weak(date(entry.at));
            });
        }
        if settings.audit.len() > settings.audit_shown && ui.button("Show older").clicked() {
            settings.audit_shown += AUDIT_PAGE;
        }
    });
    request
}

/// One audit entry as a plain sentence, with names decrypted on this device.
fn sentence(server: &Server, entry: &AuditEntry) -> String {
    let actor = server.member_name(&entry.actor);
    let target = entry
        .target
        .map_or_else(|| "someone".into(), |person| server.member_name(&person));
    match entry.action.as_str() {
        "member_banned" => format!("{actor} banned {target}"),
        "member_unbanned" => format!("{actor} unbanned {target}"),
        "member_kicked" => format!("{actor} kicked {target}"),
        "left" => format!("{actor} left"),
        "member_roles_changed" => format!("{actor} changed {target}'s roles"),
        "ownership_transferred" => format!("{actor} made {target} the owner"),
        "joined" => format!("{actor} joined"),
        "role_created" => format!("{actor} made a role"),
        "role_renamed" => format!("{actor} renamed a role"),
        "role_deleted" => format!("{actor} deleted a role"),
        "default_role_changed" => format!("{actor} changed the role for new members"),
        "channel_created" => format!("{actor} made a channel"),
        "channel_deleted" => format!("{actor} deleted a channel"),
        "channel_permissions_changed" => format!("{actor} changed who can use a channel"),
        "category_created" => format!("{actor} made a category"),
        "category_deleted" => format!("{actor} deleted a category"),
        "channels_reordered" => format!("{actor} changed the channel order"),
        "invite_created" => format!("{actor} made an invite"),
        "invite_revoked" => format!("{actor} stopped an invite"),
        "open_joining_changed" => format!("{actor} changed who can join"),
        "message_deleted" => format!("{actor} deleted a message"),
        "file_deleted" => format!("{actor} deleted a file"),
        "device_certificate_changed" => format!("{actor} changed their devices"),
        "owner_claimed" => format!("{actor} set up the server"),
        other => format!("{actor}: {}", other.replace('_', " ")),
    }
}

fn joining_tab(ui: &mut Ui, settings: &mut Settings) -> Option<Admin> {
    let Some((open, cap)) = &mut settings.joining else {
        ui.spinner();
        return None;
    };
    ui.checkbox(open, "Let people join without an invite");
    ui.weak("Anyone who has this server's address can join, up to the limit below.");
    ui.horizontal(|ui| {
        ui.label("Member limit");
        ui.add(DragValue::new(cap).range(1..=100_000));
    });
    ui.button("Save")
        .clicked()
        .then_some(Admin::Joining(*open, *cap))
}

fn listing_tab(ui: &mut Ui, server: &Server, settings: &mut Settings) -> Option<Admin> {
    let Some((on, listing)) = &mut settings.listing else {
        ui.spinner();
        return None;
    };
    ui.checkbox(on, "List this server publicly");
    if !*on {
        ui.weak("Off: this server doesn't appear in any directory.");
        return ui
            .button("Save")
            .clicked()
            .then_some(Admin::SaveListing(None));
    }
    // Start from what people already see.
    if listing.name.is_empty() && listing.address.is_empty() {
        listing.name = server.dir.name.clone().unwrap_or_default();
        listing.address = server.address.to_string();
        listing.index_url = crate::official::DIRECTORY.into();
    }
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(
            RichText::new("Anyone can see this name and description in the directory, even people who aren't members.")
                .strong(),
        );
        ui.label("Everything else in this server stays private and encrypted.");
    });
    let field = |ui: &mut Ui, label: &str, text: &mut String, limit: usize, lines: bool| {
        let label = ui.label(label);
        let edit = if lines {
            TextEdit::multiline(text).desired_rows(3)
        } else {
            TextEdit::singleline(text)
        };
        ui.add(edit.char_limit(limit).desired_width(f32::INFINITY))
            .labelled_by(label.id);
    };
    field(ui, "Public name", &mut listing.name, 100, false);
    field(
        ui,
        "Public description",
        &mut listing.description,
        1_000,
        true,
    );
    field(ui, "Server address", &mut listing.address, 512, false);
    field(ui, "Directory address", &mut listing.index_url, 512, false);
    CollapsingHeader::new("Details").show(ui, |ui| {
        field(
            ui,
            "Directory certificate fingerprint, only for a directory without a domain",
            &mut listing.index_fingerprint,
            64,
            false,
        );
    });
    if !ui.button("Save").clicked() {
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
    validate_details(&listing.name, &listing.description, &listing.address).map_err(|_| {
        "Use a name of up to 100 characters, a description of up to 1,000, no hidden or \
         control characters, and an https:// server address."
    })?;
    https_url(&listing.index_url).ok_or("The directory address must start with https://.")?;
    if !listing.index_fingerprint.is_empty() && decode::<32>(&listing.index_fingerprint).is_err() {
        return Err("That certificate fingerprint isn't right.");
    }
    Ok(())
}

/// Read, write, speak and video per role. Changes save right away.
fn channel_tab(ui: &mut Ui, server: &Server, channel: Id) -> Option<Admin> {
    let Some(found) = server
        .dir
        .categories
        .iter()
        .flat_map(|category| &category.channels)
        .find(|c| c.id == channel)
    else {
        ui.label("This channel was deleted.");
        return None;
    };
    let prefix = if found.voice { "🔊" } else { "#" };
    ui.heading(format!("{prefix} {} settings", found.name));
    ui.label("Who can use this channel");
    let columns: &[(u8, &str)] = if found.voice {
        &[(1, "Read"), (2, "Write"), (4, "Speak"), (8, "Video")]
    } else {
        &[(1, "Read"), (2, "Write")]
    };
    let mut request = None;
    Grid::new("channel-flags").striped(true).show(ui, |ui| {
        ui.label("");
        for (_, label) in columns {
            ui.strong(*label);
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
                if ui.checkbox(&mut on, "").changed() {
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

fn danger(ui: &mut Ui, text: &str) -> egui::Response {
    ui.add(Button::new(text).fill(ui.visuals().error_fg_color))
}

fn date(at: i64) -> String {
    DateTime::from_timestamp(at, 0).map_or_else(String::new, |at| {
        at.with_timezone(&Local)
            .format("%-d %b %Y %H:%M")
            .to_string()
    })
}
