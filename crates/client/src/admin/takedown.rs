//! Server settings > Takedowns (#679): the owner removes content for a legal reason, by an
//! ID pasted in or from a message's menu, and sees the record of what was removed before.

use eframe::egui::{self, CollapsingHeader, Ui};

use crate::{
    net::{Admin, TakeDown, Takedown},
    server::{Id, Server},
    ui,
};

/// What the server takes, in characters.
const REASON_CHARS: usize = 1000;
const NOTICE_CHARS: usize = 200;

/// The reason, notice and ban choice, and on the page the IDs to remove.
#[derive(Default)]
pub struct Form {
    ids: String,
    reason: String,
    notice: String,
    ban: bool,
}

impl Form {
    /// Why, the notice and whether to ban: the same on the page and in a message's dialog.
    pub fn details(&mut self, ui: &mut Ui) {
        let label = ui.label(ui::bold(t!("takedown.reason")));
        ui.add(
            ui::text_area(&mut self.reason)
                .desired_rows(3)
                .char_limit(REASON_CHARS)
                .desired_width(f32::INFINITY),
        )
        .labelled_by(label.id);
        ui.weak(t!("takedown.reason_help"));
        let label = ui.label(ui::bold(t!("takedown.notice")));
        ui.add(
            ui::field(&mut self.notice)
                .char_limit(NOTICE_CHARS)
                .desired_width(f32::INFINITY),
        )
        .labelled_by(label.id);
        ui.weak(t!("takedown.notice_help"));
        ui::checkbox(ui, &mut self.ban, t!("takedown.ban"));
        ui.weak(t!("takedown.ban_help"));
    }

    /// The takedown of `ids`, once there is a reason.
    pub fn request(&self, ids: Vec<Id>) -> Option<TakeDown> {
        let reason = self.reason.trim();
        (!ids.is_empty() && !reason.is_empty()).then(|| TakeDown {
            ids,
            reason: reason.to_owned(),
            notice: self.notice.trim().to_owned(),
            ban: self.ban,
        })
    }
}

/// IDs pasted in, separated by spaces, commas or lines: as "Copy message ID" gives them
/// (hex) or as the server writes them. `None` if any is not an ID.
pub fn parse_ids(text: &str) -> Option<Vec<Id>> {
    let mut ids = Vec::new();
    for word in text.split(|c: char| c.is_whitespace() || c == ',') {
        if word.is_empty() {
            continue;
        }
        let id = (hex::decode(word)
            .ok()
            .and_then(|bytes| bytes.try_into().ok()))
        .or_else(|| rusttalk_core::auth::decode(word).ok())?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Some(ids)
}

/// The page: remove by ID, then the takedowns so far. What to send when Remove was pressed.
pub(super) fn page(
    ui: &mut Ui,
    server: &Server,
    form: &mut Form,
    list: Option<&[Takedown]>,
) -> Option<Admin> {
    let mut request = None;
    ui::page_section(ui, t!("takedown.remove"));
    ui.add(egui::Label::new(t!("takedown.remove_help")).wrap());
    ui.add_space(ui::SPACE_S);
    let label = ui.label(ui::bold(t!("takedown.ids")));
    ui.add(
        ui::text_area(&mut form.ids)
            .desired_rows(2)
            .desired_width(f32::INFINITY),
    )
    .labelled_by(label.id);
    let ids = parse_ids(&form.ids);
    if ids.is_none() {
        ui::error_text(ui, Some(t!("takedown.bad_ids")));
    } else {
        ui.weak(t!("takedown.ids_help"));
    }
    form.details(ui);
    let ready = ids.and_then(|ids| form.request(ids));
    ui.add_space(ui::SPACE_S);
    let remove = ui
        .add_enabled_ui(ready.is_some(), |ui| {
            ui::danger(ui, t!("takedown.remove_button"))
        })
        .inner
        .clicked();
    if remove {
        request = ready.map(Admin::TakeDown);
    }

    ui::page_section(ui, t!("takedown.history"));
    match list {
        None => {
            ui.spinner();
        }
        Some([]) => {
            ui.weak(t!("takedown.none"));
        }
        Some(list) => {
            for (index, record) in list.iter().enumerate() {
                ui.push_id(index, |ui| shown(ui, server, record));
                ui.add_space(ui::SPACE_S);
            }
        }
    }
    request
}

/// One takedown: why, when and by whom, what went, and its IDs and hashes to copy.
fn shown(ui: &mut Ui, server: &Server, record: &Takedown) {
    ui::card(ui, |ui| {
        ui.add(egui::Label::new(ui::bold(&record.reason)).wrap());
        ui.weak(t!(
            "takedown.by",
            date = super::date(record.at),
            name = server.member_name(&record.actor)
        ));
        if let Some(notice) = &record.notice {
            ui.add(egui::Label::new(t!("takedown.notice_shown", notice = notice)).wrap());
        }
        let messages = record.items.iter().filter(|item| !item.file).count();
        let files = record.items.len() - messages;
        ui.label(t!(
            "audit.removed_counts",
            messages = messages,
            files = files
        ));
        if let Some(person) = &record.banned {
            ui.label(t!("takedown.banned", name = server.member_name(person)));
        }
        CollapsingHeader::new(t!("takedown.details"))
            .id_salt("details")
            .show(ui, |ui| {
                for item in &record.items {
                    ui.add(egui::Label::new(egui::RichText::new(line(item)).monospace()).wrap());
                }
                if ui::secondary(ui, t!("takedown.copy")).clicked() {
                    ui.ctx().copy_text(copied(server, record));
                }
            });
    });
}

/// "message 3f2a… · 120 bytes · SHA-256 9b1c…", IDs in hex as "Copy message ID" gives them.
fn line(item: &crate::net::TakenDown) -> String {
    let (id, size) = (hex::encode(item.id), item.size);
    let hash = item.sha256.as_deref().unwrap_or("-");
    match item.file {
        true => t!("takedown.item_file", id = id, size = size, hash = hash),
        false => t!("takedown.item_message", id = id, size = size, hash = hash),
    }
}

/// The record as plain text, to keep with the notice.
fn copied(server: &Server, record: &Takedown) -> String {
    let mut text = format!(
        "{}\n{}\n",
        record.reason,
        t!(
            "takedown.by",
            date = super::date(record.at),
            name = server.member_name(&record.actor)
        )
    );
    if let Some(notice) = &record.notice {
        text += &format!("{}\n", t!("takedown.notice_shown", notice = notice));
    }
    if let Some(person) = &record.banned {
        text += &format!("{}\n", t!("takedown.banned", name = hex::encode(person)));
    }
    for item in &record.items {
        text += &format!("{}\n", line(item));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::parse_ids;

    #[test]
    fn pasted_ids_take_hex_or_base64_and_refuse_anything_else() {
        let hex = hex::encode([7; 32]);
        let base64 = rusttalk_core::auth::encode(&[9; 32]);
        let text = format!(" {hex},\n{base64} {hex} ");
        assert_eq!(parse_ids(&text), Some(vec![[7; 32], [9; 32]]));
        assert_eq!(parse_ids(""), Some(Vec::new()));
        assert_eq!(parse_ids("not-an-id"), None);
        assert_eq!(parse_ids(&hex[..10]), None);
    }
}
