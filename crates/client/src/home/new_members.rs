//! New member rules as this person meets them: what waits, and until when.

use std::time::Duration;

use eframe::egui::Ui;

use crate::{server::Directory, ui};

/// When the new member rules stop applying to this person here, while they still do.
pub fn until(dir: &Directory) -> Option<i64> {
    dir.new_member_until
        .filter(|end| *end > crate::notify::unix_now())
}

/// The short note above the message box that says what unlocks when.
pub(super) fn note(ui: &mut Ui, dir: &Directory) {
    let Some(end) = until(dir) else {
        return;
    };
    let left = (end - crate::notify::unix_now()).max(1) as u64;
    ui.ctx().request_repaint_after(Duration::from_secs(left));
    ui::banner(
        ui,
        "new-member-rules",
        &t!("new_members.note", time = super::time(end)),
    );
}

/// Refused here before asking the server, which refuses it too.
#[derive(Debug)]
pub struct Refused;

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the new member rules apply")
    }
}

impl std::error::Error for Refused {}
