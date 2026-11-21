//! A card under a message that previews something the message links to, such as a
//! server invite. It only shows what it is given: building one never contacts anything.

use eframe::egui::{Align, Align2, FontId, Layout, Sense, Ui, Vec2};

use super::paint_hovered;
use crate::{server::Picture, ui};

const WIDTH: f32 = 420.0;
const IMAGE: f32 = 40.0;

pub enum Image<'a> {
    Picture(&'a Picture),
    /// A letter or symbol in a circle, when there is no picture.
    Glyph(String),
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
    pub button: Option<Button<'a>>,
}

impl Embed<'_> {
    /// Returns whether the button was clicked.
    pub fn show(self, ui: &mut Ui) -> bool {
        ui.add_space(ui::SPACE_XS);
        ui.scope(|ui| {
            ui.set_max_width(WIDTH.min(ui.available_width()));
            ui::card(ui, |ui| {
                ui.horizontal(|ui| {
                    if let Some(image) = &self.image {
                        let (rect, _) = ui.allocate_exact_size(Vec2::splat(IMAGE), Sense::hover());
                        match image {
                            Image::Picture(picture) => {
                                paint_hovered(ui, rect, picture, IMAGE / 2.0)
                            }
                            Image::Glyph(glyph) => {
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
                    let Some(button) = self.button else {
                        return false;
                    };
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if button.primary {
                            ui::primary(ui, button.label)
                        } else {
                            ui::secondary(ui, button.label)
                        }
                        .clicked()
                    })
                    .inner
                })
                .inner
            })
        })
        .inner
    }
}
