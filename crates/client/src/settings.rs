//! The Settings and Server settings windows: one large panel with a sidebar of grouped
//! pages on the left, the open page's sections under it, and the page on the right as
//! one long scrolling page.

use eframe::egui::{
    self, Align, ComboBox, CornerRadius, Frame, Id as UiId, Key, Layout, Rect, RichText,
    ScrollArea, Sense, Ui, UiBuilder, Vec2, accesskit::Role, scroll_area::ScrollBarVisibility,
};
use rusttalk_core::identity::fingerprint;

use crate::{
    app::Action,
    call, camera,
    files::Ask,
    home::{Home, Tab, face, privacy_tab},
    i18n::{LANGS, Lang},
    net::Command,
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
/// Where a phone gets a newer app: it has no in-app updater.
pub const DOWNLOAD_PAGE: &str = "https://github.com/rusttalkcrew/rusttalk-next/releases/latest";
/// Phones update through their download page or app store, never in the app, and
/// have no keyboard shortcuts to set.
pub const PHONE: bool = cfg!(any(target_os = "android", target_os = "ios"));

/// Whether Settings lists this page: phones have no Updates or Shortcuts page.
fn listed(tab: Tab, phone: bool) -> bool {
    !(phone && matches!(tab, Tab::Updates | Tab::Shortcuts))
}

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
    /// Phones: the open page shows instead of the page list.
    phone_page: bool,
    /// A text field had focus at the end of the last frame.
    typing: bool,
    /// Closing waits for "Discard changes?" to be answered.
    discarding: bool,
}

/// Show a settings window named `name`: `header` (true when clicked, to open the
/// first page) and `groups` in the sidebar, and the `open` page drawn by `content` as
/// one long page. Its `ui::page_section`s show under the open page in the sidebar,
/// which scrolls to them. Phones show it full screen: the page list, then the page
/// with Back, like a phone's own settings. Returns what `content` returned (the
/// default while only the list shows) and whether the window should close (×,
/// Escape or a click outside). While `unsaved` says changes wait for Save, closing
/// asks "Discard changes?" first.
#[allow(clippy::too_many_arguments)]
pub fn window<T: Copy + PartialEq, S, R: Default>(
    ctx: &egui::Context,
    id: &str,
    name: &str,
    groups: &[Group<T>],
    open: &mut T,
    state: &mut S,
    header: impl FnOnce(&mut Ui, &mut S) -> bool,
    content: impl FnOnce(&mut Ui, &mut S) -> R,
    unsaved: impl FnOnce(&S) -> bool,
) -> (R, bool) {
    let screen = ctx.content_rect().size();
    let size = (screen - Vec2::splat(2.0 * WINDOW_MARGIN))
        .min(MAX_SIZE)
        .max(MIN_SIZE);
    let nav_id = UiId::unique((id, "nav"));
    let mut nav: Nav = ctx.data(|data| data.get_temp(nav_id)).unwrap_or_default();
    let title = (groups.iter().flat_map(|group| &group.pages))
        .find(|page| page.id == *open)
        .map_or("", |page| page.label);
    let frame = ctx.cumulative_frame_nr();
    if nav.frame + 1 < frame {
        let first = groups.iter().flat_map(|group| &group.pages).next();
        nav = Nav {
            // Phones start on the page list, unless a particular page was asked for.
            phone_page: first.is_some_and(|page| page.id != *open),
            ..Nav::default()
        };
    }
    nav.frame = frame;
    if nav.page != title {
        nav = Nav {
            page: title.to_owned(),
            search: std::mem::take(&mut nav.search),
            frame,
            phone_page: nav.phone_page,
            ..Nav::default()
        };
    }
    // Escape in a text field only leaves the field: egui already took its focus away
    // this frame, so the window must not also close.
    if nav.typing {
        ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Escape));
    }
    let (inner, close) = if ui::is_phone(ctx) {
        phone(
            ctx, id, name, title, groups, open, &mut nav, state, header, content,
        )
    } else {
        desktop(
            ctx, id, size, title, groups, open, &mut nav, state, header, content,
        )
    };
    nav.typing = ctx.text_edit_focused();
    let close = close_or_ask(ctx, id, close && unsaved(state), close, &mut nav);
    ctx.data_mut(|data| data.insert_temp(nav_id, nav));
    (inner, close)
}

/// Whether to close now: at once when nothing is `unsaved`, else after "Discard
/// changes?" (which stays up across frames) is answered with Discard.
fn close_or_ask(ctx: &egui::Context, id: &str, unsaved: bool, close: bool, nav: &mut Nav) -> bool {
    if close && !unsaved {
        return true;
    }
    nav.discarding |= close;
    if !nav.discarding {
        return false;
    }
    let mut discard = false;
    let mut keep = false;
    let ((), dismissed) = ui::dialog(
        ctx,
        &format!("{id}-discard"),
        t!("settings.discard_title"),
        |ui| {
            ui.label(t!("settings.discard_text"));
            ui::footer(ui, |ui| {
                discard = ui::danger(ui, t!("settings.discard")).clicked();
                keep = ui::secondary(ui, t!("settings.keep_editing")).clicked();
            });
        },
    );
    if discard || keep || dismissed {
        nav.discarding = false;
    }
    discard && !keep && !dismissed
}

/// The settings window on a tablet or desktop: a centred panel with the sidebar.
#[allow(clippy::too_many_arguments)]
fn desktop<T: Copy + PartialEq, S, R: Default>(
    ctx: &egui::Context,
    id: &str,
    size: Vec2,
    title: &str,
    groups: &[Group<T>],
    open: &mut T,
    nav: &mut Nav,
    state: &mut S,
    header: impl FnOnce(&mut Ui, &mut S) -> bool,
    content: impl FnOnce(&mut Ui, &mut S) -> R,
) -> (R, bool) {
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

            let nav_rect =
                Rect::from_min_max(side.min + SIDEBAR_PAD_START, side.max - SIDEBAR_PAD_END);
            let side_ui = &mut child(ui, nav_rect, Align::Min);
            if header(side_ui, state)
                && let Some(first) = groups.iter().flat_map(|group| &group.pages).next()
            {
                *open = first.id;
            }
            let (jump, _) = sidebar(side_ui, id, groups, open, nav, false);

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
                Some(title),
                colors.side,
                jump,
                nav,
                |ui| content(ui, state),
            )
        });
    let close = close || modal.should_close();
    (modal.inner, close)
}

/// The settings window on a phone: a full-screen sheet with the page list, or the
/// open page under a bar with Back.
#[allow(clippy::too_many_arguments)]
fn phone<T: Copy + PartialEq, S, R: Default>(
    ctx: &egui::Context,
    id: &str,
    name: &str,
    title: &str,
    groups: &[Group<T>],
    open: &mut T,
    nav: &mut Nav,
    state: &mut S,
    header: impl FnOnce(&mut Ui, &mut S) -> bool,
    content: impl FnOnce(&mut Ui, &mut S) -> R,
) -> (R, bool) {
    let modal = ui::sheet(ctx, UiId::unique(id), |ui| {
        let colors = ui::colors(ui);
        if nav.phone_page {
            let (back, close) = ui::sheet_bar(ui, title, Some(t!("common.back")));
            // Android's Back too, before the sheet's Escape closes it all.
            nav.phone_page = !(back || crate::back::take(ui));
            let body = ui
                .available_rect_before_wrap()
                .shrink2(Vec2::new(ui::SPACE_L, 0.0));
            let mut body = child(ui, body, Align::Min);
            body.add_space(ui::SPACE_L);
            let content = |ui: &mut Ui| content(ui, state);
            let inner = page_body(
                &mut body,
                (id, title),
                None,
                colors.side,
                None,
                nav,
                content,
            );
            return (inner, close);
        }
        let (_, close) = ui::sheet_bar(ui, name, None);
        let list = ui
            .available_rect_before_wrap()
            .shrink2(Vec2::new(ui::SPACE_S, 0.0));
        let mut list = child(ui, list, Align::Min);
        list.add_space(ui::SPACE_S);
        let first = groups.iter().flat_map(|group| &group.pages).next();
        if header(&mut list, state)
            && let Some(first) = first
        {
            *open = first.id;
            nav.phone_page = true;
        }
        let (_, picked) = sidebar(&mut list, id, groups, open, nav, true);
        nav.phone_page |= picked;
        (R::default(), close)
    });
    let close = modal.should_close();
    let (inner, closed) = modal.inner;
    (inner, closed || close)
}

/// A top-down child UI filling `rect`.
fn child(ui: &mut Ui, rect: Rect, align: Align) -> Ui {
    ui.new_child(
        UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::top_down(align)),
    )
}

/// How far the arrow keys scroll a page.
const ARROW_STEP: f32 = 40.0;

/// Scrolling by keyboard, as the scroll bars are hidden: arrows, Page Up and Down, Home
/// and End, while nothing has focus (a focused field or button keeps its keys).
fn keyboard_scroll(ui: &mut Ui, view: f32) {
    if ui.memory(|memory| memory.focused().is_some()) {
        return;
    }
    let page = (view - 2.0 * ARROW_STEP).max(ARROW_STEP);
    let delta = ui.input(|input| {
        [
            (Key::ArrowUp, ARROW_STEP),
            (Key::ArrowDown, -ARROW_STEP),
            (Key::PageUp, page),
            (Key::PageDown, -page),
            (Key::Home, f32::INFINITY),
            (Key::End, f32::NEG_INFINITY),
        ]
        .into_iter()
        .filter(|(key, _)| input.key_pressed(*key))
        .map(|(_, delta)| delta)
        .sum::<f32>()
    });
    if delta != 0.0 && !delta.is_nan() {
        // Home and End go as far as the content goes.
        let delta = delta.clamp(-1e6, 1e6);
        ui.scroll_with_delta(Vec2::new(0.0, delta));
    }
}

/// The page in a scroll area of reading width: its title (if not shown elsewhere),
/// then its sections. Records the sections in `nav` and which one is in view.
fn page_body<R>(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    title: Option<&str>,
    fields: egui::Color32,
    jump: Option<usize>,
    nav: &mut Nav,
    content: impl FnOnce(&mut Ui) -> R,
) -> R {
    let view = ui.available_height();
    // The scroll bar stays hidden for a cleaner page; wheel, touchpad and keys still scroll.
    let output = ScrollArea::vertical()
        .id_salt(id)
        .auto_shrink([false, false])
        .scroll_bar_visibility(ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| {
            keyboard_scroll(ui, view);
            ui.set_max_width((ui.available_width() - 12.0).clamp(0.0, CONTENT_MAX_WIDTH));
            // Text fields and cards stand out from the page.
            ui.visuals_mut().extreme_bg_color = fields;
            if let Some(title) = title {
                ui.label(ui::heading(title).size(TEXT_HEADING));
                ui.add_space(SPACE_S);
            }
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
/// open page's sections under it. On a phone (`list`) it is a plain list of pages.
/// Returns the section to scroll to, and whether a page was picked.
fn sidebar<T: Copy + PartialEq>(
    ui: &mut Ui,
    id: &str,
    groups: &[Group<T>],
    open: &mut T,
    nav: &mut Nav,
    list: bool,
) -> (Option<usize>, bool) {
    ui.add_space(SPACE_S);
    let search = ui.add(
        ui::field(&mut nav.search)
            .hint_text(t!("common.search"))
            .desired_width(f32::INFINITY),
    );
    search
        .widget_info(|| egui::WidgetInfo::labeled(Role::SearchInput, true, t!("settings.search")));
    let query = nav.search.trim().to_lowercase();
    let mut jump = None;
    let mut picked = false;
    ui.add_space(SPACE_S);
    ScrollArea::vertical()
        .id_salt((id, "pages"))
        .auto_shrink([false, false])
        .scroll_bar_visibility(ScrollBarVisibility::AlwaysHidden)
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
                    let selected = *open == page.id && !list;
                    let row =
                        ui::nav_row(ui, page.icon, page.label, selected, Role::Tab, page.label);
                    if row.clicked() {
                        *open = page.id;
                        picked = true;
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
                ui.weak(t!("settings.nothing_found"));
            }
        });
    if jump.is_some() || picked {
        ui.ctx().request_repaint();
    }
    (jump, picked)
}

/// A group header in the sidebar, like "USER SETTINGS".
fn group_caption(ui: &mut Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(CAPTION_X);
        ui::caption_label(ui, text);
    });
}

/// The round × with "ESC" under it (touch screens have no Escape key). True when
/// clicked.
fn close_button(ui: &mut Ui) -> bool {
    let clicked = ui::close_button(ui, CLOSE_SIZE).clicked();
    if ui::TOUCH {
        return clicked;
    }
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
    keystore: &mut crate::keystore::Keystore,
) -> Option<Action> {
    if home.settings != Some(Tab::Privacy) {
        crate::key_storage::leave(ctx, &mut home.key_storage);
    }
    if home.settings != Some(Tab::Network) {
        crate::route::forget(ctx);
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    if home.settings != Some(Tab::Phone) {
        call::phone::leave_settings(&mut home.voice);
    }
    let mut tab = home.settings?;
    let page = |id, icon, label| Page { id, icon, label };
    let mut groups = [
        Group {
            title: t!("settings.group_user"),
            pages: vec![
                page(Tab::Profile, "👤", t!("settings.my_account")),
                page(Tab::Privacy, "🔒", t!("settings.privacy")),
                page(Tab::Devices, "💻", t!("settings.devices")),
            ],
        },
        Group {
            title: t!("settings.group_app"),
            pages: vec![
                page(Tab::Appearance, "🎨", t!("settings.appearance")),
                page(Tab::Voice, "🎤", t!("settings.voice")),
                page(Tab::Video, "🎥", t!("settings.video")),
                page(Tab::Shortcuts, "⌨", t!("settings.shortcuts")),
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                page(Tab::Phone, "📞", t!("phone.settings")),
                page(Tab::Network, "🌐", t!("settings.network")),
                page(Tab::Notifications, "🔔", t!("settings.notifications")),
                #[cfg(target_os = "android")]
                page(Tab::Advanced, "⚙", t!("settings.advanced")),
                page(
                    Tab::Updates,
                    "🔄",
                    if updates.available() {
                        t!("settings.updates_new")
                    } else {
                        t!("settings.updates")
                    },
                ),
            ],
        },
        Group {
            title: t!("settings.group_info"),
            pages: vec![
                page(Tab::WhatsNew, "✨", t!("settings.whats_new")),
                page(Tab::About, "ℹ", t!("settings.about")),
            ],
        },
    ];
    for group in &mut groups {
        group.pages.retain(|page| listed(page.id, PHONE));
    }
    let before = tab;
    let mut open_news = false;
    let ((), close) = window(
        ctx,
        "settings",
        t!("settings.title"),
        &groups,
        &mut tab,
        home,
        |ui, home| {
            let (dir, name) = me(home);
            let me = *home.identity.certificate.person_id();
            sidebar_header(ui, &name, t!("settings.edit_profile"), |ui| {
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
                Tab::Notifications => notifications_page(ui, home),
                Tab::Shortcuts => crate::hotkeys::settings_page(ui, home),
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                Tab::Phone => call::phone::settings_page(ui, &mut home.voice),
                Tab::Network => {
                    if crate::route::settings_page(ui, &mut home.route_editor) {
                        home.route_generation = rusttalk_core::route::generation();
                        home.on_route_changed(updates);
                    }
                }
                Tab::Privacy => privacy_tab(ui, home, keystore),
                Tab::Devices => devices_page(ui, home),
                #[cfg(target_os = "android")]
                Tab::Advanced => {
                    let mut settings = home.voice.audio.settings();
                    if ui::toggle_row(
                        ui,
                        &mut settings.android_terminal_hosting,
                        t!("call.share_terminal"),
                        t!("settings.terminal_hosting_help"),
                    )
                    .changed()
                    {
                        if !settings.android_terminal_hosting {
                            call::terminal::stop(home);
                        }
                        home.voice.audio.set_settings(settings);
                    }
                }
                Tab::Updates => updates.settings(ui),
                Tab::WhatsNew => crate::whats_new::page(ui),
                Tab::About => {
                    ui::page_section(ui, &format!("RustTalk {}", env!("CARGO_PKG_VERSION")));
                    if ui::secondary(ui, t!("settings.whats_new")).clicked() {
                        open_news = true;
                    }
                    ui.add_space(SPACE_S);
                    ui.label(t!("settings.about_license"));
                    ui.hyperlink_to(t!("settings.about_source"), REPOSITORY);
                    ui.add_space(SPACE_S);
                    ui.label(t!("settings.about_twemoji"));
                    ui.hyperlink_to(
                        t!("settings.about_twemoji_source"),
                        "https://github.com/jdecked/twemoji",
                    );
                    ui.hyperlink_to(
                        t!("settings.about_twemoji_license"),
                        "https://creativecommons.org/licenses/by/4.0/",
                    );
                }
            }
            error_text(ui, home.error.as_deref());
        },
        unsaved_profile,
    );
    home.settings = Some(if open_news { Tab::WhatsNew } else { tab });
    if close && !home.confirm_sign_out && home.remove_device.is_none() {
        home.settings = None;
        home.error = None;
        home.name_draft = None;
        home.bio_draft = None;
        home.publish_settings();
    }

    if let Some(form) = &mut home.remove_device {
        let field = UiId::unique("remove-device-words");
        let (action, dismissed) =
            ui::dialog(ctx, "remove-device", t!("settings.remove_device"), |ui| {
                form.show(ui, field)
            });
        if dismissed || action.is_some() {
            ctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(field));
            let form = home.remove_device.take().expect("open removal");
            if action == Some(true) {
                match rusttalk_core::identity::revoke_device(
                    &form.words,
                    home.identity.certificate.person_id(),
                    form.target,
                ) {
                    Ok(record) => {
                        if let Err(error) = home.remove_device(&record) {
                            tracing::warn!(%error, "could not save device removal");
                            home.error = Some(t!("settings.remove_failed").into());
                        } else {
                            home.error = None;
                        }
                    }
                    Err(_) => {
                        home.remove_device = Some(RemoveDevice {
                            error: Some(t!("settings.remove_words_error")),
                            ..RemoveDevice::new(form.target)
                        })
                    }
                }
            }
        }
    }
    if !home.confirm_sign_out {
        return None;
    }
    let mut action = None;
    let mut cancel = false;
    let ((), dismissed) = ui::dialog(ctx, "sign-out", t!("settings.sign_out_title"), |ui| {
        ui.label(t!("settings.sign_out_text"));
        ui::footer(ui, |ui| {
            if ui::danger(ui, t!("common.sign_out")).clicked() {
                action = Some(Action::SignOut);
            }
            cancel = ui::secondary(ui, t!("common.cancel")).clicked();
        });
    });
    if cancel || dismissed {
        home.confirm_sign_out = false;
    }
    action
}

/// A server that knows this person's picture, and their default name (before there
/// is one, the name on their first server).
fn me(home: &Home) -> (&Directory, String) {
    static NONE: std::sync::LazyLock<Directory> = std::sync::LazyLock::new(Directory::default);
    let me = home.identity.certificate.person_id();
    let dir = (home.servers.iter())
        .find(|s| s.member(me).is_some_and(|m| m.avatar.is_some()))
        .map_or(&*NONE, |s| &s.dir);
    let default = home.voice.audio.settings().display_name;
    let name = (!default.is_empty())
        .then_some(default)
        .or_else(|| (home.servers.iter()).find_map(|s| s.member(me).and_then(|m| m.name.clone())))
        .unwrap_or_else(|| t!("common.you").into());
    (dir, name)
}

fn account_page(ui: &mut Ui, home: &mut Home) {
    ui::page_section(ui, t!("settings.profile"));
    ui.horizontal(|ui| {
        let (dir, name) = me(home);
        let me = *home.identity.certificate.person_id();
        face(ui, dir, &me, &name, PICTURE_SIZE, false);
        ui.add_space(SPACE_S);
        ui.vertical(|ui| {
            ui::setting_row(
                ui,
                t!("settings.picture"),
                t!("settings.picture_help"),
                |ui| {
                    let change = ui
                        .add_enabled_ui(!home.servers.is_empty() && crate::files::PICKER, |ui| {
                            ui::secondary(ui, t!("settings.change_picture"))
                        })
                        .inner
                        .on_hover_text(t!("settings.picture_types"));
                    if change.clicked() {
                        home.error = None;
                        home.files.ask(ui.ctx(), Ask::Picture);
                    }
                },
            );
        });
    });
    name_row(ui, home);
    bio_editor(ui, home);
    ui::page_section(ui, t!("settings.your_id"));
    ui.weak(t!("settings.your_id_help"));
    ui::card(ui, |ui| {
        ui.monospace(fingerprint(home.identity.certificate.person_id()));
    });
}

/// A name or bio typed in My account and not saved yet.
fn unsaved_profile(home: &Home) -> bool {
    let settings = home.voice.audio.settings();
    let name =
        (home.name_draft.as_deref()).is_some_and(|name| name.trim() != settings.display_name);
    let bio = (home.bio_draft.as_deref())
        .is_some_and(|bio| crate::profile::clean_bio(bio) != settings.bio);
    name || bio
}

/// The default name: shown with an edit button, or a field while editing.
fn name_row(ui: &mut Ui, home: &mut Home) {
    let saved = home.voice.audio.settings().display_name;
    let mut save = None;
    ui::setting_row(
        ui,
        t!("settings.name"),
        t!("settings.name_help"),
        |ui| match &mut home.name_draft {
            None => {
                if !saved.is_empty() {
                    ui.label(ui::bold(&saved));
                }
                if ui::secondary(ui, t!("settings.edit_name")).clicked() {
                    home.name_draft = Some(saved.clone());
                }
            }
            Some(draft) => {
                let field = ui.add(
                    ui::field(draft)
                        .char_limit(crate::profile::NAME_CHARS)
                        .desired_width(180.0),
                );
                let entered = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                if ui::primary(ui, t!("common.save")).clicked() || entered {
                    save = Some(draft.trim().to_owned());
                }
                if ui::secondary(ui, t!("common.cancel")).clicked() {
                    home.name_draft = None;
                }
            }
        },
    );
    if let Some(name) = save.filter(|name| !name.is_empty()) {
        home.name_draft = None;
        home.change_profile(|settings, servers| {
            let told = settings.set_default_name(&name, servers.iter());
            (told.into_iter())
                .map(|server| (server, Command::SetDisplayName(name.clone())))
                .collect()
        });
    }
}

/// The bio: a few lines with a live count, saved to every server.
fn bio_editor(ui: &mut Ui, home: &mut Home) {
    use crate::profile::{BIO_CHARS, BIO_LINES, clean_bio, fit_bio};
    let saved = home.voice.audio.settings().bio;
    let draft = home.bio_draft.get_or_insert_with(|| saved.clone());
    ui::setting_row(ui, t!("settings.bio"), t!("settings.bio_help"), |_| {});
    let label = t!("settings.bio");
    let response = ui.add(
        ui::text_area(draft)
            .hint_text(t!("settings.bio_hint"))
            .desired_rows(3)
            .desired_width(f32::INFINITY),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(Role::MultilineTextInput, true, label));
    let fitted = fit_bio(draft);
    if fitted != *draft {
        *draft = fitted;
    }
    let changed = clean_bio(draft) != saved;
    let (count, lines) = (draft.chars().count(), draft.split('\n').count());
    let (mut save, mut cancel) = (false, false);
    ui.horizontal(|ui| {
        ui.label(ui::caption(format!("{count}/{BIO_CHARS}")))
            .on_hover_text(t!(
                "settings.bio_limits",
                lines = BIO_LINES,
                chars = BIO_CHARS
            ));
        if lines >= BIO_LINES {
            ui.label(ui::caption(t!(
                "settings.bio_lines_full",
                lines = BIO_LINES
            )));
        }
        if changed {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                save = ui::primary(ui, t!("common.save")).clicked();
                cancel = ui::secondary(ui, t!("common.cancel")).clicked();
            });
        }
    });
    if cancel {
        home.bio_draft = None;
    }
    if save {
        let bio = clean_bio(home.bio_draft.as_deref().unwrap_or_default());
        home.bio_draft = Some(bio.clone());
        home.change_profile(|settings, servers| {
            settings.set_bio(&bio, servers.iter());
            (servers.into_iter())
                .map(|server| (server, Command::SetBio(bio.clone())))
                .collect()
        });
    }
}

/// Settings → Notifications: the switches for every server. Servers and channels
/// pick their own level and mute from their right-click menus.
fn notifications_page(ui: &mut Ui, home: &mut Home) {
    let mut prefs = home.notify.prefs().clone();
    ui::page_section(ui, t!("settings.desktop_notifications"));
    ui::toggle_row(
        ui,
        &mut prefs.desktop,
        t!("settings.desktop_notifications"),
        t!("settings.desktop_notifications_help"),
    );
    ui.add_enabled_ui(prefs.desktop, |ui| {
        ui::toggle_row(
            ui,
            &mut prefs.sound,
            t!("settings.sound"),
            t!("settings.sound_help"),
        );
        ui::toggle_row(
            ui,
            &mut prefs.unfocused_only,
            t!("settings.unfocused_only"),
            t!("settings.unfocused_only_help"),
        );
    });
    let mut audio = home.voice.audio.settings();
    call_sounds(ui, &mut audio.call_sounds);
    home.voice.audio.set_settings(audio);
    ui::page_section(ui, t!("settings.servers_and_channels"));
    ui.weak(t!("settings.servers_and_channels_help"));
    home.notify.set_prefs(prefs);
}

fn call_sounds(ui: &mut Ui, prefs: &mut crate::sounds::Preferences) {
    ui::page_section(ui, t!("sounds.section"));
    ui::toggle_row(ui, &mut prefs.enabled, t!("sounds.enabled"), "");
    ui.add_enabled_ui(prefs.enabled, |ui| {
        let label = t!("sounds.volume");
        ui::setting_row(ui, label, "", |ui| {
            let slider = egui::Slider::new(&mut prefs.volume, 0.0..=1.0)
                .custom_formatter(|value, _| format!("{:.0}%", value * 100.0))
                .custom_parser(|text| {
                    Some(text.trim().trim_end_matches('%').parse::<f64>().ok()? / 100.0)
                });
            ui.add(slider).widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::Slider, ui.is_enabled(), label)
            });
        });
        for cue in crate::sounds::Cue::ALL {
            let mut enabled = !prefs.disabled.contains(&cue);
            if ui::toggle_row(ui, &mut enabled, cue.label(), "").changed() {
                if enabled {
                    prefs.disabled.retain(|&disabled| disabled != cue);
                } else {
                    prefs.disabled.push(cue);
                }
            }
        }
    });
}

pub struct RemoveDevice {
    target: [u8; 32],
    words: zeroize::Zeroizing<String>,
    error: Option<&'static str>,
}

impl RemoveDevice {
    fn new(target: [u8; 32]) -> Self {
        Self {
            target,
            words: crate::key_storage::secret_input(),
            error: None,
        }
    }

    fn show(&mut self, ui: &mut Ui, field: UiId) -> Option<bool> {
        ui.monospace(fingerprint(&self.target));
        ui.label(t!("settings.remove_effect"));
        let label = ui.label(t!("app.recovery_words"));
        ui.add(
            ui::field(&mut *self.words)
                .password()
                .id(field)
                .desired_width(f32::INFINITY),
        )
        .labelled_by(label.id);
        error_text(ui, self.error);
        let mut answer = None;
        ui::footer(ui, |ui| {
            if ui
                .add_enabled_ui(!self.words.trim().is_empty(), |ui| {
                    ui::danger(ui, t!("settings.remove_device"))
                })
                .inner
                .clicked()
            {
                answer = Some(true);
            }
            if ui::secondary(ui, t!("common.cancel")).clicked() {
                answer = Some(false);
            }
        });
        answer
    }
}

fn devices_page(ui: &mut Ui, home: &mut Home) {
    ui::page_section(ui, t!("settings.this_device"));
    ui::setting_row(
        ui,
        home.identity.certificate.name(),
        &format!(
            "{} · {}",
            if PHONE {
                t!("settings.this_device")
            } else {
                t!("settings.this_computer")
            },
            fingerprint(home.identity.certificate.signing_key().as_bytes())
        ),
        |_| {},
    );
    let current = &home.identity.certificate;
    let devices: std::collections::BTreeMap<_, _> = home
        .servers
        .iter()
        .filter_map(|server| server.member(current.person_id()))
        .flat_map(|member| &member.devices)
        .filter(|device| &device.signing_key != current.signing_key().as_bytes())
        .map(|device| (device.signing_key, device))
        .collect();
    if !devices.is_empty() {
        ui::page_section(ui, t!("settings.other_devices"));
        for (key, device) in devices {
            if ui.push_id(key, |ui| other_device_row(ui, device)).inner {
                home.remove_device = Some(RemoveDevice::new(key));
            }
        }
    }
    ui::danger_zone(ui, |ui| {
        ui::setting_row(
            ui,
            t!("settings.sign_out_device"),
            t!("settings.sign_out_device_help"),
            |ui| {
                if ui::danger(ui, t!("common.sign_out")).clicked() {
                    home.confirm_sign_out = true;
                }
            },
        );
    });
}

fn other_device_row(ui: &mut Ui, device: &crate::server::Device) -> bool {
    let name = if device.name.is_empty() {
        t!("settings.device")
    } else {
        &device.name
    };
    ui::setting_row(ui, name, &device.fingerprint, |ui| {
        ui::danger(ui, t!("settings.remove_device")).clicked()
    })
}

/// Settings → Appearance: dark, light or the system's mode, a theme for each, and
/// the accent colour, and the language. Changes show at once.
fn appearance_page(ui: &mut Ui, home: &mut Home) {
    let audio = home.voice.audio.clone();
    let mut settings = audio.settings();
    let before = settings.appearance();
    let mut look = before;
    let language = settings.language.clone();
    let emoji_on_hover = settings.emoji_on_hover;
    ui::page_section(ui, t!("settings.language"));
    ui::setting_row(
        ui,
        t!("settings.app_language"),
        t!("settings.language_help"),
        |ui| {
            let current =
                Lang::from_id(&settings.language).map_or(t!("common.system_default"), Lang::name);
            let combo = ComboBox::from_id_salt("language")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    let choice = &mut settings.language;
                    ui.selectable_value(choice, "system".into(), t!("common.system_default"));
                    for lang in LANGS {
                        ui.selectable_value(choice, lang.id().into(), lang.name());
                    }
                });
            combo.response.widget_info(|| {
                egui::WidgetInfo::labeled(Role::ComboBox, true, t!("settings.app_language"))
            });
        },
    );
    ui::page_section(ui, t!("settings.mode"));
    let modes = [
        (
            Mode::Dark,
            "🌙",
            t!("settings.dark"),
            t!("settings.dark_help"),
        ),
        (
            Mode::Light,
            "☀",
            t!("settings.light"),
            t!("settings.light_help"),
        ),
        (
            Mode::System,
            "🖥",
            t!("settings.system"),
            t!("settings.system_help"),
        ),
    ];
    // Three in a row, or two when three would be squeezed.
    let room = ui.available_width();
    let columns = if room >= 3.0 * 140.0 + 2.0 * SPACE_S {
        3.0
    } else {
        2.0
    };
    let width = ((room - (columns - 1.0) * SPACE_S) / columns).clamp(140.0, 200.0);
    ui.horizontal_wrapped(|ui| {
        for (mode, icon, title, help) in modes {
            let selected = look.mode == mode;
            if ui::choice_card_sized(ui, width, selected, icon, title, help).clicked() {
                look.mode = mode;
            }
        }
    });
    for (caption, themes, dark) in [
        (t!("settings.dark_theme"), DARK_THEMES, true),
        (t!("settings.light_theme"), LIGHT_THEMES, false),
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
    ui::page_section(ui, t!("settings.accent"));
    ui.weak(t!("settings.accent_help"));
    ui.horizontal_wrapped(|ui| {
        ui.set_min_height(CONTROL_HEIGHT);
        for accent in ui::ACCENTS {
            if ui::swatch(ui, accent, look.accent == accent).clicked() {
                look.accent = accent;
            }
        }
    });
    ui::page_section(ui, t!("settings.motion"));
    ui::setting_row(
        ui,
        t!("settings.animated_emoji"),
        t!("settings.animated_emoji_help"),
        |ui| {
            let current = if settings.emoji_on_hover {
                t!("settings.on_hover")
            } else {
                t!("settings.always")
            };
            let combo = ComboBox::from_id_salt("animated-emoji")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    let choice = &mut settings.emoji_on_hover;
                    ui.selectable_value(choice, false, t!("settings.always"));
                    ui.selectable_value(choice, true, t!("settings.on_hover"));
                });
            combo.response.widget_info(|| {
                egui::WidgetInfo::labeled(Role::ComboBox, true, t!("settings.animated_emoji"))
            });
        },
    );
    if look != before || settings.language != language || settings.emoji_on_hover != emoji_on_hover
    {
        crate::i18n::set(&settings.language);
        settings.set_appearance(look);
        audio.set_settings(settings);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phones_list_no_updates_or_shortcuts_page() {
        for tab in [Tab::Updates, Tab::Shortcuts] {
            assert!(!listed(tab, true));
            assert!(listed(tab, false));
        }
        for tab in [
            Tab::Profile,
            Tab::Voice,
            Tab::Network,
            Tab::About,
            Tab::WhatsNew,
        ] {
            assert!(listed(tab, true) && listed(tab, false));
        }
    }
}
