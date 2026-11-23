//! The first-start screens' layout, the same on every window size: the logo, a
//! step title and one line of text, then the step, in a centred column with a
//! readable width. Wide windows show it as a card. It sits in the middle when it
//! fits and scrolls when it doesn't (a phone's keyboard takes room), so the
//! focused field can always be scrolled into view.

use std::{fmt::Debug, hash::Hash};

use eframe::egui::{
    Align, Button, CentralPanel, ColorImage, CursorIcon, Frame, Id, Image, Layout, Margin,
    Response, RichText, ScrollArea, Stroke, TextureFilter, TextureHandle, TextureOptions, Ui, Vec2,
    WidgetInfo, accesskit::Role,
};

use super::{
    CONTROL_HEIGHT, RADIUS_WINDOW, SPACE_L, SPACE_M, SPACE_XL, SPACE_XS, colors, heading, is_phone,
};

/// The widest the content gets: short lines read easily.
const WIDTH: f32 = 500.0;
/// Inside the card on wide windows.
const CARD_PADDING: i8 = 28;
const LOGO: f32 = 64.0;

/// Draw a first-start step, `id` telling the steps apart (each keeps its own scroll
/// position). `back` labels a Back button over the logo, for every step after the
/// first. Returns whether Back was pressed, and what `add` returned. Buttons and
/// fields `add` puts in the column fill its width.
pub fn onboarding<R>(
    ui: &mut Ui,
    id: impl Hash + Debug,
    back: Option<&str>,
    add: impl FnOnce(&mut Ui) -> R,
) -> (bool, R) {
    let phone = is_phone(ui.ctx());
    let c = colors(ui);
    let id = Id::unique(("onboarding", id));
    let panel = Frame::new().fill(if phone { c.main } else { c.side });
    CentralPanel::default()
        .frame(panel)
        .show(ui, |ui| {
            let view = ui.available_height();
            let gutter = if phone { SPACE_L } else { SPACE_XL };
            ScrollArea::vertical()
                .id_salt(id)
                .auto_shrink(false)
                .show(ui, |ui| {
                    // Centred by last frame's height; a change draws the frame again.
                    let height_id = id.with("height");
                    let last: f32 = ui.data(|data| data.get_temp(height_id)).unwrap_or(0.0);
                    ui.add_space(((view - last) / 2.0).max(gutter));
                    let padding = if phone { 0 } else { CARD_PADDING };
                    let width =
                        (WIDTH + 2.0 * f32::from(padding)).min(ui.available_width() - 2.0 * gutter);
                    let card = if phone {
                        Frame::new()
                    } else {
                        Frame::new()
                            .fill(c.main)
                            .stroke(Stroke::new(1.0, c.border))
                            .corner_radius(RADIUS_WINDOW)
                            .shadow(ui.visuals().popup_shadow)
                    }
                    .inner_margin(Margin::same(padding));
                    let shown = ui.horizontal(|ui| {
                        ui.add_space((ui.available_width() - width) / 2.0);
                        ui.vertical(|ui| {
                            ui.set_width(width);
                            card.show(ui, |ui| {
                                let inner = width - 2.0 * f32::from(padding);
                                ui.set_width(inner);
                                column(ui, inner, back, add)
                            })
                            .inner
                        })
                        .inner
                    });
                    let height = shown.response.rect.height();
                    if (height - last).abs() > 0.5 {
                        ui.data_mut(|data| data.insert_temp(height_id, height));
                        ui.ctx().request_discard("first-start step changed height");
                    }
                    ui.add_space(gutter);
                    shown.inner
                })
                .inner
        })
        .inner
}

fn column<R>(
    ui: &mut Ui,
    width: f32,
    back: Option<&str>,
    add: impl FnOnce(&mut Ui) -> R,
) -> (bool, R) {
    let pressed = back.is_some_and(|label| {
        ui.allocate_ui_with_layout(
            Vec2::new(width, CONTROL_HEIGHT),
            Layout::left_to_right(Align::Center),
            |ui| back_button(ui, label).clicked(),
        )
        .inner
    });
    ui.vertical_centered(|ui| logo(ui, LOGO));
    ui.add_space(SPACE_M);
    let inner = ui
        .with_layout(Layout::top_down_justified(Align::Min), add)
        .inner;
    (pressed, inner)
}

/// The step's title and its one line of explanation, centred under the logo.
pub fn step_title(ui: &mut Ui, title: &str, text: &str) {
    ui.vertical_centered(|ui| {
        ui.label(heading(title));
        ui.add_space(SPACE_XS);
        ui.label(RichText::new(text).color(colors(ui).muted));
    });
    ui.add_space(SPACE_XL);
}

/// "← Back", without a frame until hovered.
pub fn back_button(ui: &mut Ui, label: &str) -> Response {
    let response = ui
        .add(
            Button::new(RichText::new(format!("←  {label}")).color(colors(ui).muted))
                .frame_when_inactive(false)
                .min_size(Vec2::new(0.0, CONTROL_HEIGHT)),
        )
        .on_hover_cursor(CursorIcon::PointingHand);
    response.widget_info(|| WidgetInfo::labeled(Role::Button, ui.is_enabled(), label));
    response
}

/// The RustTalk logo, `size` points square.
pub fn logo(ui: &mut Ui, size: f32) -> Response {
    let id = Id::unique("rusttalk logo");
    let texture = match ui.data(|data| data.get_temp::<TextureHandle>(id)) {
        Some(texture) => texture,
        None => {
            let texture = ui.ctx().load_texture("logo", logo_image(), {
                let mut options = TextureOptions::LINEAR;
                // Drawn much smaller than the file: mipmaps keep the edges smooth.
                options.mipmap_mode = Some(TextureFilter::Linear);
                options
            });
            ui.data_mut(|data| data.insert_temp(id, texture.clone()));
            texture
        }
    };
    ui.add(Image::new((texture.id(), Vec2::splat(size))).alt_text("RustTalk"))
}

fn logo_image() -> ColorImage {
    let bytes = include_bytes!("../../assets/rusttalk.png");
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .expect("the bundled logo is a PNG")
        .to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    ColorImage::from_rgba_unmultiplied(size, image.as_raw())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_logo_decodes() {
        assert_eq!(logo_image().size, [256, 256]);
    }
}
