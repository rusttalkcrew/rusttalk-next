use eframe::egui::Ui;
use rusttalk_core::auth::{FileLimits, FileSettings};

use crate::{files, ui};

pub(super) struct Form {
    settings: FileSettings,
    fields: [String; 5],
}

impl Form {
    pub(super) fn new(settings: FileSettings) -> Self {
        Self {
            fields: values(settings.limits).map(|value| value.to_string()),
            settings,
        }
    }

    pub(super) fn show(&mut self, ui: &mut Ui, editable: bool) -> Option<FileLimits> {
        let FileSettings {
            limits,
            ceilings,
            used_bytes,
        } = self.settings;
        ui::setting_row(ui, t!("admin.storage_used"), "", |ui| {
            ui.label(t!(
                "admin.storage_usage",
                used = files::size(used_bytes as u64),
                total = files::size(limits.server_storage_bytes as u64)
            ));
        });
        let labels = [
            (t!("admin.max_file_size"), t!("admin.max_file_size_bytes")),
            (t!("admin.member_storage"), t!("admin.member_storage_bytes")),
            (t!("admin.server_storage"), t!("admin.server_storage_bytes")),
            (t!("admin.file_retention"), t!("admin.file_retention_secs")),
            (t!("admin.max_stickers"), t!("admin.max_stickers")),
        ];
        let field_width = (ui.available_width() / 2.0).min(192.0);
        for (index, ((read_label, edit_label), field)) in
            labels.into_iter().zip(&mut self.fields).enumerate()
        {
            let format = |value: i64| match index {
                0..=2 => files::size(value as u64),
                3 if value % 86_400 == 0 => t!("admin.retention_days", count = value / 86_400),
                3 => t!("admin.retention_secs", count = value),
                _ => value.to_string(),
            };
            let ceiling = format(values(ceilings)[index]);
            let help = t!("admin.host_ceiling", limit = ceiling);
            let label = if editable { edit_label } else { read_label };
            ui::setting_row(ui, label, &help, |ui| {
                if editable {
                    ui.add(ui::field(field).desired_width(field_width).char_limit(19))
                        .accessible_name(label);
                } else {
                    ui.label(format(values(limits)[index]));
                }
            });
        }

        if !editable {
            return None;
        }
        let proposed = self.parse();
        if proposed.is_none() {
            ui::error_text(ui, Some(t!("admin.invalid_file_limits")));
        }
        ui.add_enabled_ui(proposed.is_some_and(|value| value != limits), |ui| {
            ui::primary(ui, t!("common.save")).clicked()
        })
        .inner
        .then_some(proposed)
        .flatten()
    }

    fn parse(&self) -> Option<FileLimits> {
        let [
            max_file_bytes,
            member_storage_bytes,
            server_storage_bytes,
            retention_secs,
            max_stickers,
        ] = self
            .fields
            .each_ref()
            .map(|field| field.trim().parse::<i64>().ok().filter(|&value| value > 0));
        Some(FileLimits {
            max_file_bytes: max_file_bytes?,
            member_storage_bytes: member_storage_bytes?,
            server_storage_bytes: server_storage_bytes?,
            retention_secs: retention_secs?,
            max_stickers: max_stickers?,
        })
    }
}

fn values(limits: FileLimits) -> [i64; 5] {
    [
        limits.max_file_bytes,
        limits.member_storage_bytes,
        limits.server_storage_bytes,
        limits.retention_secs,
        limits.max_stickers,
    ]
}
