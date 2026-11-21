//! Interprets read-only Hyprland IPC snapshots independently of Wayland.

use serde::Deserialize;

use crate::control::{Input, scan_code};

#[derive(Deserialize, PartialEq)]
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
        Self::from_list(serde_json::from_str(monitors)?, name)
    }

    fn from_list(monitors: Vec<Self>, name: &str) -> anyhow::Result<Self> {
        let mut matching = monitors.iter().enumerate().filter(|(_, m)| m.name == name);
        let index = matching.next().and_then(|(index, monitor)| {
            let available = monitor.usable()
                && matching.next().is_none()
                && !monitors
                    .iter()
                    .any(|other| other.name != name && other.usable() && monitor.overlaps(other));
            available.then_some(index)
        });
        let index = index.ok_or_else(|| {
            anyhow::anyhow!("shared output is missing, overlapping or unavailable")
        })?;
        Ok(monitors
            .into_iter()
            .nth(index)
            .expect("index into these monitors"))
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
    monitor: i64,
    mapped: bool,
    hidden: bool,
}

#[derive(Deserialize)]
struct Lock {
    locked: bool,
}

/// A missing or changed output, malformed snapshot or locked desktop blocks input.
pub(super) fn allows(snapshot: &str, target: &Monitor, input: &Input) -> bool {
    // The four batch results are distinct JSON values with arbitrary whitespace.
    let mut fields = serde_json::Deserializer::from_str(snapshot);
    let monitor = Vec::<Monitor>::deserialize(&mut fields)
        .ok()
        .and_then(|list| Monitor::from_list(list, &target.name).ok());
    let lock = Lock::deserialize(&mut fields);
    if monitor.as_ref() != Some(target) || !matches!(lock, Ok(Lock { locked: false })) {
        return false;
    }
    if matches!(input, Input::Move { .. }) {
        return true;
    }
    if !Cursor::deserialize(&mut fields).is_ok_and(|cursor| target.contains(cursor)) {
        return false;
    }
    if matches!(input, Input::Key { .. }) {
        return Focus::deserialize(&mut fields)
            .is_ok_and(|focus| focus.mapped && !focus.hidden && focus.monitor == target.id);
    }
    true
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
            let snapshot = [monitors, lock, cursor, focus].join("\n\n\n");
            allows(&snapshot, &monitor, input)
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
