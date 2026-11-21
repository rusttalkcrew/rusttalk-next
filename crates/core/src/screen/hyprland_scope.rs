//! Interprets read-only Hyprland IPC snapshots independently of Wayland.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::keys::{Binds, Layout};

use crate::control::{Input, scan_code};

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(super) struct Monitor {
    id: i64,
    name: String,
    x: f64,
    y: f64,
    width: u32,
    height: u32,
    scale: f64,
    transform: u8,
    dpms_status: bool,
    disabled: bool,
    mirror_of: String,
}

impl Monitor {
    pub(super) fn find(monitors: &str, name: &str) -> anyhow::Result<Self> {
        let monitors: Vec<Self> = serde_json::from_str(monitors)?;
        Self::select(&monitors, name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("shared output is missing, overlapping or unavailable"))
    }

    fn select<'a>(monitors: &'a [Self], name: &str) -> Option<&'a Self> {
        let mut matching = monitors.iter().filter(|m| m.name == name);
        matching.next().filter(|monitor| {
            monitor.usable()
                && matching.next().is_none()
                && !monitors
                    .iter()
                    .any(|other| other.name != name && other.usable() && monitor.overlaps(other))
        })
    }

    fn usable(&self) -> bool {
        !self.disabled
            && self.dpms_status
            && self.mirror_of == "none"
            && self.width > 0
            && self.height > 0
            && self.scale > 0.0
            && self.transform < 8
    }

    fn bounds(&self) -> (std::ops::Range<f64>, std::ops::Range<f64>) {
        let (mut width, mut height) = (self.width, self.height);
        if self.transform % 2 == 1 {
            std::mem::swap(&mut width, &mut height);
        }
        (
            self.x..self.x + (f64::from(width) / self.scale).round(),
            self.y..self.y + (f64::from(height) / self.scale).round(),
        )
    }

    fn contains(&self, cursor: Cursor) -> bool {
        let (x, y) = self.bounds();
        x.contains(&cursor.x) && y.contains(&cursor.y)
    }

    fn overlaps(&self, other: &Self) -> bool {
        let (x, y) = self.bounds();
        let (other_x, other_y) = other.bounds();
        x.start < other_x.end
            && other_x.start < x.end
            && y.start < other_y.end
            && other_y.start < y.end
    }
}

#[derive(Deserialize)]
struct Cursor {
    x: f64,
    y: f64,
}

#[derive(Deserialize)]
struct Focus {
    address: String,
    monitor: i64,
    mapped: bool,
    hidden: bool,
}

#[derive(Deserialize)]
struct Lock {
    locked: bool,
}

#[derive(Deserialize)]
struct Layer {
    address: String,
    #[serde(default, alias = "interactivity")]
    keyboard_interactivity: Option<u8>,
}

struct OutputLayers {
    levels: [Vec<Layer>; 4],
}

impl<'de> Deserialize<'de> for OutputLayers {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Levels {
            #[serde(rename = "0")]
            background: Vec<Layer>,
            #[serde(rename = "1")]
            bottom: Vec<Layer>,
            #[serde(rename = "2")]
            top: Vec<Layer>,
            #[serde(rename = "3")]
            overlay: Vec<Layer>,
        }
        #[derive(Deserialize)]
        struct Reply {
            levels: Levels,
        }
        let levels = Reply::deserialize(deserializer)?.levels;
        Ok(Self {
            levels: [levels.background, levels.bottom, levels.top, levels.overlay],
        })
    }
}

pub(super) enum Scope {
    Move,
    Pointer,
    Keyboard,
}

pub(super) struct Verified {
    pub focus: Option<String>,
    pub binds: Option<Binds>,
    pub layout: Option<Layout>,
}

/// A missing or changed output, malformed snapshot or locked desktop blocks input.
pub(super) fn verify(snapshot: &str, target: &Monitor, scope: Scope) -> Option<Verified> {
    // The batch results are distinct JSON values with arbitrary whitespace.
    let mut fields = serde_json::Deserializer::from_str(snapshot);
    let monitors = Vec::<Monitor>::deserialize(&mut fields).ok()?;
    let monitor = Monitor::select(&monitors, &target.name)?;
    if monitor != target || Lock::deserialize(&mut fields).ok()?.locked {
        return None;
    }
    let mut verified = Verified {
        focus: None,
        binds: None,
        layout: None,
    };
    if matches!(scope, Scope::Move) {
        return Some(verified);
    }
    if !target.contains(Cursor::deserialize(&mut fields).ok()?) {
        return None;
    }
    if !matches!(scope, Scope::Keyboard) {
        // Buttons and scrolls can trigger compositor binds too.
        verified.binds = Some(Binds::parse(&mut fields)?);
        return Some(verified);
    }
    let focus = Focus::deserialize(&mut fields).ok()?;
    if !focus.mapped
        || focus.hidden
        || focus.monitor != target.id
        || focus
            .address
            .strip_prefix("0x")
            .and_then(|address| u64::from_str_radix(address, 16).ok())
            .is_none_or(|address| address == 0)
    {
        return None;
    }

    // activewindow can stay set while a layer surface owns keyboard focus.
    let outputs = BTreeMap::<String, OutputLayers>::deserialize(&mut fields).ok()?;
    let binds = Binds::parse(&mut fields)?;
    let layout = Layout::parse(&mut fields)?;
    let modes = BTreeMap::<String, u8>::deserialize(&mut fields).unwrap_or_default();
    if monitors
        .iter()
        .any(|monitor| !outputs.contains_key(&monitor.name))
        || outputs.into_iter().any(|(name, output)| {
            output.levels.iter().enumerate().any(|(level, layers)| {
                (name != target.name || level >= 2)
                    && layers.iter().any(|layer| {
                        layer
                            .keyboard_interactivity
                            .or_else(|| modes.get(&layer.address).copied())
                            != Some(0)
                    })
            })
        })
    {
        return None;
    }
    verified.focus = Some(focus.address);
    verified.binds = Some(binds);
    verified.layout = Some(layout);
    Some(verified)
}

impl From<&Input> for Scope {
    fn from(input: &Input) -> Self {
        match input {
            Input::Move { .. } => Self::Move,
            Input::Key { .. } => Self::Keyboard,
            _ => Self::Pointer,
        }
    }
}

/// Set 1 and evdev match for these ordinary keys. Extended keys use evdev codes.
pub(super) fn evdev(key: &str) -> Option<u32> {
    Some(match scan_code(key)? {
        0xe01d => 97,
        0xe038 => 100,
        0xe047 => 102,
        0xe048 => 103,
        0xe049 => 104,
        0xe04b => 105,
        0xe04d => 106,
        0xe04f => 107,
        0xe050 => 108,
        0xe051 => 109,
        0xe052 => 110,
        0xe053 => 111,
        0xe05d => 127,
        code @ 0..=0x58 => u32::from(code),
        _ => return None,
    })
}

pub(super) fn blocked_combo(code: u32, held: &std::collections::BTreeSet<u32>) -> bool {
    let down = |key| code == key || held.contains(&key);
    (down(29) || down(97))
        && (down(56) || down(100))
        && (down(111) || (59..=68).any(down) || down(87) || down(88))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::Button;

    const MONITORS: &str = include_str!("fixtures/monitors.json");
    const CURSOR: &str = include_str!("fixtures/cursorpos.json");
    const FOCUS: &str = include_str!("fixtures/activewindow.json");
    const LOCK: &str = include_str!("fixtures/locked.json");
    const LAYERS: &str = include_str!("fixtures/layers.json");
    const BINDS: &str = include_str!("fixtures/binds.json");
    const OPTIONS: &str = include_str!("fixtures/getoption.json");

    #[test]
    fn only_targeted_events_pass() {
        let monitor = Monitor::find(MONITORS, "HDMI-A-1").unwrap();
        let click = Input::Button {
            button: Button::Left,
            down: true,
            x: 0.5,
            y: 0.5,
        };
        let scroll = Input::Scroll { x: 0.0, y: 1.0 };
        let key = Input::Key {
            key: "A".into(),
            down: true,
        };
        let moved = Input::Move { x: 0.5, y: 0.5 };
        let check = |monitors: &str, lock: &str, cursor: &str, focus: &str, input: &Input| {
            let snapshot = match Scope::from(input) {
                Scope::Move => [monitors, lock, cursor].join("\n"),
                Scope::Pointer => [monitors, lock, cursor, r#""default""#, BINDS].join("\n\n\n"),
                Scope::Keyboard => [
                    monitors,
                    lock,
                    cursor,
                    focus,
                    LAYERS,
                    r#""default""#,
                    BINDS,
                    OPTIONS,
                ]
                .join("\n\n\n"),
            };
            verify(&snapshot, &monitor, input.into()).is_some()
        };
        for event in [&click, &scroll, &key, &moved] {
            assert!(check(MONITORS, LOCK, CURSOR, FOCUS, event));
            assert!(!check(MONITORS, r#"{"locked":true}"#, CURSOR, FOCUS, event));
            assert!(!check("[]", LOCK, CURSOR, FOCUS, event));
            assert!(!check(
                &MONITORS.replace("\"x\": 0", "\"x\": -1920"),
                LOCK,
                CURSOR,
                FOCUS,
                event
            ));
        }
        for event in [&click, &scroll, &key] {
            assert!(!check(MONITORS, LOCK, r#"{"x":1920,"y":6}"#, FOCUS, event));
            assert!(!check(MONITORS, LOCK, "garbage", FOCUS, event));
        }
        assert!(check(MONITORS, LOCK, CURSOR, "{}", &click));
        assert!(!check(MONITORS, LOCK, CURSOR, "{}", &key));
        assert!(!check(
            MONITORS,
            LOCK,
            CURSOR,
            &FOCUS.replace("\"monitor\": 1", "\"monitor\": 2"),
            &key
        ));
        // Unreadable layout options, or a keymap file Hyprland resolves itself, block keys.
        let snapshot = |options: &str| {
            [
                MONITORS,
                LOCK,
                CURSOR,
                FOCUS,
                LAYERS,
                r#""default""#,
                BINDS,
                options,
            ]
            .join("\n")
        };
        let keyboard = |options: &str| verify(&snapshot(options), &monitor, Scope::Keyboard);
        assert!(keyboard(OPTIONS).is_some_and(|verified| verified.layout.is_some()));
        assert!(keyboard("unavailable").is_none());
        assert!(keyboard(&OPTIONS.replace("[[EMPTY]]", "~/.config/hypr/keymap.xkb")).is_none());
        assert!(keyboard(&OPTIONS.replace("kb_variant", "kb_options")).is_none());
    }

    #[test]
    fn regular_window_focus_does_not_override_interactive_layers() {
        let monitor = Monitor::find(MONITORS, "HDMI-A-1").unwrap();
        let check = |layers: &str, modes: &str| {
            let snapshot = [
                MONITORS,
                LOCK,
                CURSOR,
                FOCUS,
                layers,
                r#""default""#,
                BINDS,
                OPTIONS,
                modes,
            ]
            .join("\n");
            verify(&snapshot, &monitor, Scope::Keyboard).is_some()
        };
        for (output, level) in [("DP-1", "0"), ("HDMI-A-1", "2"), ("HDMI-A-1", "3")] {
            let mut layers: serde_json::Value = serde_json::from_str(LAYERS).unwrap();
            layers[output]["levels"][level] = serde_json::json!([{"address":"0xa123"}]);
            let layers = layers.to_string();
            assert!(!check(&layers, "unavailable"));
            assert!(!check(&layers, r#"{"0xa123":1}"#));
            assert!(!check(&layers, r#"{"0xa123":2}"#));
            assert!(check(&layers, r#"{"0xa123":0}"#));
        }
        assert!(!check("{}", "{}"));
    }

    #[test]
    fn scaled_rotated_output_uses_logical_bounds() {
        let json = MONITORS
            .replace("\"x\": 0", "\"x\": -540")
            .replace("\"scale\": 1", "\"scale\": 2")
            .replace("\"transform\": 0", "\"transform\": 1");
        let monitor = Monitor::find(&json, "HDMI-A-1").unwrap();
        assert!(monitor.contains(Cursor {
            x: -540.0,
            y: 959.0
        }));
        assert!(!monitor.contains(Cursor { x: 0.0, y: 0.0 }));
        assert!(!monitor.contains(Cursor {
            x: -540.0,
            y: 960.0
        }));
    }

    #[test]
    fn adjacent_output_is_allowed_but_overlapping_output_fails_closed() {
        let other = |x| {
            serde_json::json!({
                "id": 2, "name": "DP-1", "x": x, "y": 0, "width": 1920, "height": 1080,
                "scale": 1, "transform": 0, "dpmsStatus": true, "disabled": false, "mirrorOf": "none",
            })
        };
        let mut monitors: Vec<serde_json::Value> = serde_json::from_str(MONITORS).unwrap();
        monitors.push(other(1920));
        assert!(Monitor::find(&serde_json::to_string(&monitors).unwrap(), "HDMI-A-1").is_ok());
        monitors[1] = other(1919);
        assert!(Monitor::find(&serde_json::to_string(&monitors).unwrap(), "HDMI-A-1").is_err());
    }

    #[test]
    fn blocks_system_keys_and_combinations_in_either_order() {
        assert_eq!(evdev("Right"), Some(106));
        assert_eq!(evdev("A"), Some(30));
        assert_eq!(evdev("Super"), None);
        assert!(blocked_combo(111, &[29, 56].into()));
        assert!(blocked_combo(97, &[100, 88].into()));
        assert!(!blocked_combo(88, &[97].into()));
    }
}
