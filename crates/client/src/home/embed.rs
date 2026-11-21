//! A card under a message that previews something the message links to, such as a
//! server invite. It only shows what it is given: building one never contacts anything.

use eframe::egui::{Align, Align2, FontId, Layout, Sense, TextureHandle, Ui, Vec2};

use super::paint_hovered;
use crate::{server::Picture, ui};

const WIDTH: f32 = 420.0;
const IMAGE: f32 = 40.0;

pub enum Image<'a> {
    Picture(&'a Picture),
    /// A letter or symbol in a circle, when there is no picture.
    Glyph(String),
    /// A banner across the top, such as a game's header picture. Owned by its
    /// preview, so it is freed with it.
    Wide(&'a TextureHandle),
}

pub struct Button<'a> {
    pub label: &'a str,
    /// The main action (accent), or a quieter one.
    pub primary: bool,
}

pub struct Embed<'a> {
    pub image: Option<Image<'a>>,
    /// A small line above the title saying what kind of thing this is.
    pub kind: &'a str,
    pub title: &'a str,
    pub description: Option<&'a str>,
    /// Short labelled facts under the description.
    pub fields: &'a [(&'a str, &'a str)],
    pub buttons: &'a [Button<'a>],
}

impl Embed<'_> {
    /// Returns which button was clicked.
    pub fn show(self, ui: &mut Ui) -> Option<usize> {
        ui.add_space(ui::SPACE_XS);
        ui.scope(|ui| {
            ui.set_max_width(WIDTH.min(ui.available_width()));
            // A big card, or one with a choice, puts its buttons under everything else.
            let stacked = matches!(self.image, Some(Image::Wide(_)))
                || !self.fields.is_empty()
                || self.buttons.len() > 1;
            ui::card(ui, |ui| {
                if let Some(Image::Wide(texture)) = &self.image {
                    let [w, h] = texture.size();
                    let width = ui.available_width();
                    let size = Vec2::new(width, width * h as f32 / w.max(1) as f32);
                    ui.add(
                        eframe::egui::Image::new(*texture)
                            .fit_to_exact_size(size)
                            .corner_radius(ui::RADIUS),
                    );
                }
                let clicked = ui
                    .horizontal(|ui| {
                        let icon = match &self.image {
                            Some(Image::Picture(picture)) => Some(Ok(*picture)),
                            Some(Image::Glyph(glyph)) => Some(Err(glyph)),
                            _ => None,
                        };
                        if let Some(icon) = icon {
                            let (rect, _) =
                                ui.allocate_exact_size(Vec2::splat(IMAGE), Sense::hover());
                            match icon {
                                Ok(picture) => paint_hovered(ui, rect, picture, IMAGE / 2.0),
                                Err(glyph) => {
                                    let c = ui::colors(ui);
                                    ui.painter()
                                        .circle_filled(rect.center(), IMAGE / 2.0, c.main);
                                    ui.painter().text(
                                        rect.center(),
                                        Align2::CENTER_CENTER,
                                        glyph,
                                        FontId::new(16.0, ui::bold_family()),
                                        c.text,
                                    );
                                }
                            }
                        }
                        ui.vertical(|ui| {
                            ui.label(ui::caption(self.kind));
                            ui.label(ui::bold(self.title));
                            if let Some(description) = self.description {
                                ui.weak(description);
                            }
                        });
                        if stacked {
                            return None;
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            buttons(ui, self.buttons)
                        })
                        .inner
                    })
                    .inner;
                if !self.fields.is_empty() {
                    eframe::egui::Grid::new(ui.next_auto_id())
                        .num_columns(2)
                        .spacing(Vec2::new(ui::SPACE_M, ui::SPACE_XS))
                        .show(ui, |ui| {
                            for (label, value) in self.fields {
                                ui.label(ui::caption(*label));
                                ui.add(eframe::egui::Label::new(*value).wrap());
                                ui.end_row();
                            }
                        });
                }
                if stacked {
                    return ui
                        .horizontal(|ui| buttons(ui, self.buttons))
                        .inner
                        .or(clicked);
                }
                clicked
            })
        })
        .inner
    }
}

/// In right-to-left layouts the first button ends up rightmost, as the main one should.
fn buttons(ui: &mut Ui, buttons: &[Button]) -> Option<usize> {
    let mut clicked = None;
    for (index, button) in buttons.iter().enumerate() {
        let response = if button.primary {
            ui::primary(ui, button.label)
        } else {
            ui::secondary(ui, button.label)
        };
        if response.clicked() {
            clicked = Some(index);
        }
    }
    clicked
}
