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
        .then_some(Self { submap, binds })
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

    fn matches(&self, code: u32, syms: &[xkb::Keysym], mods: u8) -> bool {
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
            // Older IPC omits the ignore-mods flag. A reported trigger may match any modifier mask.
            if bind.ignore_mods == Some(false) && bind.modmask != mods {
                return false;
            }
            if bind.catch_all {
                return true;
            }
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

    fn blocks(&self, state: &xkb::State, evdev: u32) -> bool {
        let code = evdev + 8;
        let key = code.into();
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
        let effective = mods(xkb::STATE_MODS_EFFECTIVE);
        let depressed = mods(xkb::STATE_MODS_DEPRESSED | xkb::STATE_MODS_LATCHED);
        let keymap = state.get_keymap();
        let mut syms = state.key_get_syms(key).to_vec();
        for layout in [0, state.key_get_layout(key)] {
            syms.extend(keymap.key_get_syms_by_level(key, layout, 0));
        }
        let logo = effective & 64 != 0
            || ["Super", "Logo", "Hyper", "Meta"]
                .iter()
                .any(|name| active(name, xkb::STATE_MODS_EFFECTIVE))
            || syms.iter().any(|sym| matches!(sym.raw(), 0xffeb..=0xffee));
        let system = effective & (4 | 8) == (4 | 8)
            && syms
                .iter()
                .any(|sym| matches!(sym.raw(), 0xffff | 0xffbe..=0xffc9 | 0x1008fe01..=0x1008fe0c));
        logo || system
            || self.matches(code, &syms, effective)
            || self.matches(code, &syms, depressed)
    }

    pub(super) fn allows(
        &self,
        state: &xkb::State,
        held: &std::collections::BTreeSet<u32>,
        event: Option<(u32, bool)>,
    ) -> bool {
        if held.iter().any(|code| self.blocks(state, *code)) {
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
        !self.blocks(&next, code) && held.iter().all(|held| !self.blocks(&next, *held))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn keymap(options: Option<&str>) -> xkb::Keymap {
        xkb::Keymap::new_from_names(
            &xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            "evdev",
            "pc105",
            "us",
            "",
            options.map(str::to_owned),
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap()
    }

    #[test]
    fn caps_remapped_to_super_is_blocked() {
        let state = xkb::State::new(&keymap(Some("caps:super")));
        let binds = Binds::new("default".into(), vec![]).unwrap();
        assert!(!binds.allows(&state, &BTreeSet::new(), Some((58, true))));
        let state = xkb::State::new(&keymap(None));
        assert!(binds.allows(&state, &BTreeSet::new(), Some((58, true))));
    }

    #[test]
    fn effective_modifiers_keysyms_keycodes_and_submaps_match_live_binds() {
        let binds = || serde_json::from_str(include_str!("fixtures/binds.json")).unwrap();
        let active = Binds::new("default".into(), binds()).unwrap();
        let mut state = xkb::State::new(&keymap(None));
        assert!(active.allows(&state, &BTreeSet::new(), Some((18, true))));
        for code in [29_u32, 42] {
            state.update_key((code + 8).into(), xkb::KeyDirection::Down);
        }
        let held = [29, 42].into();
        assert!(!active.allows(&state, &held, Some((18, true))));
        assert!(active.allows(&state, &held, Some((19, true))));
        let resize = Binds::new("resize".into(), binds()).unwrap();
        assert!(resize.allows(&state, &held, Some((18, true))));
        assert!(!resize.allows(&state, &held, Some((19, true))));
        let state = xkb::State::new(&keymap(None));
        assert!(!resize.allows(&state, &BTreeSet::new(), Some((48, true))));
    }
    #[test]
    fn newly_added_bind_invalidates_an_already_held_combo() {
        let mut state = xkb::State::new(&keymap(None));
        let held = [29_u32, 42, 18].into_iter().collect();
        for code in [29_u32, 42, 18] {
            state.update_key((code + 8).into(), xkb::KeyDirection::Down);
        }
        assert!(
            Binds::new("default".into(), vec![])
                .unwrap()
                .allows(&state, &held, None)
        );
        let binds = serde_json::from_str(include_str!("fixtures/binds.json")).unwrap();
        assert!(
            !Binds::new("default".into(), binds)
                .unwrap()
                .allows(&state, &held, None)
        );
    }
    #[test]
    fn a_display_key_fills_an_opaque_json_bind() {
        let opaque = r#"[{"modmask":64,"key":"","keycode":0,"submap":"","submap_universal":"false","catch_all":false}]"#;
        let mut binds =
            Binds::new("default".into(), serde_json::from_str(opaque).unwrap()).unwrap();
        assert!(binds.needs_display_keys());
        let reply = "bindd\n\tmodmask: 64\n\tsubmap: \n\tkey: SUPER + code:10\n\tkeycode: 0\n\tcatchall: false\n";
        assert!(binds.fill_display_keys(&reply.replace("64", "4")).is_none());
        binds.fill_display_keys(reply).unwrap();
        assert!(!binds.needs_display_keys());
        let state = xkb::State::new(&keymap(None));
        assert!(!binds.allows(&state, &BTreeSet::new(), Some((2, true))));
        assert!(binds.allows(&state, &BTreeSet::new(), Some((3, true))));
    }
}
