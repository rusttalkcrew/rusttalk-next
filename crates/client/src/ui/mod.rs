//! RustTalk's design language: colours, spacing, radii, type scale and animation
//! timing as tokens, the global egui theme built from them, and the shared widgets
//! and menu every screen uses. Change the look here, not in the screens.

use std::sync::Arc;

use eframe::egui::{
    Color32, Context, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Id, Margin,
    RichText, Shadow, Stroke, Style, TextStyle, Theme, Ui, Vec2, Visuals, style::Selection,
};

mod completion;
mod menu;
mod page;
mod theme;
mod widgets;

pub use completion::completion;
pub use menu::{
    Item, Menu, context_menu, is_open as menu_open, menu, menu_above, menu_below, menu_wide,
};
pub use page::*;
pub use theme::{
    ACCENTS, Accent, Appearance, DARK_THEMES, LIGHT_THEMES, Mode, Palette, ROLE_COLORS, contrast,
};
pub use widgets::*;

// Spacing scale.
pub const SPACE_XS: f32 = 4.0;
pub const SPACE_S: f32 = 8.0;
pub const SPACE_M: f32 = 12.0;
pub const SPACE_L: f32 = 16.0;
pub const SPACE_XL: f32 = 24.0;

// Corner radii.
pub const RADIUS_S: u8 = 4;
/// List rows: channels, menu items, sidebar pages.
pub const RADIUS_ROW: u8 = 6;
/// Buttons, fields and cards.
pub const RADIUS: u8 = 8;
pub const RADIUS_MENU: u8 = 10;
pub const RADIUS_WINDOW: u8 = 12;

// Type scale. Body text is Inter Regular; `bold` text and titles are Inter SemiBold.
pub const TEXT_CAPTION: f32 = 12.0;
pub const TEXT_SMALL: f32 = 13.0;
pub const TEXT_BODY: f32 = 15.0;
pub const TEXT_MONO: f32 = 14.0;
/// Pane titles: the channel name over a chat, section names in a page.
pub const TEXT_TITLE: f32 = 17.0;
/// Dialog and page headings.
pub const TEXT_HEADING: f32 = 22.0;
pub const TEXT_ICON: f32 = 16.0;
/// File and live-file icons in messages.
pub const TEXT_ICON_LARGE: f32 = 22.0;

/// The font family for SemiBold text.
pub fn bold_family() -> FontFamily {
    FontFamily::Name(SEMIBOLD.into())
}
const SEMIBOLD: &str = "semibold";
const INTER: &str = "Inter";
const JAPANESE: &str = "Noto Sans JP";
const INTER_SEMIBOLD: &str = "Inter-SemiBold";

// Sizes.
/// Buttons and fields.
pub const CONTROL_HEIGHT: f32 = 32.0;
pub const LARGE_CONTROL: Vec2 = Vec2::new(240.0, 40.0);
pub const ROW_HEIGHT: f32 = 32.0;
pub const ICON_BUTTON: f32 = 32.0;
pub const MENU_WIDTH: f32 = 220.0;
pub const MENU_PADDING: i8 = 8;
pub const MENU_ROW: f32 = 34.0;
/// A menu row with a second line of help.
pub const MENU_ROW_TALL: f32 = 46.0;
pub const POPUP_WIDTH: f32 = 300.0;
/// Menus with a line of help under each item.
pub const MENU_WIDE: f32 = 340.0;
pub const DIALOG_WIDTH: f32 = 420.0;
pub const DIALOG_PADDING: i8 = 20;
pub const TOOLTIP_WIDTH: f32 = 280.0;

// Animation timing, in seconds.
/// Hover and press feedback.
pub const HOVER_SECONDS: f32 = 0.12;
/// A control turning on or off.
pub const STATE_SECONDS: f32 = 0.18;
/// A card appearing.
pub const CARD_SECONDS: f32 = 0.2;
/// Rows sliding aside while dragging.
pub const SLIDE_SECONDS: f32 = 0.15;
/// How far a card slides up while it appears.
pub const CARD_SLIDE: f32 = 8.0;
/// Glyph growth on hover and shrink while pressed.
pub const HOVER_GROW: f32 = 0.1;
pub const PRESS_SHRINK: f32 = 0.12;

/// Text and glyphs on accent, danger and success fills, in every theme.
pub const ON_FILL: Color32 = Color32::WHITE;
/// The dark backdrop behind the picture viewer, its buttons and text on it.
pub const OVERLAY: Color32 = Color32::from_black_alpha(225);
pub const OVERLAY_BUTTON: Color32 = Color32::from_black_alpha(160);
pub const OVERLAY_TEXT: Color32 = Color32::from_gray(0xd0);

fn appearance_id() -> Id {
    Id::unique("rusttalk appearance")
}

/// The person's look, as last applied.
pub fn appearance(ctx: &Context) -> Appearance {
    ctx.data(|data| data.get_temp(appearance_id()))
        .unwrap_or_default()
}

/// The palette `ui` is drawn in: the dark or the light theme the person picked.
pub fn colors(ui: &Ui) -> &'static Palette {
    appearance(ui.ctx()).palette(ui.visuals().dark_mode)
}

/// The accent colour the person picked.
pub fn accent(ui: &Ui) -> &'static Accent {
    appearance(ui.ctx()).accent
}

/// Set fonts, sizes and spacing once at start, and the default look.
pub fn install(ctx: &Context) {
    ctx.set_fonts(fonts());
    crate::emoji::install(ctx);
    ctx.all_styles_mut(base_style);
    apply(ctx, Appearance::default());
}

/// Switch to `look` if it changed: both egui styles, so plain egui widgets already
/// look right, and the mode. With "System" egui follows the OS as it changes.
pub fn apply(ctx: &Context, look: Appearance) {
    let current: Option<Appearance> = ctx.data(|data| data.get_temp(appearance_id()));
    if current == Some(look) {
        return;
    }
    ctx.data_mut(|data| data.insert_temp(appearance_id(), look));
    ctx.style_mut_of(Theme::Dark, |style| {
        theme(&mut style.visuals, look.dark, look.accent)
    });
    ctx.style_mut_of(Theme::Light, |style| {
        theme(&mut style.visuals, look.light, look.accent);
    });
    ctx.set_theme(look.mode.preference());
}

/// Inter for all proportional text, with egui's own fonts after it for anything Inter
/// lacks (emoji, icons), then Japanese: egui has no CJK and not every Linux install
/// has a Japanese font. Other scripts come from the system's fonts.
fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let regular = include_bytes!("../../assets/fonts/Inter-Regular.ttf");
    let semibold = include_bytes!("../../assets/fonts/Inter-SemiBold.ttf");
    let japanese = include_bytes!("../../assets/fonts/NotoSansJP-Subset.otf");
    fonts.font_data.insert(
        JAPANESE.into(),
        Arc::new(FontData::from_static(japanese.as_slice())),
    );
    fonts.font_data.insert(
        INTER.into(),
        Arc::new(FontData::from_static(regular.as_slice())),
    );
    fonts.font_data.insert(
        INTER_SEMIBOLD.into(),
        Arc::new(FontData::from_static(semibold.as_slice())),
    );
    let mut fallback = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    fallback.push(JAPANESE.to_owned());
    let family = |first: &str| {
        std::iter::once(first.to_owned())
            .chain(fallback.clone())
            .collect()
    };
    fonts
        .families
        .insert(FontFamily::Proportional, family(INTER));
    fonts.families.insert(bold_family(), family(INTER_SEMIBOLD));
    (fonts.families.entry(FontFamily::Monospace).or_default()).push(JAPANESE.to_owned());
    fonts
        .families
        .insert(crate::emoji::family(false), family(INTER));
    fonts
        .families
        .insert(crate::emoji::family(true), family(INTER_SEMIBOLD));
    fonts
}

fn base_style(style: &mut Style) {
    use FontFamily::{Monospace, Proportional};
    style.text_styles = [
        (TextStyle::Small, FontId::new(TEXT_CAPTION, Proportional)),
        (TextStyle::Body, FontId::new(TEXT_BODY, Proportional)),
        (TextStyle::Button, FontId::new(TEXT_BODY, Proportional)),
        (TextStyle::Heading, FontId::new(TEXT_HEADING, bold_family())),
        (TextStyle::Monospace, FontId::new(TEXT_MONO, Monospace)),
    ]
    .into();
    let spacing = &mut style.spacing;
    spacing.item_spacing = Vec2::new(SPACE_S, SPACE_S);
    spacing.button_padding = Vec2::new(SPACE_M, 6.0);
    spacing.interact_size.y = 22.0;
    spacing.menu_margin = Margin::same(MENU_PADDING);
    spacing.window_margin = Margin::same(DIALOG_PADDING);
    spacing.tooltip_width = TOOLTIP_WIDTH;
    spacing.combo_height = 300.0;
    // Tooltips show at once, also while the pointer moves.
    let interaction = &mut style.interaction;
    interaction.tooltip_delay = 0.0;
    interaction.tooltip_grace_time = 0.0;
    interaction.show_tooltips_only_when_still = false;
}

fn theme(visuals: &mut Visuals, c: &Palette, accent: &Accent) {
    *visuals = if c.dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    visuals.panel_fill = c.main;
    visuals.window_fill = c.raised;
    visuals.window_stroke = Stroke::new(1.0, c.border);
    visuals.extreme_bg_color = c.field;
    visuals.text_edit_bg_color = None;
    visuals.faint_bg_color = c.hover_soft;
    visuals.code_bg_color = c.field;
    visuals.window_corner_radius = CornerRadius::same(RADIUS_WINDOW);
    visuals.menu_corner_radius = CornerRadius::same(RADIUS_MENU);
    visuals.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: c.shadow,
    };
    visuals.window_shadow = Shadow {
        offset: [0, 8],
        blur: 32,
        spread: 0,
        color: c.shadow,
    };
    visuals.weak_text_color = Some(c.muted);
    visuals.hyperlink_color = accent.text(c.dark);
    visuals.error_fg_color = c.danger_text;
    visuals.warn_fg_color = c.warning;
    // Selected buttons and selected text: white on the accent. Focus outlines are
    // drawn by the kit in the accent instead.
    visuals.selection = Selection {
        bg_fill: accent.base,
        stroke: Stroke::new(1.5, ON_FILL),
    };
    visuals.text_cursor.stroke = Stroke::new(2.0, c.strong);
    visuals.slider_trailing_fill = true;
    visuals.striped = false;
    let widgets = &mut visuals.widgets;
    let states = [
        (&mut widgets.noninteractive, c.side, c.text),
        (&mut widgets.inactive, c.button, c.text),
        (&mut widgets.hovered, c.button_hover, c.strong),
        (&mut widgets.active, c.button_pressed, c.strong),
        (&mut widgets.open, c.button_hover, c.strong),
    ];
    for (state, fill, text) in states {
        state.weak_bg_fill = fill;
        state.bg_fill = fill;
        state.bg_stroke = Stroke::NONE;
        state.fg_stroke = Stroke::new(1.0, text);
        state.corner_radius = CornerRadius::same(RADIUS);
        state.expansion = 0.0;
    }
    // Separators and outlines.
    widgets.noninteractive.bg_stroke = Stroke::new(1.0, c.border);
    // Checkbox and radio boxes, slider rails.
    widgets.inactive.bg_fill = c.field;
}

/// SemiBold text, for names and short emphasis.
pub fn bold(text: impl Into<String>) -> RichText {
    RichText::new(text)
        .family(crate::emoji::family(true))
        .strong()
}

/// A pane or card title.
pub fn title(text: impl Into<String>) -> RichText {
    bold(text).size(TEXT_TITLE)
}

/// A page or dialog heading.
pub fn heading(text: impl Into<String>) -> RichText {
    bold(text).size(TEXT_HEADING)
}

/// Small grey text under or next to something.
pub fn caption(text: impl Into<String>) -> RichText {
    RichText::new(text).size(TEXT_CAPTION).weak()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inter_comes_first_with_egui_and_japanese_fonts_as_fallback() {
        let fonts = fonts();
        let default = FontDefinitions::default();
        let fallback = &default.families[&FontFamily::Proportional];
        for (family, first) in [
            (FontFamily::Proportional, INTER),
            (bold_family(), INTER_SEMIBOLD),
        ] {
            let list = &fonts.families[&family];
            assert_eq!(list[0], first);
            assert_eq!(&list[1..list.len() - 1], fallback.as_slice());
            assert_eq!(list.last().map(String::as_str), Some(JAPANESE));
        }
        assert!(fonts.font_data.contains_key(INTER));
        assert!(fonts.font_data.contains_key(JAPANESE));
        assert!(fonts.font_data.contains_key(INTER_SEMIBOLD));
    }

    #[test]
    fn tooltips_show_at_once() {
        let mut style = Style::default();
        base_style(&mut style);
        assert_eq!(style.interaction.tooltip_delay, 0.0);
        assert!(!style.interaction.show_tooltips_only_when_still);
    }
}
