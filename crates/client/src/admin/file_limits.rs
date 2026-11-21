use eframe::egui::{ComboBox, Ui};
use rusttalk_core::auth::{FileLimits, FileSettings};

use crate::{files, i18n, ui};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Unit {
    Mb,
    Gb,
    Days,
    Stickers,
}

impl Unit {
    fn scale(self) -> i128 {
        match self {
            Self::Mb => 1_000_000,
            Self::Gb => 1_000_000_000,
            Self::Days => 86_400,
            Self::Stickers => 1,
        }
    }

    fn for_size(bytes: i64) -> Self {
        if i128::from(bytes) >= Self::Gb.scale() {
            Self::Gb
        } else {
            Self::Mb
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Mb => t!("admin.limit_mb"),
            Self::Gb => t!("admin.limit_gb"),
            Self::Days => t!("admin.limit_days"),
            Self::Stickers => "",
        }
    }

    fn number(self, value: i64, exact_size: bool) -> String {
        let value = i128::from(value);
        let scale = self.scale();
        let mut decimals = if exact_size { scale } else { 100 };
        while value > 0 && value * decimals < scale {
            decimals *= 10;
        }
        let displayed = value * decimals / scale;
        let whole = displayed / decimals;
        let fraction = displayed % decimals;
        if fraction == 0 {
            return whole.to_string();
        }
        let point = match i18n::current() {
            i18n::Lang::De | i18n::Lang::Nl => ',',
            _ => '.',
        };
        let width = decimals.ilog10() as usize;
        let digits = format!("{fraction:0width$}");
        format!("{whole}{point}{}", digits.trim_end_matches('0'))
    }

    fn display(self, value: i64) -> String {
        match self {
            Self::Stickers => value.to_string(),
            Self::Days if i128::from(value) % self.scale() == 0 => {
                t!(
                    "admin.retention_days",
                    count = i128::from(value) / self.scale()
                )
            }
            _ => t!(
                "admin.limit_amount",
                amount = self.number(value, false),
                unit = self.label()
            ),
        }
    }
}

struct Field {
    text: String,
    unit: Unit,
}

#[derive(Debug, PartialEq, Eq)]
enum Invalid {
    Number,
    Ceiling,
}

impl Field {
    fn new(value: i64, unit: Unit) -> Self {
        Self {
            text: unit.number(value, false),
            unit,
        }
    }

    fn proposed(&self, saved: i64, ceiling: i64) -> Result<i64, Invalid> {
        let text = self.text.trim();
        // The compact display can omit precision from a previously saved value.
        if text == self.unit.number(saved, false) {
            return Ok(saved);
        }
        if self.unit == Unit::Stickers {
            let value = text
                .parse::<i128>()
                .ok()
                .filter(|&n| n > 0)
                .ok_or(Invalid::Number)?;
            return if value <= i128::from(ceiling) {
                Ok(value as i64)
            } else {
                Err(Invalid::Ceiling)
            };
        }

        let (whole, fraction) = text.split_once(['.', ',']).unwrap_or((text, ""));
        let fraction = fraction.trim_end_matches('0');
        if fraction.len() > 12
            || !whole
                .bytes()
                .chain(fraction.bytes())
                .all(|c| c.is_ascii_digit())
        {
            return Err(Invalid::Number);
        }
        let divisor = 10_i128.pow(fraction.len() as u32);
        let whole = if whole.is_empty() {
            0
        } else {
            whole.parse::<i128>().map_err(|_| Invalid::Ceiling)?
        };
        let fraction = fraction.parse::<i128>().unwrap_or_default();
        let scale = self.unit.scale();
        if whole > i128::from(ceiling) / scale {
            return Err(Invalid::Ceiling);
        }
        let scaled = (whole * divisor + fraction) * scale;
        if scaled > i128::from(ceiling) * divisor {
            return Err(Invalid::Ceiling);
        }
        let rounded = (scaled + divisor / 2) / divisor;
        if rounded == 0 {
            return Err(Invalid::Number);
        }
        Ok(rounded as i64)
    }

    fn unit_picker(&mut self, ui: &mut Ui, label: &str, saved: i64) {
        let mut unit = self.unit;
        ComboBox::from_id_salt(label)
            .width(64.0)
            .selected_text(unit.label())
            .show_ui(ui, |ui| {
                for choice in [Unit::Mb, Unit::Gb] {
                    ui.selectable_value(&mut unit, choice, choice.label());
                }
            })
            .response
            .accessible_name(t!("admin.limit_unit", label = label, unit = unit.label()));
        if unit != self.unit {
            if let Ok(value) = self.proposed(saved, i64::MAX) {
                let exact_size = value != saved || unit != Unit::for_size(saved);
                self.text = unit.number(value, exact_size);
            }
            self.unit = unit;
        }
    }
}

pub(super) struct Form {
    settings: FileSettings,
    fields: [Field; 5],
}

impl Form {
    pub(super) fn new(settings: FileSettings) -> Self {
        let [file, member, server, retention, stickers] = values(settings.limits);
        Self {
            fields: [
                Field::new(file, Unit::for_size(file)),
                Field::new(member, Unit::for_size(member)),
                Field::new(server, Unit::for_size(server)),
                Field::new(retention, Unit::Days),
                Field::new(stickers, Unit::Stickers),
            ],
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
                total = self.fields[2].unit.display(limits.server_storage_bytes)
            ));
        });
        let labels = [
            t!("admin.max_file_size"),
            t!("admin.member_storage"),
            t!("admin.server_storage"),
            t!("admin.file_retention"),
            t!("admin.max_stickers"),
        ];
        let width = (ui.available_width() * 0.53).min(196.0);
        let saved = values(limits);
        let ceilings = values(ceilings);
        let mut proposed = [None; 5];
        for (index, (label, field)) in labels.into_iter().zip(&mut self.fields).enumerate() {
            let ceiling = ceilings[index];
            let value = saved[index];
            let help = t!("admin.host_ceiling", limit = field.unit.display(ceiling));
            ui::setting_row(ui, label, &help, |ui| {
                if !editable {
                    ui.label(field.unit.display(value));
                    return;
                }
                ui.vertical(|ui| {
                    ui.set_width(width);
                    let input_width = if field.unit == Unit::Stickers {
                        width
                    } else {
                        width * 0.57
                    };
                    ui.horizontal(|ui| {
                        ui.add(
                            ui::field(&mut field.text)
                                .desired_width(input_width)
                                .char_limit(24),
                        )
                        .accessible_name(label);
                        match field.unit {
                            Unit::Mb | Unit::Gb => field.unit_picker(ui, label, value),
                            Unit::Days => {
                                ui.label(field.unit.label());
                            }
                            Unit::Stickers => {}
                        }
                    });
                    match field.proposed(value, ceiling) {
                        Ok(value) => proposed[index] = Some(value),
                        Err(error) => ui::error_text(
                            ui,
                            Some(match error {
                                Invalid::Ceiling => t!("admin.limit_above_ceiling"),
                                Invalid::Number if field.unit == Unit::Stickers => {
                                    t!("admin.limit_positive_whole")
                                }
                                Invalid::Number => t!("admin.limit_positive"),
                            }),
                        ),
                    }
                });
            });
        }

        if !editable {
            return None;
        }
        let next = match proposed {
            [
                Some(max_file_bytes),
                Some(member_storage_bytes),
                Some(server_storage_bytes),
                Some(retention_secs),
                Some(max_stickers),
            ] => Some(FileLimits {
                max_file_bytes,
                member_storage_bytes,
                server_storage_bytes,
                retention_secs,
                max_stickers,
            }),
            _ => None,
        };
        ui.add_enabled_ui(next.is_some_and(|value| value != limits), |ui| {
            ui::primary(ui, t!("common.save")).clicked()
        })
        .inner
        .then_some(next)
        .flatten()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_keep_saved_precision_and_validate_decimal_units() {
        let mut field = Field::new(16_777_216, Unit::Mb);
        assert_eq!(field.proposed(16_777_216, 33_554_432), Ok(16_777_216));
        field.text = "16,75".into();
        assert_eq!(field.proposed(16_777_216, 33_554_432), Ok(16_750_000));
        field.text = "33.554433".into();
        assert_eq!(
            field.proposed(16_777_216, 33_554_432),
            Err(Invalid::Ceiling)
        );
        field.text = "0".into();
        assert_eq!(field.proposed(16_777_216, 33_554_432), Err(Invalid::Number));

        let field = Field {
            text: "1.5".into(),
            unit: Unit::Days,
        };
        assert_eq!(field.proposed(86_400, 172_800), Ok(129_600));
        assert_eq!(field.proposed(86_400, 129_599), Err(Invalid::Ceiling));
    }
}
