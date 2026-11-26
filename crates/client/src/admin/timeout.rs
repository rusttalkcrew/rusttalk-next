//! Timeouts (#663): a moderator picks how long a member may only read, with an optional
//! reason; the member sees a banner until it ends. The server enforces it.

use chrono::{Local, TimeZone};
use eframe::egui::{self, Frame, Margin, RichText, Ui};

use crate::{
    home::{Home, person::Person},
    net::{Admin, Command},
    notify::unix_now,
    server::{Id, Server, Status},
    ui,
};

/// The lengths the server accepts, in seconds.
const LENGTHS: [i64; 6] = [60, 300, 600, 3600, 86_400, 604_800];
/// The longest reason; its ciphertext must stay within the server's small-metadata limit.
const REASON_CHARS: usize = 200;

/// The open "Time out" dialog: frozen like a kick, so a reordered list or a reconnect
/// can't retarget it.
pub struct Dialog {
    server: Id,
    person: Id,
    name: String,
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
    length: i64,
    reason: String,
}

fn allowed(home: &Home, server: &Server, person: Id) -> bool {
    server.status == Status::Online
        && server.dir.perms.timeout_members
        && person != server.dir.me
        && Person::of(home, server, person).outranked()
}

/// Open the dialog for `person` on server `index`, if this person may time them out.
pub fn open(home: &mut Home, index: usize, person: Id) {
    let server = &home.servers[index];
    if home.timeout.is_none() && allowed(home, server, person) {
        home.timeout = Some(Dialog {
            server: server.known.server_id,
            person,
            name: server.member_name(&person),
            commands: server.commands.clone(),
            length: LENGTHS[1],
            reason: String::new(),
        });
    }
}

pub fn dialog(ctx: &egui::Context, home: &mut Home) {
    let Some(mut open) = home.timeout.take() else {
        return;
    };
    let Some(server) = home.servers.iter().find(|server| {
        server.known.server_id == open.server && server.commands.same_channel(&open.commands)
    }) else {
        return;
    };
    if !allowed(home, server, open.person) {
        return;
    }
    let (mut confirm, mut cancel) = (false, false);
    let ((), dismissed) = ui::dialog(
        ctx,
        "timeout-member",
        &t!("timeout.title", name = open.name),
        |ui| {
            ui.label(t!("timeout.help"));
            ui.add_space(ui::SPACE_M);
            let choices: Vec<_> = (LENGTHS.iter())
                .map(|length| (*length, label(*length), ""))
                .collect();
            ui::segmented(ui, &mut open.length, &choices);
            ui.add_space(ui::SPACE_S);
            ui.label(ui::bold(t!("timeout.reason")));
            ui.add(
                ui::text_area(&mut open.reason)
                    .char_limit(REASON_CHARS)
                    .hint_text(t!("timeout.reason_hint"))
                    .desired_rows(2)
                    .desired_width(f32::INFINITY),
            );
            ui::footer(ui, |ui| {
                confirm = ui::danger(ui, t!("timeout.confirm")).clicked();
                cancel = ui::secondary(ui, t!("common.cancel")).clicked();
            });
        },
    );
    if confirm && !cancel && !dismissed {
        let reason = open.reason.trim();
        server.send(Command::Admin(Admin::Timeout {
            person: open.person,
            seconds: open.length,
            reason: (!reason.is_empty()).then(|| reason.to_owned()),
        }));
    } else if !cancel && !dismissed {
        home.timeout = Some(open);
    }
}

/// "5 min" for one of the offered lengths in seconds.
pub fn label(seconds: i64) -> &'static str {
    match seconds {
        60 => t!("timeout.length_60s"),
        300 => t!("timeout.length_5m"),
        600 => t!("timeout.length_10m"),
        3600 => t!("timeout.length_1h"),
        86_400 => t!("timeout.length_1d"),
        _ => t!("timeout.length_1w"),
    }
}

/// A unix time as the clock shows it: "2:32 PM" today, with the date further out.
pub fn clock(at: i64) -> String {
    let Some(at) = Local.timestamp_opt(at, 0).single() else {
        return String::new();
    };
    let pattern = match at.date_naive() == Local::now().date_naive() {
        true => "time.clock",
        false => "time.day_clock",
    };
    crate::i18n::time(at, pattern)
}

/// "You're in a timeout until 14:32" above the composer, with the moderator's reason,
/// in place of the composer. False when this person isn't in a timeout here.
pub fn banner(ui: &mut Ui, server: &Server) -> bool {
    let Some(until) = server.timed_out() else {
        return false;
    };
    // Repaint at the end, so the composer comes back on its own.
    let left = (until - unix_now()).clamp(1, 60) as u64;
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_secs(left));
    Frame::new()
        .fill(ui::colors(ui).field)
        .corner_radius(ui::RADIUS_S)
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let text = t!("timeout.banner", time = clock(until));
            ui.label(RichText::new(text).family(ui::bold_family()));
            if let Some(reason) = server.dir.timeout_reason.as_deref() {
                ui.label(ui::caption(t!("timeout.banner_reason", reason = reason)));
            }
            ui.label(ui::caption(t!("timeout.banner_help")));
        });
    true
}
