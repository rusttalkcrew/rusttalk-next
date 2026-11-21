//! The colour themes: six palettes (three dark, three light), the accent swatches,
//! and the person's choice of mode, themes and accent.

use eframe::egui::{Color32, ThemePreference};

/// The colours of one theme.
pub struct Palette {
    pub id: &'static str,
    pub name: &'static str,
    pub dark: bool,
    /// The server rail, the darkest (or deepest) surface.
    pub rail: Color32,
    /// Channel and member lists, settings sidebars.
    pub side: Color32,
    /// The chat and every page.
    pub main: Color32,
    /// Text fields and cards on the main surface.
    pub field: Color32,
    /// The message bar floating over the chat: a step away from `main`.
    pub composer: Color32,
    /// Menus, popups and tooltips: a little lighter than the page.
    pub raised: Color32,
    /// A hovered row on a side surface or in a menu.
    pub hover: Color32,
    /// A hovered message row.
    pub hover_soft: Color32,
    /// The open channel or page.
    pub selected: Color32,
    /// Secondary buttons, idle, hovered and pressed.
    pub button: Color32,
    pub button_hover: Color32,
    pub button_pressed: Color32,
    pub border: Color32,
    pub text: Color32,
    pub strong: Color32,
    pub muted: Color32,
    pub success: Color32,
    pub warning: Color32,
    /// Fills: the leave and delete buttons, the live badge.
    pub danger: Color32,
    /// Danger as text and icons, readable on every surface.
    pub danger_text: Color32,
    pub offline: Color32,
    pub shadow: Color32,
}

/// An accent colour: primary buttons, the selection, links and focus.
pub struct Accent {
    pub id: &'static str,
    pub name: &'static str,
    /// The fill behind white text and icons.
    pub base: Color32,
    /// Hovered and pressed fills.
    pub strong: Color32,
    /// A light tint: accent text and icons on dark surfaces, subtle fills.
    pub tint: Color32,
}

impl Accent {
    /// The colour's name in the app's language.
    pub fn label(&self) -> &'static str {
        match self.id {
            "rust" => t!("accent.rust"),
            "blue" => t!("accent.blue"),
            "green" => t!("accent.green"),
            "violet" => t!("accent.violet"),
            "rose" => t!("accent.rose"),
            "teal" => t!("accent.teal"),
            _ => self.name,
        }
    }

    /// Accent text and icons on a theme's surfaces.
    pub fn text(&self, dark: bool) -> Color32 {
        if dark { self.tint } else { self.base }
    }

    pub fn mention_colors(&self, palette: &Palette) -> (Color32, Color32) {
        let text = if palette.dark { self.tint } else { self.strong };
        (text, palette.main.lerp_to_gamma(self.base, 0.13))
    }
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// Colours people can give a role. They are the same in every theme, as people chose them.
pub const ROLE_COLORS: [Color32; 10] = [
    rgb(0x1abc9c),
    rgb(0x2ecc71),
    rgb(0x3498db),
    rgb(0x9b59b6),
    rgb(0xe91e63),
    rgb(0xf1c40f),
    rgb(0xe67e22),
    rgb(0xe74c3c),
    rgb(0x95a5a6),
    rgb(0x607d8b),
];

const DARK_STATUS: (Color32, Color32, Color32, Color32) =
    (rgb(0x23a55a), rgb(0xf0b232), rgb(0xda373c), rgb(0xf6787c));
const LIGHT_STATUS: (Color32, Color32, Color32, Color32) =
    (rgb(0x1a7f45), rgb(0xb07800), rgb(0xd22d32), rgb(0xbb1f24));

pub const MIDNIGHT: Palette = Palette {
    id: "midnight",
    name: "Midnight",
    dark: true,
    rail: rgb(0x1e1f22),
    side: rgb(0x2b2d31),
    main: rgb(0x313338),
    field: rgb(0x1e1f22),
    composer: rgb(0x383a40),
    raised: rgb(0x393b41),
    hover: rgb(0x43454c),
    hover_soft: rgb(0x2e3035),
    selected: rgb(0x404249),
    button: rgb(0x4e5058),
    button_hover: rgb(0x5d5f68),
    button_pressed: rgb(0x6d6f78),
    border: rgb(0x3f4147),
    text: rgb(0xdbdee1),
    strong: rgb(0xf2f3f5),
    muted: rgb(0x9da3ab),
    success: DARK_STATUS.0,
    warning: DARK_STATUS.1,
    danger: DARK_STATUS.2,
    danger_text: DARK_STATUS.3,
    offline: rgb(0x80848e),
    shadow: Color32::from_black_alpha(110),
};

pub const GRAPHITE: Palette = Palette {
    id: "graphite",
    name: "Graphite",
    dark: true,
    rail: rgb(0x141414),
    side: rgb(0x1d1d1d),
    main: rgb(0x242424),
    field: rgb(0x141414),
    composer: rgb(0x2e2e2e),
    raised: rgb(0x2e2e2e),
    hover: rgb(0x393939),
    hover_soft: rgb(0x292929),
    selected: rgb(0x343434),
    button: rgb(0x444444),
    button_hover: rgb(0x525252),
    button_pressed: rgb(0x616161),
    border: rgb(0x353535),
    text: rgb(0xdedede),
    strong: rgb(0xf5f5f5),
    muted: rgb(0xa3a3a3),
    success: DARK_STATUS.0,
    warning: DARK_STATUS.1,
    danger: DARK_STATUS.2,
    danger_text: DARK_STATUS.3,
    offline: rgb(0x858585),
    shadow: Color32::from_black_alpha(140),
};

pub const DEEP_SEA: Palette = Palette {
    id: "deep-sea",
    name: "Deep sea",
    dark: true,
    rail: rgb(0x0b1520),
    side: rgb(0x10202e),
    main: rgb(0x152737),
    field: rgb(0x0b1520),
    composer: rgb(0x1c3245),
    raised: rgb(0x1c3245),
    hover: rgb(0x243d53),
    hover_soft: rgb(0x192d3f),
    selected: rgb(0x21394d),
    button: rgb(0x2b4861),
    button_hover: rgb(0x355670),
    button_pressed: rgb(0x406581),
    border: rgb(0x233a4e),
    text: rgb(0xd8e4ee),
    strong: rgb(0xf2f7fb),
    muted: rgb(0x93a9bb),
    success: DARK_STATUS.0,
    warning: DARK_STATUS.1,
    danger: DARK_STATUS.2,
    danger_text: DARK_STATUS.3,
    offline: rgb(0x7a8ea0),
    shadow: Color32::from_black_alpha(130),
};

pub const DAYLIGHT: Palette = Palette {
    id: "daylight",
    name: "Daylight",
    dark: false,
    rail: rgb(0xe3e5e8),
    side: rgb(0xf2f3f5),
    main: Color32::WHITE,
    field: rgb(0xebedef),
    composer: rgb(0xebedef),
    raised: Color32::WHITE,
    hover: rgb(0xe6e8eb),
    hover_soft: rgb(0xf5f6f7),
    selected: rgb(0xd7d9dc),
    button: rgb(0xe3e5e8),
    button_hover: rgb(0xd7d9dc),
    button_pressed: rgb(0xc4c9ce),
    border: rgb(0xdcdee2),
    text: rgb(0x2e3035),
    strong: rgb(0x060607),
    muted: rgb(0x5c5f66),
    success: LIGHT_STATUS.0,
    warning: LIGHT_STATUS.1,
    danger: LIGHT_STATUS.2,
    danger_text: LIGHT_STATUS.3,
    offline: rgb(0x80848e),
    shadow: Color32::from_black_alpha(40),
};

pub const PAPER: Palette = Palette {
    id: "paper",
    name: "Paper",
    dark: false,
    rail: rgb(0xe6e0d4),
    side: rgb(0xf2eee5),
    main: rgb(0xfbf9f4),
    field: rgb(0xece7dc),
    composer: rgb(0xefe9dd),
    raised: rgb(0xfffdf8),
    hover: rgb(0xe8e2d6),
    hover_soft: rgb(0xf5f1e8),
    selected: rgb(0xddd5c6),
    button: rgb(0xe4ddd0),
    button_hover: rgb(0xd9d1c2),
    button_pressed: rgb(0xcbc2b1),
    border: rgb(0xddd5c7),
    text: rgb(0x2f2a24),
    strong: rgb(0x141210),
    muted: rgb(0x655d52),
    success: LIGHT_STATUS.0,
    warning: LIGHT_STATUS.1,
    danger: LIGHT_STATUS.2,
    danger_text: LIGHT_STATUS.3,
    offline: rgb(0x8a8378),
    shadow: Color32::from_black_alpha(40),
};

pub const MIST: Palette = Palette {
    id: "mist",
    name: "Mist",
    dark: false,
    rail: rgb(0xd9e0e8),
    side: rgb(0xe9eef3),
    main: rgb(0xf6f8fb),
    field: rgb(0xe1e7ee),
    composer: rgb(0xe6ebf1),
    raised: Color32::WHITE,
    hover: rgb(0xdde4ec),
    hover_soft: rgb(0xeef2f6),
    selected: rgb(0xd0d9e3),
    button: rgb(0xdae1e9),
    button_hover: rgb(0xcdd6e0),
    button_pressed: rgb(0xbdc8d4),
    border: rgb(0xd2dae3),
    text: rgb(0x243041),
    strong: rgb(0x0c1420),
    muted: rgb(0x52627a),
    success: LIGHT_STATUS.0,
    warning: LIGHT_STATUS.1,
    danger: LIGHT_STATUS.2,
    danger_text: LIGHT_STATUS.3,
    offline: rgb(0x7f8a99),
    shadow: Color32::from_black_alpha(40),
};

pub const DARK_THEMES: [&Palette; 3] = [&MIDNIGHT, &GRAPHITE, &DEEP_SEA];
pub const LIGHT_THEMES: [&Palette; 3] = [&DAYLIGHT, &PAPER, &MIST];

/// RustTalk's brand orange, from the logo and the website.
pub const RUST: Accent = Accent {
    id: "rust",
    name: "Rust",
    base: rgb(0xb7410e),
    strong: rgb(0x8c3008),
    tint: rgb(0xf08a4b),
};

pub const ACCENTS: [&Accent; 6] = [
    &RUST,
    &Accent {
        id: "blue",
        name: "Blue",
        base: rgb(0x2b63c6),
        strong: rgb(0x214f9f),
        tint: rgb(0x7eaaf5),
    },
    &Accent {
        id: "green",
        name: "Green",
        base: rgb(0x1d7a45),
        strong: rgb(0x165e35),
        tint: rgb(0x5fcf8e),
    },
    &Accent {
        id: "violet",
        name: "Violet",
        base: rgb(0x7048c8),
        strong: rgb(0x5734a3),
        tint: rgb(0xb59af5),
    },
    &Accent {
        id: "rose",
        name: "Rose",
        base: rgb(0xbf2f6d),
        strong: rgb(0x982457),
        tint: rgb(0xf28ab6),
    },
    &Accent {
        id: "teal",
        name: "Teal",
        base: rgb(0x0c706e),
        strong: rgb(0x0a5e5c),
        tint: rgb(0x52d0c6),
    },
];

/// Dark, light, or whatever the system uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Dark,
    Light,
    System,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Dark, Mode::Light, Mode::System];

    pub fn id(self) -> &'static str {
        match self {
            Mode::Dark => "dark",
            Mode::Light => "light",
            Mode::System => "system",
        }
    }

    pub fn from_id(id: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|mode| mode.id() == id)
            .unwrap_or(Mode::System)
    }

    pub fn preference(self) -> ThemePreference {
        match self {
            Mode::Dark => ThemePreference::Dark,
            Mode::Light => ThemePreference::Light,
            Mode::System => ThemePreference::System,
        }
    }
}

/// The person's look: mode, a dark and a light theme, and the accent.
#[derive(Clone, Copy, PartialEq)]
pub struct Appearance {
    pub mode: Mode,
    pub dark: &'static Palette,
    pub light: &'static Palette,
    pub accent: &'static Accent,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            mode: Mode::System,
            dark: DARK_THEMES[0],
            light: LIGHT_THEMES[0],
            accent: &RUST,
        }
    }
}

impl Palette {
    /// The theme's name in the app's language.
    pub fn label(&self) -> &'static str {
        match self.id {
            "midnight" => t!("theme.midnight"),
            "graphite" => t!("theme.graphite"),
            "deep-sea" => t!("theme.deep_sea"),
            "daylight" => t!("theme.daylight"),
            "paper" => t!("theme.paper"),
            "mist" => t!("theme.mist"),
            _ => self.name,
        }
    }
}

impl PartialEq for Palette {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl PartialEq for Accent {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Appearance {
    /// From the stored IDs; unknown ones fall back to the defaults.
    pub fn from_ids(mode: &str, dark: &str, light: &str, accent: &str) -> Self {
        let default = Self::default();
        let find = |list: [&'static Palette; 3], id: &str, fallback| {
            list.into_iter().find(|p| p.id == id).unwrap_or(fallback)
        };
        Self {
            mode: Mode::from_id(mode),
            dark: find(DARK_THEMES, dark, default.dark),
            light: find(LIGHT_THEMES, light, default.light),
            accent: ACCENTS
                .into_iter()
                .find(|a| a.id == accent)
                .unwrap_or(default.accent),
        }
    }

    pub fn palette(&self, dark: bool) -> &'static Palette {
        if dark { self.dark } else { self.light }
    }
}

/// WCAG relative luminance.
fn luminance(color: Color32) -> f64 {
    let channel = |c: u8| {
        let c = f64::from(c) / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
}

pub fn contrast(a: Color32, b: Color32) -> f64 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_theme_meets_wcag_aa_for_text() {
        for p in DARK_THEMES.into_iter().chain(LIGHT_THEMES) {
            let surfaces = [
                p.rail, p.side, p.main, p.field, p.composer, p.raised, p.hover, p.selected,
            ];
            for surface in surfaces {
                assert!(contrast(p.text, surface) >= 4.5, "{} text", p.id);
                assert!(contrast(p.strong, surface) >= 4.5, "{} strong", p.id);
                assert!(contrast(p.muted, surface) >= 3.0, "{} muted", p.id);
                assert!(contrast(p.danger_text, surface) >= 3.0, "{} danger", p.id);
            }
            assert!(contrast(p.muted, p.main) >= 4.5, "{} muted on main", p.id);
            assert!(contrast(p.text, p.button) >= 4.5, "{} button", p.id);
            assert!(
                contrast(p.strong, p.button_pressed) >= 4.5,
                "{} pressed",
                p.id
            );
            assert!(contrast(p.success, p.field) >= 3.0, "{} success", p.id);
            assert!(
                contrast(Color32::WHITE, p.danger) >= 3.0,
                "{} on danger",
                p.id
            );
        }
    }

    #[test]
    fn accents_stay_readable_in_every_theme() {
        for accent in ACCENTS {
            assert!(
                contrast(Color32::WHITE, accent.base) >= 4.5,
                "{} fill",
                accent.id
            );
            assert!(
                contrast(Color32::WHITE, accent.strong) >= 4.5,
                "{} hover",
                accent.id
            );
            for p in DARK_THEMES.into_iter().chain(LIGHT_THEMES) {
                let (text, background) = accent.mention_colors(p);
                assert!(
                    contrast(text, background) >= 4.5,
                    "{} mention on {}",
                    accent.id,
                    p.id
                );
                for surface in [p.side, p.main, p.raised] {
                    let text = accent.text(p.dark);
                    assert!(contrast(text, surface) >= 4.5, "{} on {}", accent.id, p.id);
                }
            }
        }
    }

    #[test]
    fn appearance_falls_back_to_defaults() {
        let saved = Appearance::from_ids("light", "deep-sea", "paper", "violet");
        assert_eq!(saved.mode, Mode::Light);
        assert_eq!(saved.dark.id, "deep-sea");
        assert_eq!(saved.light.id, "paper");
        assert_eq!(saved.accent.id, "violet");
        let unknown = Appearance::from_ids("sepia", "x", "y", "z");
        assert!(unknown == Appearance::default());
        assert_eq!(Appearance::default().accent.base, rgb(0xb7410e));
        assert_eq!(Appearance::default().mode, Mode::System);
    }
}
