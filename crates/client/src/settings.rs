//! The Settings and Server settings windows: one large panel with grouped pages in a
//! sidebar on the left and the open page on the right, like the old client's settings.

use eframe::egui::{
    self, Align, CollapsingHeader, CornerRadius, Frame, Id as UiId, Layout, Rect, RichText,
    ScrollArea, Sense, Ui, UiBuilder, Vec2, accesskit::Role,
};
use rusttalk_core::identity::fingerprint;

use crate::{
    app::Action,
    call, camera,
    files::Ask,
    home::{Home, Tab, face, privacy_tab},
    server::Directory,
    ui::{
        self, CONTROL_HEIGHT, DARK_THEMES, LIGHT_THEMES, Mode, RADIUS_WINDOW, SPACE_S,
        TEXT_CAPTION, TEXT_HEADING, TEXT_TITLE, error_text, section,
    },
};

// The layout of the settings windows; #56 redesigns it on top of the kit.
/// The biggest the window grows; smaller windows leave `WINDOW_MARGIN` around it.
const MAX_SIZE: Vec2 = Vec2::new(1040.0, 720.0);
const MIN_SIZE: Vec2 = Vec2::new(300.0, 240.0);
const WINDOW_MARGIN: f32 = 24.0;
/// Below this width the window uses the narrow spacing.
const NARROW: f32 = 600.0;
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
/// Indent of the group captions, in line with the row icons.
const CAPTION_X: f32 = 10.0;
const PICTURE_SIZE: f32 = 64.0;
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
    let modal = egui::Modal::new(UiId::unique(id))
        .frame(
            Frame::new()
                .corner_radius(RADIUS_WINDOW)
                .shadow(ctx.global_style().visuals.window_shadow),
        )
        .show(ctx, |ui| {
            let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
            let colors = ui::colors(ui);
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
            ui.painter().rect_filled(rect, RADIUS_WINDOW, colors.main);
            ui.painter().rect_filled(
                side,
                CornerRadius {
                    nw: RADIUS_WINDOW,
                    sw: RADIUS_WINDOW,
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
    fields: egui::Color32,
    content: impl FnOnce(&mut Ui) -> R,
) -> R {
    ui.label(ui::heading(title).size(TEXT_HEADING));
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
        ui.add_space(CAPTION_X);
        let text = ui::bold(heading).size(TEXT_TITLE - 1.0);
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
                    let selected = *open == page.id;
                    let row =
                        ui::nav_row(ui, page.icon, page.label, selected, Role::Tab, page.label);
                    if row.clicked() {
                        *open = page.id;
                    }
                }
            }
        });
}

/// A group header in the sidebar, like "USER SETTINGS".
fn group_caption(ui: &mut Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(CAPTION_X);
        ui::caption_label(ui, text);
    });
}

/// The round × with "ESC" under it. True when clicked.
fn close_button(ui: &mut Ui) -> bool {
    let clicked = ui::close_button(ui, CLOSE_SIZE).clicked();
    let text = RichText::new("ESC")
        .size(TEXT_CAPTION)
        .family(ui::bold_family())
        .color(ui::colors(ui).muted);
    ui.label(text);
    clicked
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
                page(Tab::Appearance, "🎨", "Appearance"),
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
            Tab::Appearance => appearance_page(ui, home),
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
                ui::card(ui, |ui| {
                    ui.label(ui::bold("Sign out of this device"));
                    ui.label("Removes your account from this computer. You need your 24 recovery words to sign in again.");
                    if ui::danger(ui, "Sign out of this device").clicked() {
                        home.confirm_sign_out = true;
                    }
                });
            }
            Tab::Updates => updates.settings(ui),
            Tab::About => {
                ui.label(ui::title(format!("RustTalk {}", env!("CARGO_PKG_VERSION"))));
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
    let ((), dismissed) = ui::dialog(ctx, "sign-out", "Sign out of this device?", |ui| {
        ui.label("This removes your account from this computer. To sign in again, you need your 24 recovery words.");
        ui::footer(ui, |ui| {
            if ui::danger(ui, "Sign out").clicked() {
                action = Some(Action::SignOut);
            }
            cancel = ui::secondary(ui, "Cancel").clicked();
        });
    });
    if cancel || dismissed {
        home.confirm_sign_out = false;
    }
    action
}

fn account_page(ui: &mut Ui, home: &mut Home) {
    ui::card(ui, |ui| {
        ui.horizontal(|ui| {
            let me = home.identity.certificate.person_id();
            // The picture is the same everywhere; show it from any server that has it.
            let none = Directory::default();
            let dir = (home.servers.iter())
                .find(|s| s.member(me).is_some_and(|m| m.avatar.is_some()))
                .map_or(&none, |s| &s.dir);
            face(ui, dir, me, "?", PICTURE_SIZE, false);
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.label("Your picture, on all your servers.");
                let change = ui
                    .add_enabled_ui(!home.servers.is_empty(), |ui| {
                        ui::secondary(ui, "Change picture")
                    })
                    .inner
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

/// Settings → Appearance: dark, light or the system's mode, a theme for each, and
/// the accent colour. Changes show at once.
fn appearance_page(ui: &mut Ui, home: &mut Home) {
    let audio = home.voice.audio.clone();
    let mut settings = audio.settings();
    let before = settings.appearance();
    let mut look = before;
    section(ui, "Mode");
    let modes = [
        (Mode::Dark, "🌙", "Dark", "Always dark"),
        (Mode::Light, "☀", "Light", "Always light"),
        (Mode::System, "🖥", "System", "Like your computer"),
    ];
    let width = ((ui.available_width() - 2.0 * SPACE_S) / 3.0).clamp(140.0, 200.0);
    ui.horizontal_wrapped(|ui| {
        for (mode, icon, title, help) in modes {
            let selected = look.mode == mode;
            if ui::choice_card_sized(ui, width, selected, icon, title, help).clicked() {
                look.mode = mode;
            }
        }
    });
    for (caption, themes, dark) in [
        ("Dark theme", DARK_THEMES, true),
        ("Light theme", LIGHT_THEMES, false),
    ] {
        section(ui, caption);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            for theme in themes {
                let selected = look.palette(dark) == theme;
                if ui::theme_card(ui, theme, look.accent, selected).clicked() {
                    if dark {
                        look.dark = theme;
                    } else {
                        look.light = theme;
                    }
                }
            }
        });
    }
    section(ui, "Accent colour");
    ui.horizontal_wrapped(|ui| {
        ui.set_min_height(CONTROL_HEIGHT);
        for accent in ui::ACCENTS {
            if ui::swatch(ui, accent, look.accent == accent).clicked() {
                look.accent = accent;
            }
        }
    });
    ui.weak("Used for buttons, links and what's selected.");
    if look != before {
        settings.set_appearance(look);
        audio.set_settings(settings);
    }
}
