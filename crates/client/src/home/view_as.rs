//! "View server as": a preview of a server as someone with only some of your own roles.
//! It never asks the server for anything: it filters the directory this app already has,
//! so it can hide what those roles can't see but never show more than you can see
//! yourself. While it is on nothing is sent that would change something.

use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui::{self, Align, Frame, Label, Layout, Margin, Panel, Ui};

use super::{Home, View};
use crate::{
    net::Command,
    server::{Directory, Perms, Role, Server},
    ui,
};

/// How long the bar says an action is off, in seconds.
const NOTICE_SECONDS: f64 = 4.0;

pub struct ViewAs {
    /// The previewed roles, highest first.
    pub roles: Vec<String>,
    /// The directory as the server sent it; the preview is made from it again on changes.
    real: Box<Directory>,
    /// Something was not sent since the bar last looked.
    refused: AtomicBool,
    /// Until when the bar says so.
    notice_until: f64,
}

impl ViewAs {
    pub fn real_mut(&mut self) -> &mut Directory {
        &mut self.real
    }

    /// Called by `Server::send` for a command it dropped.
    pub fn refuse(&self) {
        tracing::info!("not sent: the server is shown as other roles");
        self.refused.store(true, Ordering::Relaxed);
    }
}

/// The roles you may preview: only your own, and the role every member gets, so a
/// preview can never stand for more than you have.
pub fn choosable(dir: &Directory) -> Vec<&Role> {
    let mine = dir.members.get(&dir.me).map_or(&[][..], |m| &m.roles[..]);
    (dir.roles.iter())
        .filter(|role| mine.contains(&role.id) || role.id == dir.default_role)
        .collect()
}

/// The directory as someone with only `roles` would see it, from the one you see.
/// Roles you may not preview are ignored, and every permission needs your own too.
pub fn preview(real: &Directory, roles: &[String]) -> Directory {
    let roles: Vec<&Role> = (choosable(real).into_iter())
        .filter(|role| roles.contains(&role.id))
        .collect();
    let ids: Vec<String> = roles.iter().map(|role| role.id.clone()).collect();
    let grants = |permission: &str| {
        (roles.iter()).any(|role| role.permissions.iter().any(|p| p == permission))
    };
    let mine = real.perms;
    // The server lets the owner read every channel, whatever the roles say.
    let owner = mine.owner && ids.iter().any(|id| id == "owner");
    let perms = Perms {
        owner,
        admin: mine.admin && (owner || ids.iter().any(|id| id == "admin")),
        manage_channels: mine.manage_channels && grants("manage_channels"),
        invite: mine.invite && grants("invite"),
        delete_messages: mine.delete_messages && grants("delete_messages"),
        kick: mine.kick && grants("kick"),
        ban: mine.ban && grants("ban"),
        manage_roles: mine.manage_roles && grants("manage_roles"),
        audit: mine.audit && grants("view_audit_log"),
        manage_server: mine.manage_server && grants("manage_server"),
        manage_emoji: mine.manage_emoji && grants("manage_emoji"),
        mute_members: mine.mute_members && grants("mute_members"),
        move_members: mine.move_members && grants("move_members"),
        suggest_events: mine.suggest_events && grants("suggest_events"),
        create_events: mine.create_events && grants("create_events"),
        manage_events: mine.manage_events && grants("manage_events"),
        start_focus: mine.start_focus && grants("start_focus"),
        bypass_slow_mode: mine.bypass_slow_mode && grants("bypass_slow_mode"),
        timeout_members: mine.timeout_members && grants("timeout_members"),
        lock_down: mine.lock_down && grants("lock_down"),
        remove_recent_messages: mine.remove_recent_messages && grants("remove_recent_messages"),
        review_reports: mine.review_reports && grants("review_reports"),
        stats: mine.stats && grants("view_server_stats"),
        rank: (roles.iter().map(|role| role.rank).max())
            .unwrap_or(-1)
            .min(mine.rank),
        preview: true,
    };
    let mut dir = real.clone();
    dir.perms = perms;
    // A channel shows when one of the roles may read it, as on the server.
    for category in &mut dir.categories {
        category.channels.retain(|channel| {
            owner
                || (channel.flags.iter()).any(|(role, flags)| flags & 1 != 0 && ids.contains(role))
        });
    }
    // Members never see a category without a channel they can read.
    dir.categories
        .retain(|category| !category.channels.is_empty());
    if let Some(me) = (dir.members.edit().iter_mut()).find(|m| m.person == real.me) {
        me.roles = ids;
    }
    dir
}

/// Show `server` as someone with only these roles, or change the roles shown.
pub fn show_as(server: &mut Server, roles: Vec<String>) {
    let (real, notice_until) = match server.view_as.take() {
        Some(view) => (*view.real, view.notice_until),
        None => (std::mem::take(&mut server.dir), 0.0),
    };
    let roles: Vec<String> = (choosable(&real).into_iter())
        .filter(|role| roles.contains(&role.id))
        .map(|role| role.id.clone())
        .collect();
    server.dir = preview(&real, &roles);
    server.view_as = Some(ViewAs {
        roles,
        real: Box::new(real),
        refused: AtomicBool::new(false),
        notice_until,
    });
}

/// Back to the server as it is. Returns the roles that were shown.
pub fn exit(server: &mut Server) -> Option<Vec<String>> {
    let view = server.view_as.take()?;
    server.dir = *view.real;
    Some(view.roles)
}

/// What may still be sent while previewing: reading what you can already read, your
/// own presence, which the app keeps in step by itself, and stopping or declining
/// things. Everything else would change something and is dropped; new commands are too.
pub fn allowed(command: &Command) -> bool {
    use Command::*;
    matches!(
        command,
        Open(_)
            | Older(..)
            | Backfill(_)
            | CheckGone(..)
            | Image(..)
            | LinkCard(_)
            | StickerFile(..)
            | Save(..)
            | SharedFiles(_)
            | SetStatus(_)
            | SetHideOffline(_)
            | SyncVoiceStatus
            | Watch(..)
            | Listen(..)
            | WatchTerminal(_)
            | LeaveVoice
            | StopApp(_)
            | StopScreen
            | StopCamera
            | StopTerminal
            | StopTyping
            | ReleaseControl
            | StopControl
            | CancelFile(_)
            | RevokeTyping(_)
            | AnswerControl(_, false)
            | AnswerTyping(_, false)
    )
}

/// The bar across the top while a server is previewed: its roles as chips to add or
/// remove, and Exit. Leaving the server ends the preview.
pub fn bar(ui: &mut Ui, home: &mut Home) {
    let shown = match home.view {
        View::Server(index) => Some(index),
        _ => None,
    };
    for (index, server) in home.servers.iter_mut().enumerate() {
        if Some(index) != shown {
            exit(server);
        }
    }
    let Some(index) = shown else {
        return;
    };
    let server = &mut home.servers[index];
    let Some(view) = &mut server.view_as else {
        return;
    };
    let now = ui.input(|input| input.time);
    if view.refused.swap(false, Ordering::Relaxed) {
        view.notice_until = now + NOTICE_SECONDS;
    }
    let notice = view.notice_until > now;
    if notice {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs_f64(view.notice_until - now));
    }
    let (c, accent) = (ui::colors(ui), ui::accent(ui));
    let roles = view.roles.clone();
    let shown_roles: Vec<Role> = (view.real.roles.iter())
        .filter(|role| roles.contains(&role.id))
        .cloned()
        .collect();
    let addable: Vec<Role> = (choosable(&view.real).into_iter())
        .filter(|role| !roles.contains(&role.id))
        .cloned()
        .collect();
    let (mut close, mut remove, mut add) = (false, None, None);
    let bar = Panel::top("view-as bar")
        .frame(
            Frame::new()
                .fill(c.main.lerp_to_gamma(accent.base, 0.18))
                .inner_margin(Margin::symmetric(ui::SPACE_M as i8, ui::SPACE_XS as i8)),
        )
        .show(ui, |ui| {
            let row = egui::vec2(ui.available_width(), ui::CONTROL_HEIGHT);
            ui.allocate_ui_with_layout(row, Layout::left_to_right(Align::Center), |ui| {
                let title = ui::bold(t!("view_as.title")).color(c.strong);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    close = ui::primary(ui, t!("view_as.exit")).clicked();
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.add(Label::new(title).truncate());
                    });
                });
            });
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(ui::SPACE_XS, ui::SPACE_XS);
                for role in &shown_roles {
                    // The last role stays: a preview of nobody shows nothing.
                    if chip(ui, role, shown_roles.len() > 1) {
                        remove = Some(role.id.clone());
                    }
                }
                if !addable.is_empty() {
                    let button = ui::secondary(ui, t!("view_as.add_role"));
                    ui::menu(&button, |m| {
                        for role in &addable {
                            let dot = role.color.unwrap_or(c.muted);
                            if m.add(ui::Item::dot(dot, &role.name)).clicked() {
                                add = Some(role.id.clone());
                            }
                        }
                    });
                }
            });
            let note = if notice {
                ui::caption(t!("view_as.refused")).color(c.warning)
            } else {
                ui::caption(t!("view_as.note"))
            };
            ui.add(Label::new(note).wrap());
        });
    let rect = bar.response.rect;
    let line = egui::Stroke::new(1.0, accent.base.gamma_multiply(0.6));
    ui.painter().hline(rect.x_range(), rect.bottom(), line);
    if close {
        exit(server);
    } else if let Some(id) = remove {
        show_as(server, roles.into_iter().filter(|r| *r != id).collect());
    } else if let Some(id) = add {
        show_as(server, roles.into_iter().chain([id]).collect());
    }
}

/// A previewed role: its colour, its name and, when `removable`, a × to drop it.
/// Returns whether the × was clicked.
fn chip(ui: &mut Ui, role: &Role, removable: bool) -> bool {
    let c = ui::colors(ui);
    Frame::new()
        .fill(c.button)
        .stroke(egui::Stroke::new(1.0, c.border))
        .corner_radius(ui::RADIUS)
        .inner_margin(Margin::symmetric(ui::SPACE_S as i8, 1))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = ui::SPACE_XS;
                ui::color_dot(ui, role.color);
                ui.label(&role.name);
                removable && ui::small_close(ui, &t!("view_as.remove", role = role.name)).clicked()
            })
            .inner
        })
        .inner
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{Category, Channel, Member};
    use rusttalk_core::presence::Presence;

    const ME: [u8; 32] = [1; 32];

    fn role(id: &str, rank: i64, permissions: &[&str]) -> Role {
        Role {
            id: id.into(),
            name: id.into(),
            name_known: true,
            rank,
            permissions: permissions.iter().map(|p| p.to_string()).collect(),
            custom: !matches!(id, "owner" | "admin" | "moderator" | "member"),
            color: None,
            separate: false,
        }
    }

    fn channel(n: u8, readers: &[&str]) -> Channel {
        Channel {
            id: [n; 32],
            name: format!("c{n}"),
            topic: String::new(),
            voice: false,
            page: false,
            forum: false,
            audio_kbps: 0,
            member_limit: 0,
            slow_mode_secs: 0,
            nsfw: false,
            focus_until: None,
            flags: readers.iter().map(|r| (r.to_string(), 3)).collect(),
        }
    }

    /// A moderator who also has Helpers, on a server with an Admin and a Secret role
    /// they lack. The client only got the channels they can read.
    fn moderator() -> Directory {
        let mut dir = Directory {
            me: ME,
            default_role: "member".into(),
            roles: vec![
                role("owner", 1000, &["kick", "ban", "manage_roles"]),
                role(
                    "admin",
                    900,
                    &["kick", "ban", "manage_roles", "manage_server"],
                ),
                role("moderator", 50, &["kick", "delete_messages"]),
                role("secret", 40, &["ban", "manage_server"]),
                role("helpers", 30, &["invite"]),
                role("member", 0, &[]),
            ],
            categories: vec![
                Category {
                    id: Some([10; 32]),
                    name: "Staff".into(),
                    channels: vec![channel(1, &["moderator"]), channel(2, &["helpers"])],
                },
                Category {
                    id: Some([11; 32]),
                    name: "Everyone".into(),
                    channels: vec![channel(3, &["member", "moderator"])],
                },
            ],
            members: vec![Member {
                person: ME,
                name: Some("Me".into()),
                devices: Vec::new(),
                roles: vec!["moderator".into(), "helpers".into()],
                avatar: None,
                presence: Presence::Online,
                hide_offline: false,
                bio: None,
                voice_muted: false,
                timeout_until: None,
            }]
            .into(),
            ..Directory::default()
        };
        dir.perms = Perms {
            kick: true,
            delete_messages: true,
            invite: true,
            rank: 50,
            ..Perms::default()
        };
        dir
    }

    fn channels(dir: &Directory) -> Vec<u8> {
        (dir.categories.iter().flat_map(|c| &c.channels))
            .map(|c| c.id[0])
            .collect()
    }

    #[test]
    fn only_your_own_roles_and_the_member_role_can_be_picked() {
        let dir = moderator();
        let picks: Vec<&str> = choosable(&dir).iter().map(|r| r.id.as_str()).collect();
        assert_eq!(picks, ["moderator", "helpers", "member"]);

        let mut server = crate::home::new_server(
            rusttalk_core::auth::KnownServer {
                address: "https://example.org/".parse().unwrap(),
                server_id: [2; 32],
                tls_fingerprint: None,
            },
            "Example",
            tokio::sync::mpsc::unbounded_channel().0,
        );
        server.dir = dir;
        show_as(
            &mut server,
            vec!["admin".into(), "secret".into(), "helpers".into()],
        );
        assert_eq!(server.view_as.as_ref().unwrap().roles, ["helpers"]);
        assert!(!server.dir.perms.ban && !server.dir.perms.manage_server);
        assert_eq!(exit(&mut server).unwrap(), ["helpers"]);
        assert!(server.dir.perms.kick && !server.dir.perms.preview);
    }

    #[test]
    fn a_preview_never_grants_what_you_lack() {
        let mut dir = moderator();
        // Even a role whose permissions say more than yours (here: the member role,
        // which you may preview without holding it) gives nothing you don't have.
        dir.roles[5].permissions = vec!["ban".into(), "manage_server".into(), "invite".into()];
        dir.members.edit()[0].roles = vec!["moderator".into()];
        dir.perms.invite = false;
        let all = ["owner", "admin", "moderator", "secret", "helpers", "member"].map(String::from);
        let shown = preview(&dir, &all);
        let (real, seen) = (dir.perms, shown.perms);
        for (has, may) in [
            (real.owner, seen.owner),
            (real.admin, seen.admin),
            (real.ban, seen.ban),
            (real.invite, seen.invite),
            (real.manage_server, seen.manage_server),
            (real.manage_roles, seen.manage_roles),
            (real.kick, seen.kick),
        ] {
            assert!(may <= has);
        }
        assert!(seen.rank <= real.rank && seen.preview);
        assert!(channels(&shown).iter().all(|c| channels(&dir).contains(c)));
    }

    #[test]
    fn channels_and_empty_categories_follow_the_roles() {
        let dir = moderator();
        let member = preview(&dir, &["member".into()]);
        assert_eq!(channels(&member), [3]);
        // The Staff category has no channel left for a member, so it goes.
        assert_eq!(member.categories.len(), 1);
        assert!(!member.perms.kick && !member.perms.delete_messages);
        assert_eq!(member.members.get(&ME).unwrap().roles, ["member"]);

        let helpers = preview(&dir, &["helpers".into(), "member".into()]);
        assert_eq!(channels(&helpers), [2, 3]);
        assert!(helpers.perms.invite && !helpers.perms.kick);
    }

    #[test]
    fn nothing_that_changes_anything_is_sent() {
        let (commands, mut sent) = tokio::sync::mpsc::unbounded_channel();
        let mut server = crate::home::new_server(
            rusttalk_core::auth::KnownServer {
                address: "https://example.org/".parse().unwrap(),
                server_id: [2; 32],
                tls_fingerprint: None,
            },
            "Example",
            commands,
        );
        server.dir = moderator();
        show_as(&mut server, vec!["moderator".into()]);
        server.send(Command::DeleteChannel([1; 32]));
        server.send(Command::JoinVoice([1; 32]));
        server.send(Command::Invite(1, 60));
        server.send(Command::SetServerName("x".into()));
        assert!(sent.try_recv().is_err());
        assert!(
            server
                .view_as
                .as_ref()
                .unwrap()
                .refused
                .load(Ordering::Relaxed)
        );
        // Reading what you can already read still works.
        server.send(Command::Open([3; 32]));
        assert!(matches!(sent.try_recv(), Ok(Command::Open(_))));
        exit(&mut server);
        server.send(Command::Invite(1, 60));
        assert!(matches!(sent.try_recv(), Ok(Command::Invite(..))));
    }
}
