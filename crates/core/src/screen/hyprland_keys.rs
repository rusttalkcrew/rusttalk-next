//! Match Hyprland's bind data against the virtual keyboard's XKB state.

use serde::Deserialize;
use xkbcommon::xkb;

#[derive(Deserialize)]
#[serde(untagged)]
enum Flag {
    Bool(bool),
    Text(String),
}

impl Flag {
    fn get(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            Self::Text(value) => match value.as_str() {
                "true" | "1" => Some(true),
                "false" | "0" => Some(false),
                _ => None,
            },
        }
    }
}

#[derive(Deserialize)]
struct Bind {
    modmask: u8,
    key: String,
    keycode: u32,
    submap: String,
    submap_universal: Flag,
    catch_all: bool,
    #[serde(default, alias = "ignoreMods")]
    ignore_mods: Option<bool>,
}

impl Bind {
    fn missing_key(&self) -> bool {
        self.keycode == 0 && self.key.is_empty() && !self.catch_all
    }
}

pub(super) struct Binds {
    submap: String,
    binds: Vec<Bind>,
    /// Hyprland resolves keysym binds with its own keymap, not the device's.
    resolve: Option<xkb::State>,
}

/// The global `input:kb_*` options Hyprland builds its bind translation keymap from.
#[derive(Clone, PartialEq)]
pub(super) struct Layout([String; 5]);

impl Layout {
    /// Reads `j/getoption` replies for rules, model, layout, variant, options and file.
    pub(super) fn parse(
        fields: &mut serde_json::Deserializer<serde_json::de::StrRead<'_>>,
    ) -> Option<Self> {
        #[derive(Deserialize)]
        struct Reply {
            option: String,
            str: String,
        }
        let mut names: [String; 5] = Default::default();
        for (index, option) in ["rules", "model", "layout", "variant", "options", "file"]
            .iter()
            .enumerate()
        {
            let reply = Reply::deserialize(&mut *fields).ok()?;
            let value = (!reply.str.is_empty() && reply.str != "[[EMPTY]]").then_some(reply.str);
            if reply.option != format!("input:kb_{option}") {
                return None;
            }
            match (names.get_mut(index), value) {
                (Some(name), Some(value)) => *name = value,
                (Some(_), None) => (),
                // A keymap file is read relative to Hyprland's config; it can't be evaluated here.
                (None, value) => value.is_none().then_some(())?,
            }
        }
        Some(Self(names))
    }

    /// Hyprland falls back to other rules if these fail to compile; keys are blocked instead.
    pub(super) fn keymap(&self) -> Option<xkb::Keymap> {
        let [rules, model, layout, variant, options] = &self.0;
        xkb::Keymap::new_from_names(
            &xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            rules,
            model,
            layout,
            variant,
            Some(options.clone()),
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
    }
}

/// Hyprland's modifier mask bits, from depressed and latched or effective XKB modifiers.
fn modmasks(state: &xkb::State) -> [u8; 2] {
    let active = |name: &str, flags| state.mod_name_is_active(name, flags);
    let mods = |flags| {
        [
            "Shift", "Lock", "Control", "Mod1", "Mod2", "Mod3", "Mod4", "Mod5",
        ]
        .iter()
        .enumerate()
        .fold(0, |mask, (bit, name)| {
            mask | (u8::from(active(name, flags)) << bit)
        })
    };
    [
        mods(xkb::STATE_MODS_EFFECTIVE),
        mods(xkb::STATE_MODS_DEPRESSED | xkb::STATE_MODS_LATCHED),
    ]
}

/// Keysyms the compositor acts on before binds: VT switches and the server-private range.
fn compositor_sym(sym: xkb::Keysym) -> bool {
    matches!(sym.raw(), 0xfed5 | 0x1008fe01..=0x1008fe2f)
}

impl Binds {
    pub(super) fn parse(
        fields: &mut serde_json::Deserializer<serde_json::de::StrRead<'_>>,
    ) -> Option<Self> {
        Self::new(
            String::deserialize(&mut *fields).ok()?,
            Vec::<Bind>::deserialize(&mut *fields).ok()?,
        )
    }

    fn new(submap: String, binds: Vec<Bind>) -> Option<Self> {
        (!submap.is_empty()
            && binds
                .iter()
                .all(|bind| bind.submap_universal.get().is_some() && !bind.key.contains('\0')))
        .then_some(Self {
            submap,
            binds,
            resolve: None,
        })
    }

    pub(super) fn resolve_with(&mut self, keymap: &xkb::Keymap) {
        self.resolve = Some(xkb::State::new(keymap));
    }

    pub(super) fn needs_display_keys(&self) -> bool {
        self.binds.iter().any(Bind::missing_key)
    }

    /// Some releases omit Lua chord triggers in JSON but include them in the text reply.
    /// Match rows to the JSON metadata before using a display trigger.
    pub(super) fn fill_display_keys(&mut self, text: &str) -> Option<()> {
        let mut rows = text.split("\n\tmodmask: ").skip(1);
        for bind in &mut self.binds {
            let mut row = rows.next()?.lines();
            let mask = row.next()?.parse::<u8>().ok()?;
            let submap = row.next()?.strip_prefix("\tsubmap: ")?;
            let display = row.next()?.strip_prefix("\tkey: ")?;
            let code = row
                .next()?
                .strip_prefix("\tkeycode: ")?
                .parse::<u32>()
                .ok()?;
            let catch_all = match row.next()?.strip_prefix("\tcatchall: ")? {
                "true" | "1" => true,
                "false" | "0" => false,
                _ => return None,
            };
            if (mask, submap, code, catch_all)
                != (
                    bind.modmask,
                    bind.submap.as_str(),
                    bind.keycode,
                    bind.catch_all,
                )
                || (!bind.missing_key() && !bind.key.is_empty() && bind.key != display)
            {
                return None;
            }
            if bind.missing_key() {
                let key = display.rsplit(" + ").next()?.trim();
                if let Some(code) = key.strip_prefix("code:") {
                    bind.keycode = code.parse().ok().filter(|code| *code != 0)?;
                } else {
                    if key.contains('\0') || key.is_empty() {
                        return None;
                    }
                    bind.key = key.into();
                }
            }
        }
        rows.next().is_none().then_some(())
    }

    /// Catch-all binds match any trigger, including pointer buttons and scrolls.
    fn matches(&self, mods: u8, trigger: impl Fn(&Bind) -> bool) -> bool {
        self.binds.iter().any(|bind| {
            if bind.submap_universal.get() != Some(true)
                && (if bind.submap.is_empty() {
                    "default"
                } else {
                    &bind.submap
                }) != self.submap
            {
                return false;
            }
            // Hyprland's JSON omits the ignore-mods flag. Binds need exact modifiers unless
            // explicitly flagged (Hyprland's default), so a flagged bind can't be detected then.
            if bind.ignore_mods != Some(true) && bind.modmask != mods {
                return false;
            }
            bind.catch_all || trigger(bind)
        })
    }

    fn matches_key(&self, code: u32, syms: &[xkb::Keysym], mods: u8) -> bool {
        self.matches(mods, |bind| {
            if bind.keycode != 0 {
                return bind.keycode == code;
            }
            let sym = xkb::keysym_from_name(&bind.key, xkb::KEYSYM_CASE_INSENSITIVE);
            if sym.raw() == 0
                && (bind.missing_key() || bind.key.contains('&') || bind.key.contains('+'))
            {
                return true;
            }
            sym.raw() != 0
                && syms.iter().any(|key| {
                    key == &sym || xkb::keysym_get_name(*key).eq_ignore_ascii_case(&bind.key)
                })
        })
    }

    /// Pointer triggers are Hyprland event names such as `mouse:272` or `mouse_down`.
    fn blocks_pointer(&self, state: &xkb::State, trigger: &str) -> bool {
        modmasks(state).into_iter().any(|mods| {
            self.matches(mods, |bind| {
                bind.missing_key()
                    || bind
                        .key
                        .split('&')
                        .any(|key| key.trim().eq_ignore_ascii_case(trigger))
            })
        })
    }

    fn blocks(&self, state: &xkb::State, evdev: u32) -> bool {
        let code = evdev + 8;
        let key = code.into();
        // Without the compositor's translation keymap, a bind could match an unseen keysym.
        let Some(resolve) = &self.resolve else {
            return true;
        };
        let [effective, depressed] = modmasks(state);
        let keymap = state.get_keymap();
        let mut syms = state.key_get_syms(key).to_vec();
        if syms.iter().any(|sym| compositor_sym(*sym)) {
            return true;
        }
        for layout in [0, state.key_get_layout(key)] {
            syms.extend(keymap.key_get_syms_by_level(key, layout, 0));
        }
        syms.push(resolve.key_get_one_sym(key));
        let logo = effective & 64 != 0
            || ["Super", "Logo", "Hyper", "Meta"]
                .iter()
                .any(|name| state.mod_name_is_active(name, xkb::STATE_MODS_EFFECTIVE))
            || syms.iter().any(|sym| matches!(sym.raw(), 0xffeb..=0xffee));
        let system = effective & (4 | 8) == (4 | 8)
            && syms
                .iter()
                .any(|sym| matches!(sym.raw(), 0xffff | 0xffbe..=0xffc9));
        logo || system
            || self.matches_key(code, &syms, effective)
            || self.matches_key(code, &syms, depressed)
    }

    /// Checks a pointer trigger and held buttons against binds under the current modifiers.
    pub(super) fn allows_pointer(&self, state: &xkb::State, triggers: &[String]) -> bool {
        !triggers
            .iter()
            .any(|trigger| self.blocks_pointer(state, trigger))
    }

    /// `buttons` are the held pointer triggers a modifier change could turn into a bind.
    pub(super) fn allows(
        &self,
        state: &xkb::State,
        held: &std::collections::BTreeSet<u32>,
        buttons: &[String],
        event: Option<(u32, bool)>,
    ) -> bool {
        if held.iter().any(|code| self.blocks(state, *code)) || !self.allows_pointer(state, buttons)
        {
            return false;
        }
        let Some((code, down)) = event else {
            return true;
        };
        if self.blocks(state, code) {
            return false;
        }
        if !down || held.contains(&code) {
            return true;
        }
        // A new modifier may itself be unsafe, or complete a bind with a key already held.
        let mut next = xkb::State::new(&state.get_keymap());
        next.update_mask(
            state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
            state.serialize_mods(xkb::STATE_MODS_LATCHED),
            state.serialize_mods(xkb::STATE_MODS_LOCKED),
            state.serialize_layout(xkb::STATE_LAYOUT_DEPRESSED),
            state.serialize_layout(xkb::STATE_LAYOUT_LATCHED),
            state.serialize_layout(xkb::STATE_LAYOUT_LOCKED),
        );
        next.update_key((code + 8).into(), xkb::KeyDirection::Down);
        !self.blocks(&next, code)
            && held.iter().all(|held| !self.blocks(&next, *held))
            && self.allows_pointer(&next, buttons)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn keymap(options: Option<&str>) -> xkb::Keymap {
        names("us", "", options)
    }

    fn names(layout: &str, variant: &str, options: Option<&str>) -> xkb::Keymap {
        xkb::Keymap::new_from_names(
            &xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            "evdev",
            "pc105",
            layout,
            variant,
            options.map(str::to_owned),
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap()
    }

    /// Binds resolved the way Hyprland does with a global US layout.
    fn binds(submap: &str, json: &str) -> Binds {
        let mut binds = Binds::new(submap.into(), serde_json::from_str(json).unwrap()).unwrap();
        binds.resolve_with(&keymap(None));
        binds
    }

    fn fixture(submap: &str) -> Binds {
        binds(submap, include_str!("fixtures/binds.json"))
    }

    fn pressed(keymap: &xkb::Keymap, codes: &[u32]) -> xkb::State {
        let mut state = xkb::State::new(keymap);
        for code in codes {
            state.update_key((code + 8).into(), xkb::KeyDirection::Down);
        }
        state
    }

    fn bind(modmask: u8, key: &str) -> String {
        format!(
            r#"[{{"modmask":{modmask},"key":"{key}","keycode":0,"submap":"","submap_universal":"false","catch_all":false,"ignore_mods":false}}]"#
        )
    }

    #[test]
    fn caps_remapped_to_super_is_blocked() {
        let state = xkb::State::new(&keymap(Some("caps:super")));
        let binds = binds("default", "[]");
        assert!(!binds.allows(&state, &BTreeSet::new(), &[], Some((58, true))));
        let state = xkb::State::new(&keymap(None));
        assert!(binds.allows(&state, &BTreeSet::new(), &[], Some((58, true))));
    }

    #[test]
    fn effective_modifiers_keysyms_keycodes_and_submaps_match_live_binds() {
        let active = fixture("default");
        let state = xkb::State::new(&keymap(None));
        assert!(active.allows(&state, &BTreeSet::new(), &[], Some((18, true))));
        let state = pressed(&keymap(None), &[29, 42]);
        let held = [29, 42].into();
        assert!(!active.allows(&state, &held, &[], Some((18, true))));
        assert!(active.allows(&state, &held, &[], Some((19, true))));
        let resize = fixture("resize");
        assert!(resize.allows(&state, &held, &[], Some((18, true))));
        assert!(!resize.allows(&state, &held, &[], Some((19, true))));
        // A universal bind without the ignore-mods flag needs its exact modifiers.
        let state = xkb::State::new(&keymap(None));
        assert!(resize.allows(&state, &BTreeSet::new(), &[], Some((48, true))));
        let flagged = include_str!("fixtures/binds.json").replace(
            r#""catch_all": false}"#,
            r#""catch_all": false, "ignore_mods": true}"#,
        );
        assert!(!binds("resize", &flagged).allows(&state, &BTreeSet::new(), &[], Some((48, true))));
    }

    #[test]
    fn newly_added_bind_invalidates_an_already_held_combo() {
        let state = pressed(&keymap(None), &[29, 42, 18]);
        let held = [29_u32, 42, 18].into_iter().collect();
        assert!(binds("default", "[]").allows(&state, &held, &[], None));
        assert!(!fixture("default").allows(&state, &held, &[], None));
    }

    #[test]
    fn a_display_key_fills_an_opaque_json_bind() {
        let opaque = r#"[{"modmask":4,"key":"","keycode":0,"submap":"","submap_universal":"false","catch_all":false}]"#;
        let mut binds = binds("default", opaque);
        assert!(binds.needs_display_keys());
        let reply = "bindd\n\tmodmask: 4\n\tsubmap: \n\tkey: CTRL + code:10\n\tkeycode: 0\n\tcatchall: false\n";
        assert!(
            binds
                .fill_display_keys(&reply.replace("modmask: 4", "modmask: 5"))
                .is_none()
        );
        binds.fill_display_keys(reply).unwrap();
        assert!(!binds.needs_display_keys());
        let state = pressed(&keymap(None), &[29]);
        assert!(!binds.allows(&state, &[29].into(), &[], Some((2, true))));
        assert!(binds.allows(
            &xkb::State::new(&keymap(None)),
            &BTreeSet::new(),
            &[],
            Some((2, true))
        ));
        assert!(binds.allows(&state, &[29].into(), &[], Some((3, true))));
    }

    #[test]
    fn pointer_binds_block_clicks_scrolls_and_modifier_drags() {
        let us = keymap(None);
        let idle = xkb::State::new(&us);
        let ctrl = pressed(&us, &[29]);
        let left = ["mouse:272".to_owned()];
        let right = ["mouse:273".to_owned()];
        let wheel = ["mouse_up".to_owned(), "mouse_down".to_owned()];

        // bind = , mouse:272, exec, ...
        let exec = binds("default", &bind(0, "mouse:272"));
        assert!(!exec.allows_pointer(&idle, &left));
        assert!(exec.allows_pointer(&idle, &right));
        assert!(exec.allows_pointer(&ctrl, &left));

        // bind = CTRL, mouse_down, ...
        let scroll = binds("default", &bind(4, "mouse_down"));
        assert!(scroll.allows_pointer(&idle, &wheel));
        assert!(!scroll.allows_pointer(&ctrl, &wheel));

        // bindm = CTRL, mouse:272, movewindow: the press, a modifier added mid-drag, and
        // held-button revalidation all block. (Alt is already blocked as Meta.)
        let drag = binds("default", &bind(4, "mouse:272"));
        assert!(drag.allows_pointer(&idle, &left));
        assert!(!drag.allows_pointer(&ctrl, &left));
        assert!(drag.allows(&idle, &BTreeSet::new(), &[], Some((29, true))));
        assert!(!drag.allows(&idle, &BTreeSet::new(), &left, Some((29, true))));
        assert!(!drag.allows(&ctrl, &[29].into(), &left, None));

        // Hyprland's JSON has no ignore-mods flag: SUPER + mouse:272 (bindm movewindow)
        // must leave plain clicks alone, while Super itself stays blocked for keys.
        let unflagged =
            |modmask: u8, key: &str| bind(modmask, key).replace(r#","ignore_mods":false"#, "");
        let window = binds("default", &unflagged(64, "mouse:272"));
        assert!(window.allows_pointer(&idle, &left));
        assert!(!window.allows_pointer(&pressed(&us, &[125]), &left));
        assert!(!window.allows(&idle, &BTreeSet::new(), &[], Some((125, true))));
        assert!(!binds("default", &unflagged(0, "mouse:272")).allows_pointer(&idle, &left));
        let scroll = binds("default", &unflagged(4, "mouse_down"));
        assert!(scroll.allows_pointer(&idle, &wheel));
        assert!(!scroll.allows_pointer(&ctrl, &wheel));

        let catch_all = r#"[{"modmask":0,"key":"","keycode":0,"submap":"","submap_universal":"false","catch_all":true,"ignore_mods":false}]"#;
        assert!(!binds("default", catch_all).allows_pointer(&idle, &right));
        assert!(binds("resize", catch_all).allows_pointer(&idle, &right));
    }

    #[test]
    fn binds_resolve_with_the_compositor_layout_too() {
        // Global US layout, Dvorak device keymap: physical R types `p` but Hyprland sees `r`.
        let dvorak = names("us", "dvorak", None);
        let state = xkb::State::new(&dvorak);
        let mut binds = Binds::new(
            "default".into(),
            serde_json::from_str(&bind(0, "r")).unwrap(),
        )
        .unwrap();
        assert!(!binds.allows(&state, &BTreeSet::new(), &[], Some((30, true))));
        binds.resolve_with(&dvorak);
        assert!(binds.allows(&state, &BTreeSet::new(), &[], Some((19, true))));
        binds.resolve_with(&keymap(None));
        assert!(!binds.allows(&state, &BTreeSet::new(), &[], Some((19, true))));
        // Dvorak's own `r` key stays blocked as well.
        assert!(!binds.allows(&state, &BTreeSet::new(), &[], Some((24, true))));
        assert!(binds.allows(&state, &BTreeSet::new(), &[], Some((30, true))));
    }

    #[test]
    fn vt_switch_keysyms_are_blocked_without_ctrl_alt() {
        let remapped = xkb::Keymap::new_from_string(
            &xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            r#"xkb_keymap {
                xkb_keycodes { minimum = 8; maximum = 255; <FK01> = 67; <DELE> = 119; };
                xkb_types { include "complete" };
                xkb_compat { include "complete" };
                xkb_symbols { key <FK01> { [ XF86Switch_VT_2 ] }; key <DELE> { [ Delete ] }; };
            };"#
            .into(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        let binds = binds("default", "[]");
        let none = BTreeSet::new();
        assert!(!binds.allows(&xkb::State::new(&remapped), &none, &[], Some((59, true))));
        assert!(binds.allows(&xkb::State::new(&remapped), &none, &[], Some((111, true))));
        let us = keymap(None);
        assert!(binds.allows(&xkb::State::new(&us), &none, &[], Some((59, true))));
        assert!(!binds.allows(
            &pressed(&us, &[29, 56]),
            &[29, 56].into(),
            &[],
            Some((59, true))
        ));
    }
}
