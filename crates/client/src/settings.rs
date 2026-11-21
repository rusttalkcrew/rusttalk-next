//! The Settings and Server settings windows: one large panel with grouped pages in a
//! sidebar on the left and the open page on the right, like the old client's settings.

use eframe::egui::{
    self, Align, Align2, Button, CollapsingHeader, Color32, CornerRadius, FontId, Frame,
    Id as UiId, Layout, Modal, Rect, Response, RichText, Role, ScrollArea, Sense, Stroke,
    StrokeKind, Ui, UiBuilder, Vec2, WidgetInfo,
};
use rusttalk_core::identity::fingerprint;

use crate::{
    app::{Action, error_text},
    call, camera,
    files::Ask,
    home::{Home, Tab, face, palette, privacy_tab},
    server::Directory,
};

// Look of the settings windows, kept together for the shared UI kit (#52).
/// The biggest the window grows; smaller windows leave `WINDOW_MARGIN` around it.
const MAX_SIZE: Vec2 = Vec2::new(1040.0, 720.0);
const MIN_SIZE: Vec2 = Vec2::new(300.0, 240.0);
const WINDOW_MARGIN: f32 = 24.0;
/// Below this width the window uses the narrow spacing.
const NARROW: f32 = 600.0;
const WINDOW_RADIUS: u8 = 10;
const SIDEBAR_SHARE: f32 = 0.28;
const SIDEBAR_MIN: f32 = 150.0;
const SIDEBAR_MAX: f32 = 232.0;
/// Sidebar padding: left and top, then right and bottom.
const SIDEBAR_PAD_START: Vec2 = Vec2::new(12.0, 18.0);
const SIDEBAR_PAD_END: Vec2 = Vec2::new(10.0, 10.0);
const CLOSE_COLUMN: f32 = 72.0;
const CLOSE_COLUMN_NARROW: f32 = 52.0;
const CLOSE_TOP: f32 = 24.0;
const CLOSE_SIZE: f32 = 36.0;
const CONTENT_GAP: f32 = 36.0;
const CONTENT_GAP_NARROW: f32 = 16.0;
const CONTENT_TOP: f32 = 28.0;
/// Widest the page content grows, for easy reading.
const CONTENT_MAX_WIDTH: f32 = 700.0;
const ROW_HEIGHT: f32 = 32.0;
const ROW_RADIUS: u8 = 6;
/// Indent of the row icon, the row label and the group captions.
const ROW_ICON_X: f32 = 10.0;
const ROW_LABEL_X: f32 = 36.0;
const CARD_RADIUS: u8 = 8;
const CARD_HEIGHT: f32 = 64.0;
const CARD_MAX_WIDTH: f32 = 240.0;
const CARD_MIN_WIDTH: f32 = 150.0;
const HEADING_SIZE: f32 = 16.0;
const TITLE_SIZE: f32 = 22.0;
const CAPTION_SIZE: f32 = 11.5;
const ROW_ICON_SIZE: f32 = 16.0;
const ROW_TEXT_SIZE: f32 = 14.5;
const CARD_ICON_SIZE: f32 = 22.0;
const CARD_TITLE_SIZE: f32 = 15.0;
const CARD_HELP_SIZE: f32 = 12.5;
/// Sidebar row fills, Discord-like, for dark and light mode.
const SELECTED_DARK: Color32 = Color32::from_rgb(0x40, 0x42, 0x49);
const SELECTED_LIGHT: Color32 = Color32::from_rgb(0xd7, 0xd9, 0xdc);
const HOVER_DARK: Color32 = Color32::from_rgb(0x35, 0x37, 0x3c);
const HOVER_LIGHT: Color32 = Color32::from_rgb(0xe3, 0xe5, 0xe8);
const REPOSITORY: &str = "https://github.com/rusttalkcrew/rusttalk-next";

pub struct Page<T> {
    pub id: T,
    pub icon: &'static str,
    pub label: &'static str,
}

pub struct Group<T> {
    pub title: &'static str,
    pub pages: Vec<Page<T>>,
}

/// Show a settings window with `groups` in the sidebar under `heading`, and the `open`
/// page drawn by `content` under its title. Returns what `content` returned and whether
/// the window should close (× clicked, Escape, or a click outside).
pub fn window<T: Copy + PartialEq, R>(
    ctx: &egui::Context,
    id: &str,
    heading: &str,
    groups: &[Group<T>],
    open: &mut T,
    content: impl FnOnce(&mut Ui) -> R,
) -> (R, bool) {
    let screen = ctx.content_rect().size();
    let size = (screen - Vec2::splat(2.0 * WINDOW_MARGIN))
        .min(MAX_SIZE)
        .max(MIN_SIZE);
    let narrow = size.x < NARROW;
    let mut close = false;
    let modal = Modal::new(UiId::unique(id))
        .frame(
            Frame::new()
                .corner_radius(WINDOW_RADIUS)
                .shadow(ctx.global_style().visuals.popup_shadow),
        )
        .show(ctx, |ui| {
            let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
            let colors = palette(ui);
            let side_width = (size.x * SIDEBAR_SHARE).clamp(SIDEBAR_MIN, SIDEBAR_MAX);
            let close_width = if narrow {
                CLOSE_COLUMN_NARROW
            } else {
                CLOSE_COLUMN
            };
            let side = Rect::from_min_max(
                rect.min,
                egui::pos2(rect.left() + side_width, rect.bottom()),
            );
            ui.painter().rect_filled(rect, WINDOW_RADIUS, colors.main);
            ui.painter().rect_filled(
                side,
                CornerRadius {
                    nw: WINDOW_RADIUS,
                    sw: WINDOW_RADIUS,
                    ne: 0,
                    se: 0,
                },
                colors.side,
            );

            let nav = Rect::from_min_max(side.min + SIDEBAR_PAD_START, side.max - SIDEBAR_PAD_END);
            sidebar(&mut child(ui, nav, Align::Min), id, heading, groups, open);

            let close_rect = Rect::from_min_size(
                egui::pos2(rect.right() - close_width, rect.top() + CLOSE_TOP),
                Vec2::new(close_width, 2.0 * CLOSE_SIZE),
            );
            close = close_button(&mut child(ui, close_rect, Align::Center));

            let gap = if narrow {
                CONTENT_GAP_NARROW
            } else {
                CONTENT_GAP
            };
            let body = Rect::from_min_max(
                egui::pos2(side.right() + gap, rect.top() + CONTENT_TOP),
                egui::pos2(
                    rect.right() - close_width,
                    rect.bottom() - SIDEBAR_PAD_END.y,
                ),
            );
            let title = (groups.iter().flat_map(|group| &group.pages))
                .find(|page| page.id == *open)
                .map_or("", |page| page.label);
            page_body(
                &mut child(ui, body, Align::Min),
                (id, title),
                title,
                colors.side,
                content,
            )
        });
    let close = close || modal.should_close();
    (modal.inner, close)
}

/// A top-down child UI filling `rect`.
fn child(ui: &mut Ui, rect: Rect, align: Align) -> Ui {
    ui.new_child(
        UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::top_down(align)),
    )
}

/// The page title, then the page in a scroll area of reading width.
fn page_body<R>(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    title: &str,
    fields: Color32,
    content: impl FnOnce(&mut Ui) -> R,
) -> R {
    ui.label(
        RichText::new(title)
            .size(TITLE_SIZE)
            .strong()
            .color(ui.visuals().strong_text_color()),
    );
    ui.add_space(10.0);
    ScrollArea::vertical()
        .id_salt(id)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_max_width((ui.available_width() - 12.0).clamp(0.0, CONTENT_MAX_WIDTH));
            // Text fields and cards stand out from the page.
            ui.visuals_mut().extreme_bg_color = fields;
            let inner = content(ui);
            ui.add_space(16.0);
            inner
        })
        .inner
}

fn sidebar<T: Copy + PartialEq>(
    ui: &mut Ui,
    id: &str,
    heading: &str,
    groups: &[Group<T>],
    open: &mut T,
) {
    ui.horizontal(|ui| {
        ui.add_space(ROW_ICON_X);
        let text = RichText::new(heading)
            .size(HEADING_SIZE)
            .strong()
            .color(ui.visuals().strong_text_color());
        ui.add(egui::Label::new(text).truncate());
    });
    ui.add_space(8.0);
    ScrollArea::vertical()
        .id_salt((id, "pages"))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for (index, group) in groups.iter().filter(|g| !g.pages.is_empty()).enumerate() {
                ui.add_space(if index == 0 { 4.0 } else { 14.0 });
                group_caption(ui, group.title);
                ui.add_space(2.0);
                for page in &group.pages {
                    if nav_row(ui, page.icon, page.label, *open == page.id).clicked() {
                        *open = page.id;
                    }
                }
            }
        });
}

/// A group header in the sidebar, like "USER SETTINGS".
fn group_caption(ui: &mut Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(ROW_ICON_X);
        let text = RichText::new(text.to_uppercase())
            .size(CAPTION_SIZE)
            .strong();
        ui.label(text.color(ui.visuals().weak_text_color()));
    });
}

/// One page in the sidebar: icon and label, highlighted when open.
fn nav_row(ui: &mut Ui, icon: &str, label: &str, selected: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width().max(0.0), ROW_HEIGHT),
        Sense::click(),
    );
    let dark = ui.visuals().dark_mode;
    let hovered = response.hovered();
    let fill = match (selected, hovered, dark) {
        (true, _, true) => SELECTED_DARK,
        (true, _, false) => SELECTED_LIGHT,
        (false, true, true) => HOVER_DARK,
        (false, true, false) => HOVER_LIGHT,
        (false, false, _) => Color32::TRANSPARENT,
    };
    let text = if selected || hovered {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    let painter = ui.painter();
    painter.rect_filled(rect, ROW_RADIUS, fill);
    if response.has_focus() {
        painter.rect_stroke(
            rect,
            ROW_RADIUS,
            ui.visuals().selection.stroke,
            StrokeKind::Inside,
        );
    }
    let left = rect.left_center();
    let font = |size| FontId::proportional(size);
    painter.text(
        left + Vec2::new(ROW_ICON_X, 0.0),
        Align2::LEFT_CENTER,
        icon,
        font(ROW_ICON_SIZE),
        text,
    );
    painter.text(
        left + Vec2::new(ROW_LABEL_X, 0.0),
        Align2::LEFT_CENTER,
        label,
        font(ROW_TEXT_SIZE),
        text,
    );
    response.widget_info(|| WidgetInfo::selected(Role::Tab, true, selected, label));
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The round × with "ESC" under it. True when clicked.
fn close_button(ui: &mut Ui) -> bool {
    let weak = ui.visuals().weak_text_color();
    let button = ui
        .add(
            Button::new(RichText::new("×").size(TITLE_SIZE).color(weak))
                .fill(Color32::TRANSPARENT)
                .stroke(Stroke::new(1.5, weak))
                .corner_radius(CLOSE_SIZE / 2.0)
                .min_size(Vec2::splat(CLOSE_SIZE)),
        )
        .on_hover_text("Close (Esc)")
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    button.widget_info(|| WidgetInfo::labeled(Role::Button, true, "Close"));
    ui.label(RichText::new("ESC").size(CAPTION_SIZE).strong().color(weak));
    button.clicked()
}

/// A pick-one card with an icon, a title and a line of help, like the input mode cards.
pub fn choice_card(ui: &mut Ui, selected: bool, icon: &str, title: &str, help: &str) -> Response {
    let width = ((ui.available_width() - 8.0) / 2.0).clamp(CARD_MIN_WIDTH, CARD_MAX_WIDTH);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, CARD_HEIGHT), Sense::click());
    let visuals = ui.visuals();
    let accent = visuals.selection.bg_fill;
    let fill = if response.hovered() || selected {
        visuals.widgets.hovered.weak_bg_fill
    } else {
        visuals.extreme_bg_color
    };
    let painter = ui.painter();
    painter.rect_filled(rect, CARD_RADIUS, fill);
    if selected {
        painter.rect_stroke(
            rect,
            CARD_RADIUS,
            Stroke::new(2.0, accent),
            StrokeKind::Inside,
        );
    } else if response.has_focus() {
        painter.rect_stroke(
            rect,
            CARD_RADIUS,
            visuals.selection.stroke,
            StrokeKind::Inside,
        );
    }
    let (strong, weak) = (visuals.strong_text_color(), visuals.weak_text_color());
    let icon_color = if selected { accent } else { weak };
    let font = |size| FontId::proportional(size);
    let mid = rect.center().y;
    painter.text(
        rect.left_center() + Vec2::new(24.0, 0.0),
        Align2::CENTER_CENTER,
        icon,
        font(CARD_ICON_SIZE),
        icon_color,
    );
    let x = rect.left() + 46.0;
    painter.text(
        egui::pos2(x, mid - 1.0),
        Align2::LEFT_BOTTOM,
        title,
        font(CARD_TITLE_SIZE),
        strong,
    );
    painter.text(
        egui::pos2(x, mid + 2.0),
        Align2::LEFT_TOP,
        help,
        font(CARD_HELP_SIZE),
        weak,
    );
    response.widget_info(|| WidgetInfo::selected(Role::RadioButton, true, selected, title));
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A rounded box that sets a group of settings apart.
fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .fill(ui.visuals().extreme_bg_color)
        .corner_radius(CARD_RADIUS)
        .inner_margin(16)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// The Settings window, and the sign-out question it can ask.
pub fn show(
    ctx: &egui::Context,
    home: &mut Home,
    updates: &mut crate::update::Updater,
) -> Option<Action> {
    let mut tab = home.settings?;
    let page = |id, icon, label| Page { id, icon, label };
    let groups = [
        Group {
            title: "User settings",
            pages: vec![
                page(Tab::Profile, "👤", "My account"),
                page(Tab::Privacy, "🔒", "Privacy"),
                page(Tab::Devices, "💻", "Devices"),
            ],
        },
        Group {
            title: "App settings",
            pages: vec![
                page(Tab::Voice, "🎤", "Voice & audio"),
                page(Tab::Video, "🎥", "Video"),
                page(
                    Tab::Updates,
                    "🔄",
                    if updates.available() {
                        "Updates (new)"
                    } else {
                        "Updates"
                    },
                ),
            ],
        },
        Group {
            title: "Info",
            pages: vec![page(Tab::About, "ℹ", "About RustTalk")],
        },
    ];
    let before = tab;
    let ((), close) = window(ctx, "settings", "Settings", &groups, &mut tab, |ui| {
        match before {
            Tab::Profile => account_page(ui, home),
            Tab::Voice => call::settings_tab(ui, &mut home.voice),
            Tab::Video => {
                let audio = home.voice.audio.clone();
                let mut s = audio.settings();
                camera::settings_tab(ui, &mut home.voice.camera, &mut s);
                audio.set_settings(s);
            }
            Tab::Privacy => privacy_tab(ui, home),
            Tab::Devices => {
                ui.label(format!("This device: {}", home.identity.certificate.name()));
                ui.add_space(12.0);
                card(ui, |ui| {
                    ui.label(RichText::new("Sign out of this device").strong());
                    ui.label("Removes your account from this computer. You need your 24 recovery words to sign in again.");
                    if ui.button("Sign out of this device").clicked() {
                        home.confirm_sign_out = true;
                    }
                });
            }
            Tab::Updates => updates.settings(ui),
            Tab::About => {
                ui.label(
                    RichText::new(format!("RustTalk {}", env!("CARGO_PKG_VERSION")))
                        .size(16.0)
                        .strong(),
                );
                ui.label("Free and open source, under the MIT license.");
                ui.hyperlink_to("Source code on GitHub", REPOSITORY);
            }
        }
        error_text(ui, home.error.as_deref());
    });
    home.settings = Some(tab);
    if close && !home.confirm_sign_out {
        home.settings = None;
        home.error = None;
        home.publish_settings();
    }

    if !home.confirm_sign_out {
        return None;
    }
    let mut action = None;
    let mut cancel = false;
    let modal = Modal::new(UiId::unique("sign-out")).show(ctx, |ui| {
        ui.set_width(380.0);
        ui.heading("Sign out of this device?");
        ui.label("This removes your account from this computer. To sign in again, you need your 24 recovery words.");
        ui.horizontal(|ui| {
            if ui.button("Sign out").clicked() {
                action = Some(Action::SignOut);
            }
            cancel = ui.button("Cancel").clicked();
        });
    });
    if cancel || modal.should_close() {
        home.confirm_sign_out = false;
    }
    action
}

fn account_page(ui: &mut Ui, home: &mut Home) {
    card(ui, |ui| {
        ui.horizontal(|ui| {
            let me = home.identity.certificate.person_id();
            // The picture is the same everywhere; show it from any server that has it.
            let none = Directory::default();
            let dir = (home.servers.iter())
                .find(|s| s.member(me).is_some_and(|m| m.avatar.is_some()))
                .map_or(&none, |s| &s.dir);
            face(ui, dir, me, "?", CARD_HEIGHT, false);
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.label("Your picture, on all your servers.");
                let change = ui
                    .add_enabled(!home.servers.is_empty(), Button::new("Change picture"))
                    .on_hover_text("PNG, JPG or GIF. GIFs play.");
                if change.clicked() {
                    home.error = None;
                    home.files.ask(ui.ctx(), Ask::Picture);
                }
            });
        });
    });
    ui.add_space(8.0);
    ui.label("You pick your name on each server. Change it from the server's menu.");
    CollapsingHeader::new("Details").show(ui, |ui| {
        ui.label("Your ID");
        ui.monospace(fingerprint(home.identity.certificate.person_id()));
        ui.weak("Friends can compare this to make sure it's really you.");
    });
}
