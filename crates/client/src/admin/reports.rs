//! Reports (#678): the queue for people who review reports. Each report was opened and
//! checked on this device: the reporter's signature, and the reported message against
//! its sender's signature, decrypted with the key the reporter disclosed for it alone.

use eframe::egui::{self, Frame, Margin, RichText, Ui};
use rusttalk_core::{
    client::{Checked, ReceivedReport, Unreadable},
    messages::Attachment,
};

use super::{date, timeout};
use crate::{
    home::report::reason_label,
    net::{Admin, ReportAction},
    server::{Directory, Id, Server},
    ui,
};

/// Reviewer devices from which the role page warns: a report is sealed to at most
/// `MAX_REVIEWER_DEVICES` (200), and refused above.
pub const DEVICES_WARNING: usize = 150;

/// The devices a channel report is sealed to: every device of the owner and of members
/// whose roles include `review_reports`, with `editing` (a role and its unsaved
/// permissions) counted as it would be saved.
pub fn reviewer_devices(dir: &Directory, editing: Option<(&str, &[String])>) -> usize {
    let reviewing: Vec<&str> = (dir.roles.iter())
        .filter(|role| {
            let permissions = match editing {
                Some((id, chosen)) if id == role.id => chosen,
                _ => &role.permissions,
            };
            permissions.iter().any(|p| p == "review_reports")
        })
        .map(|role| role.id.as_str())
        .collect();
    (dir.members.iter())
        .filter(|member| {
            (member.roles.iter()).any(|role| role == "owner" || reviewing.contains(&role.as_str()))
        })
        .map(|member| member.devices.len())
        .sum()
}

/// A kick, ban or delete waiting for "Are you sure?".
pub struct Confirm {
    report: Id,
    sender: Id,
    name: String,
    action: ReportAction,
    /// The verified message's stored files, deleted with it.
    files: Vec<Id>,
}

/// What the page asks for.
pub enum Asked {
    Request(Admin),
    /// Save a reported file here, decrypted with its key from the report.
    Save(Attachment),
}

pub fn page(
    ui: &mut Ui,
    server: &Server,
    reports: &[ReceivedReport],
    confirm: &mut Option<Confirm>,
) -> Option<Asked> {
    ui.label(ui::caption(t!("reports.page_help")));
    ui.add_space(ui::SPACE_S);
    if reports.is_empty() {
        ui.label(t!("reports.empty"));
        return None;
    }
    let mut asked = None;
    for report in reports {
        // A reporter with several open reports of one kind (a spammer, say) can be
        // dismissed in one go: channel reports, or DM reports (only the owner sees those).
        // As the server counts: reports about yourself only if you're the owner.
        let theirs = (reports.iter())
            .filter(|other| other.reporter == report.reporter && other.dm == report.dm)
            .filter(|other| server.dir.perms.owner || other.sender != server.dir.me)
            .count();
        if let Some(picked) = card(ui, server, report, theirs, confirm) {
            asked = Some(picked);
        }
        ui.add_space(ui::SPACE_S);
    }
    asked
}

fn card(
    ui: &mut Ui,
    server: &Server,
    report: &ReceivedReport,
    by_reporter: usize,
    confirm: &mut Option<Confirm>,
) -> Option<Asked> {
    let perms = server.dir.perms;
    let sender = server.member_name(&report.sender);
    let mut asked = None;
    ui::card(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            let reporter = server.member_name(&report.reporter);
            let title = t!("reports.reported", reporter = reporter, sender = sender);
            ui.label(ui::bold(title));
            if report.dm {
                ui::badge(ui, t!("reports.from_dm"), ui::accent(ui).base);
            }
        });
        ui.label(ui::caption(date(report.at)));
        let dismiss_all = |ui: &mut Ui, asked: &mut Option<Asked>| {
            if by_reporter > 1 {
                let reporter = server.member_name(&report.reporter);
                let label = t!(
                    "reports.dismiss_all",
                    reporter = reporter,
                    count = by_reporter
                );
                if ui::secondary(ui, &label).clicked() {
                    let dismiss = Admin::DismissFrom(report.reporter, report.dm);
                    *asked = Some(Asked::Request(dismiss));
                }
            }
        };
        let checked = match &report.content {
            Ok(checked) => checked,
            Err(unreadable) => {
                match unreadable {
                    // Not a bad report: it went to this person's devices of that time.
                    Unreadable::NotSealedHere => {
                        ui.label(ui::bold(t!("reports.not_sealed_here")));
                        ui.label(ui::caption(t!("reports.not_sealed_here_help")));
                    }
                    Unreadable::Invalid(_) => {
                        let text = RichText::new(t!("reports.unverified"));
                        ui.label(text.color(ui::colors(ui).danger_text));
                        ui.label(ui::caption(t!("reports.unverified_help")));
                    }
                }
                ui::footer(ui, |ui| {
                    if ui::secondary(ui, t!("reports.dismiss")).clicked() {
                        asked = Some(request(report, ReportAction::Dismiss));
                    }
                    dismiss_all(ui, &mut asked);
                });
                return;
            }
        };
        ui.label(t!("reports.reason", reason = reason_label(checked.reason)));
        if !checked.note.is_empty() {
            ui.label(ui::caption(t!("reports.note_from", note = checked.note)));
        }
        ui.add_space(ui::SPACE_XS);
        // A removed device may have been stolen: its signature proves less, so no tick.
        if checked.sender_removed {
            let text = t!("reports.device_removed", sender = sender);
            ui.label(RichText::new(text).color(ui::colors(ui).warning));
            ui.label(ui::caption(t!("reports.device_removed_help")));
        } else {
            let text = t!("reports.verified", sender = sender);
            ui.label(RichText::new(text).color(ui::colors(ui).success));
        }
        if let Some(file) = message(ui, checked) {
            asked = Some(file);
        }
        // DMs are private to their two people: the owner reviews but doesn't delete,
        // and a timeout leaves DMs open.
        let delete = perms.delete_messages && !report.dm;
        let can_timeout = perms.timeout_members && !report.dm;
        let mut picked = None;
        // Right-aligned like a dialog's buttons; on a phone in two rows, so they fit.
        let mut removal = |ui: &mut Ui| {
            if delete && ui::danger(ui, t!("reports.delete")).clicked() {
                picked = Some(ReportAction::Delete);
            }
            if can_timeout {
                let button = ui::secondary(ui, t!("reports.timeout"));
                ui::menu_below(&button, |m| {
                    for seconds in [600, 3600, 86_400, 604_800] {
                        if m.item("⏳", timeout::label(seconds)).clicked() {
                            picked = Some(ReportAction::Timeout(seconds));
                        }
                    }
                });
            }
        };
        let people = |ui: &mut Ui, picked: &mut Option<ReportAction>| {
            if ui::secondary(ui, t!("reports.dismiss")).clicked() {
                *picked = Some(ReportAction::Dismiss);
            }
            if perms.ban && ui::danger(ui, t!("admin.ban")).clicked() {
                *picked = Some(ReportAction::Ban);
            }
            if perms.kick && ui::secondary(ui, t!("admin.kick")).clicked() {
                *picked = Some(ReportAction::Kick);
            }
        };
        let mut chosen = None;
        if ui::is_phone(ui.ctx()) {
            ui::footer(ui, |ui| people(ui, &mut chosen));
            if delete || can_timeout {
                ui::footer(ui, &mut removal);
            }
        } else {
            ui::footer(ui, |ui| {
                people(ui, &mut chosen);
                removal(ui);
            });
        }
        match chosen.or(picked) {
            Some(action @ (ReportAction::Dismiss | ReportAction::Timeout(_))) => {
                asked = Some(request(report, action));
            }
            Some(action) => {
                *confirm = Some(Confirm {
                    report: report.id,
                    sender: report.sender,
                    name: sender.clone(),
                    action,
                    files: (checked.body.attachments.iter())
                        .flat_map(|file| std::iter::once(&file.file).chain(&file.thumbnail))
                        .map(|blob| blob.id)
                        .collect(),
                });
            }
            None => {}
        }
        if by_reporter > 1 {
            ui::footer(ui, |ui| dismiss_all(ui, &mut asked));
        }
    });
    asked
}

/// The reported text as it was shown, then its files, each with Save.
fn message(ui: &mut Ui, checked: &Checked) -> Option<Asked> {
    let mut asked = None;
    Frame::new()
        .fill(ui::colors(ui).field)
        .corner_radius(ui::RADIUS_S)
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if !checked.body.text.is_empty() {
                ui.add(egui::Label::new(&checked.body.text).wrap());
            }
            for file in &checked.body.attachments {
                ui::trailing_row(
                    ui,
                    |ui| {
                        // Phones save through the system's sheet, named like elsewhere.
                        let label = crate::files::export_label().unwrap_or(t!("reports.save_file"));
                        if ui::secondary(ui, label).clicked() {
                            asked = Some(Asked::Save(file.clone()));
                        }
                    },
                    |ui| {
                        let size = crate::files::size(file.file.size.into());
                        ui.add(egui::Label::new(format!("📎 {} ({size})", file.name)).truncate());
                    },
                );
            }
        });
    asked
}

fn request(report: &ReceivedReport, action: ReportAction) -> Asked {
    Asked::Request(Admin::HandleReport {
        id: report.id,
        sender: report.sender,
        action,
        files: Vec::new(),
    })
}

/// "Ban Sam?" over the settings, for the actions that can't be undone from here.
pub fn confirm_dialog(ctx: &egui::Context, confirm: &mut Option<Confirm>) -> Option<Admin> {
    let open = confirm.as_ref()?;
    let (title, help, label) = match open.action {
        ReportAction::Delete => (
            t!("reports.delete_title").to_owned(),
            t!("reports.delete_help"),
            t!("reports.delete"),
        ),
        ReportAction::Kick => (
            t!("admin.kick_title", name = open.name),
            t!("admin.kick_help"),
            t!("admin.kick"),
        ),
        _ => (
            t!("reports.ban_title", name = open.name),
            t!("reports.ban_help"),
            t!("admin.ban"),
        ),
    };
    let (mut yes, mut no) = (false, false);
    let ((), dismissed) = ui::dialog(ctx, "report-action", &title, |ui| {
        ui.label(help);
        ui::footer(ui, |ui| {
            yes = ui::danger(ui, label).clicked();
            no = ui::secondary(ui, t!("common.cancel")).clicked();
        });
    });
    if !(yes || no || dismissed) {
        return None;
    }
    let open = confirm.take()?;
    (yes && !no && !dismissed).then_some(Admin::HandleReport {
        id: open.report,
        sender: open.sender,
        action: open.action,
        files: open.files,
    })
}
