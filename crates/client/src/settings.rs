//! The Settings and Server settings windows: one large panel with a sidebar of grouped
//! pages on the left, the open page's sections under it, and the page on the right as
//! one long scrolling page.

use eframe::egui::{
    self, Align, CornerRadius, Frame, Id as UiId, Layout, Rect, RichText, ScrollArea, Sense, Ui,
    UiBuilder, Vec2, accesskit::Role,
};
use rusttalk_core::identity::fingerprint;

use crate::{
    app::Action,
    call, camera,
    files::Ask,
    home::{Home, Tab, face, privacy_tab},
    server::Directory,
    ui::{
        self, CONTROL_HEIGHT, DARK_THEMES, LIGHT_THEMES, Mode, RADIUS_WINDOW, SPACE_S, TEXT_BODY,
        TEXT_CAPTION, TEXT_HEADING, error_text,
    },
};

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
const HEADER_PICTURE: f32 = 40.0;
const HEADER_HEIGHT: f32 = 56.0;
/// Room under the last section, so it can scroll up past the bottom edge a little.
const CONTENT_BOTTOM: f32 = 48.0;
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

/// What the sidebar remembers between frames.
#[derive(Clone, Default)]
struct Nav {
    /// The page `sections` belong to.
    page: String,
    sections: Vec<String>,
    /// The section in view, highlighted in the sidebar.
    current: usize,
    search: String,
    /// The frame the window was last shown in, to start fresh when it opens again.
    frame: u64,
}

/// Show a settings window: `header` (true when clicked, to open the first page) and
/// `groups` in the sidebar, and the `open` page drawn by `content` as one long page.
/// Its `ui::page_section`s show under the open page in the sidebar, which scrolls to
/// them. Returns what `content` returned and whether the window should close (×,
/// Escape or a click outside).
pub fn window<T: Copy + PartialEq, S, R>(
    ctx: &egui::Context,
    id: &str,
    groups: &[Group<T>],
    open: &mut T,
    state: &mut S,
    header: impl FnOnce(&mut Ui, &mut S) -> bool,
    content: impl FnOnce(&mut Ui, &mut S) -> R,
) -> (R, bool) {
    let screen = ctx.content_rect().size();
    let size = (screen - Vec2::splat(2.0 * WINDOW_MARGIN))
        .min(MAX_SIZE)
        .max(MIN_SIZE);
    let narrow = size.x < NARROW;
    let nav_id = UiId::unique((id, "nav"));
    let mut nav: Nav = ctx.data(|data| data.get_temp(nav_id)).unwrap_or_default();
    let title = (groups.iter().flat_map(|group| &group.pages))
        .find(|page| page.id == *open)
        .map_or("", |page| page.label);
    let frame = ctx.cumulative_frame_nr();
    if nav.frame + 1 < frame {
        nav = Nav::default();
    }
    nav.frame = frame;
    if nav.page != title {
        nav = Nav {
            page: title.to_owned(),
            search: std::mem::take(&mut nav.search),
            frame,
            ..Nav::default()
        };
    }
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

            let nav_rect =
                Rect::from_min_max(side.min + SIDEBAR_PAD_START, side.max - SIDEBAR_PAD_END);
            let side_ui = &mut child(ui, nav_rect, Align::Min);
            if header(side_ui, state)
                && let Some(first) = groups.iter().flat_map(|group| &group.pages).next()
            {
                *open = first.id;
            }
            let jump = sidebar(side_ui, id, groups, open, &mut nav);

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
            page_body(
                &mut child(ui, body, Align::Min),
                (id, title),
                title,
                colors.side,
                jump,
                &mut nav,
                |ui| content(ui, state),
            )
        });
    ctx.data_mut(|data| data.insert_temp(nav_id, nav));
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

/// The page in a scroll area of reading width: its title, then its sections. Records
/// the sections in `nav` and which one is in view.
fn page_body<R>(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    title: &str,
    fields: egui::Color32,
    jump: Option<usize>,
    nav: &mut Nav,
    content: impl FnOnce(&mut Ui) -> R,
) -> R {
    let output = ScrollArea::vertical()
        .id_salt(id)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_max_width((ui.available_width() - 12.0).clamp(0.0, CONTENT_MAX_WIDTH));
            // Text fields and cards stand out from the page.
            ui.visuals_mut().extreme_bg_color = fields;
            ui.label(ui::heading(title).size(TEXT_HEADING));
            ui.add_space(SPACE_S);
            let (inner, sections) = ui::collect_sections(ui, jump, content);
            ui.add_space(CONTENT_BOTTOM);
            (inner, sections)
        });
    let (inner, sections) = output.inner;
    let room = output.content_size.y - output.inner_rect.height();
    nav.current = if room <= 0.0 {
        // Nothing scrolls: keep the section last picked.
        jump.unwrap_or(nav.current)
    } else {
        let at_end = output.state.clamped_offset().y >= room - 1.0;
        ui::current_section(&sections.tops, output.inner_rect.top(), at_end)
    };
    nav.sections = sections.titles;
    inner
}

/// The sidebar under the header: a search box, then the pages by group, with the
/// open page's sections under it. Returns the section to scroll to.
fn sidebar<T: Copy + PartialEq>(
    ui: &mut Ui,
    id: &str,
    groups: &[Group<T>],
    open: &mut T,
    nav: &mut Nav,
) -> Option<usize> {
    ui.add_space(SPACE_S);
    let search = ui.add(
        ui::field(&mut nav.search)
            .hint_text("Search")
            .desired_width(f32::INFINITY),
    );
    search.widget_info(|| egui::WidgetInfo::labeled(Role::SearchInput, true, "Search settings"));
    let query = nav.search.trim().to_lowercase();
    let mut jump = None;
    ui.add_space(SPACE_S);
    ScrollArea::vertical()
        .id_salt((id, "pages"))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            let shown = groups.iter().filter_map(|group| {
                let pages: Vec<_> = (group.pages.iter())
                    .filter(|page| page.label.to_lowercase().contains(&query))
                    .collect();
                (!pages.is_empty()).then_some((group.title, pages))
            });
            let mut any = false;
            for (index, (title, pages)) in shown.enumerate() {
                any = true;
                if index > 0 {
                    ui.add_space(SPACE_S);
                    let y = ui.cursor().top();
                    let x = ui.max_rect().x_range();
                    ui.painter()
                        .hline(x, y, egui::Stroke::new(1.0, ui::colors(ui).border));
                    ui.add_space(SPACE_S);
                }
                ui.add_space(4.0);
                group_caption(ui, title);
                ui.add_space(2.0);
                for page in pages {
                    let selected = *open == page.id;
                    let row =
                        ui::nav_row(ui, page.icon, page.label, selected, Role::Tab, page.label);
                    if row.clicked() {
                        *open = page.id;
                    }
                    // A page with more than one section lists them, to jump to.
                    if selected && nav.page == page.label && nav.sections.len() > 1 {
                        for (at, section) in nav.sections.iter().enumerate() {
                            if ui::nav_sub_row(ui, section, nav.current == at).clicked() {
                                jump = Some(at);
                            }
                        }
                    }
                }
            }
            if !any {
                ui.weak("Nothing found.");
            }
        });
    if jump.is_some() {
        ui.ctx().request_repaint();
    }
    jump
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

/// The top of a settings sidebar: a picture, a name and a line under it. Clickable.
pub fn sidebar_header(ui: &mut Ui, name: &str, line: &str, picture: impl FnOnce(&mut Ui)) -> bool {
    ui::row_button(ui, HEADER_HEIGHT, name, |ui| {
        picture(ui);
        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.add(egui::Label::new(ui::bold(name).size(TEXT_BODY + 1.0)).truncate());
            ui.add(egui::Label::new(ui::caption(line)).truncate());
        });
    })
    .clicked()
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
    let ((), close) = window(
        ctx,
        "settings",
        &groups,
        &mut tab,
        home,
        |ui, home| {
            let (dir, name) = me(home);
            let me = *home.identity.certificate.person_id();
            sidebar_header(ui, &name, "Edit profile", |ui| {
                face(ui, dir, &me, &name, HEADER_PICTURE, false);
            })
        },
        |ui, home| {
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
                Tab::Devices => devices_page(ui, home),
                Tab::Updates => updates.settings(ui),
                Tab::About => {
                    ui::page_section(ui, &format!("RustTalk {}", env!("CARGO_PKG_VERSION")));
                    ui.label("Free and open source, under the MIT license.");
                    ui.hyperlink_to("Source code on GitHub", REPOSITORY);
                }
            }
            error_text(ui, home.error.as_deref());
        },
    );
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

/// A server that knows this person's picture, and the name they use on their first
/// server. The picture is the same everywhere; names are per server.
fn me(home: &Home) -> (&Directory, String) {
    static NONE: std::sync::LazyLock<Directory> = std::sync::LazyLock::new(Directory::default);
    let me = home.identity.certificate.person_id();
    let dir = (home.servers.iter())
        .find(|s| s.member(me).is_some_and(|m| m.avatar.is_some()))
        .map_or(&*NONE, |s| &s.dir);
    let name = (home.servers.iter())
        .find_map(|s| s.member(me).and_then(|m| m.name.clone()))
        .unwrap_or_else(|| "You".into());
    (dir, name)
}

fn account_page(ui: &mut Ui, home: &mut Home) {
    ui::page_section(ui, "Profile");
    ui.horizontal(|ui| {
        let (dir, name) = me(home);
        let me = *home.identity.certificate.person_id();
        face(ui, dir, &me, &name, PICTURE_SIZE, false);
        ui.add_space(SPACE_S);
        ui.vertical(|ui| {
            ui::setting_row(
                ui,
                "Picture",
                "Shown on all your servers. GIFs play.",
                |ui| {
                    let change = ui
                        .add_enabled_ui(!home.servers.is_empty(), |ui| {
                            ui::secondary(ui, "Change picture")
                        })
                        .inner
                        .on_hover_text("PNG, JPG or GIF.");
                    if change.clicked() {
                        home.error = None;
                        home.files.ask(ui.ctx(), Ask::Picture);
                    }
                },
            );
        });
    });
    ui::setting_row(
        ui,
        "Name",
        "You pick your name on each server. Change it from the server's menu.",
        |_| {},
    );
    ui::page_section(ui, "Your ID");
    ui.weak("Friends can compare this to make sure it's really you.");
    ui::card(ui, |ui| {
        ui.monospace(fingerprint(home.identity.certificate.person_id()));
    });
}

fn devices_page(ui: &mut Ui, home: &mut Home) {
    ui::page_section(ui, "This device");
    ui::setting_row(
        ui,
        home.identity.certificate.name(),
        "This computer",
        |_| {},
    );
    ui::danger_zone(ui, |ui| {
        ui::setting_row(
            ui,
            "Sign out of this device",
            "Removes your account from this computer. You need your 24 recovery words to sign in again.",
            |ui| {
                if ui::danger(ui, "Sign out").clicked() {
                    home.confirm_sign_out = true;
                }
            },
        );
    });
}

/// Settings → Appearance: dark, light or the system's mode, a theme for each, and
/// the accent colour. Changes show at once.
fn appearance_page(ui: &mut Ui, home: &mut Home) {
    let audio = home.voice.audio.clone();
    let mut settings = audio.settings();
    let before = settings.appearance();
    let mut look = before;
    ui::page_section(ui, "Mode");
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
        ui::page_section(ui, caption);
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
    ui::page_section(ui, "Accent colour");
    ui.weak("Used for buttons, links and what's selected.");
    ui.horizontal_wrapped(|ui| {
        ui.set_min_height(CONTROL_HEIGHT);
        for accent in ui::ACCENTS {
            if ui::swatch(ui, accent, look.accent == accent).clicked() {
                look.accent = accent;
            }
        }
    });
    if look != before {
        settings.set_appearance(look);
        audio.set_settings(settings);
    }
}
