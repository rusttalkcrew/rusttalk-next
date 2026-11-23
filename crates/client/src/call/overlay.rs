//! What a screen tile adds on request: the right-click menu's rows, and the "Show FPS"
//! and "Stats for nerds" overlays. Both overlays read the numbers the call's periodic
//! stats polling already collected, so nothing is polled per frame.

use eframe::egui::{Color32, FontId, Pos2, Rect, Ui, Vec2, pos2, vec2};

use super::stage::Tile;
use crate::{
    server::{Stats, TrackStats},
    ui,
};

/// Tiles smaller than this get no overlay: it would cover the picture.
const MIN_TILE: Vec2 = Vec2::new(180.0, 90.0);
const MARGIN: f32 = 8.0;
const PAD: Vec2 = Vec2::new(6.0, 3.0);
const COLUMN_GAP: f32 = 10.0;

/// A row of a screen tile's right-click menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Focus,
    Unfocus,
    FullScreen,
    StopWatching,
    /// The volume of the stream's sound.
    Volume,
    ShowFps,
    NerdStats,
}

/// The rows for a tile, in order. Person tiles keep their own menu.
pub fn rows(tile: &Tile, focused: bool, picture: bool, watched: bool, sound: bool) -> Vec<Row> {
    let mut rows = vec![if focused { Row::Unfocus } else { Row::Focus }];
    if picture {
        rows.push(Row::FullScreen);
    }
    match tile {
        Tile::Screen(_) if watched => rows.push(Row::StopWatching),
        Tile::Screen(_) | Tile::Preview | Tile::Person(_) | Tile::Terminal(_) => {}
    }
    if sound && watched {
        rows.push(Row::Volume);
    }
    rows.extend([Row::ShowFps, Row::NerdStats]);
    rows
}

/// A tile's overlays, with the numbers of its track.
pub struct Overlay {
    pub fps: bool,
    pub nerd: bool,
    track: Option<TrackStats>,
    ping_ms: Option<f64>,
}

impl Overlay {
    /// Looks up the tile's track only while an overlay is on. A person's tile has none.
    pub fn new(tile: &Tile, fps: bool, nerd: bool, stats: Option<&Stats>) -> Self {
        let track = stats.filter(|_| fps || nerd).and_then(|stats| {
            stats.tracks.iter().find(|track| match tile {
                Tile::Screen(sid) => !track.mine && track.sid == *sid,
                Tile::Preview => track.mine && track.screen,
                Tile::Person(_) | Tile::Terminal(_) => false,
            })
        });
        Self {
            fps,
            nerd,
            track: track.cloned(),
            ping_ms: stats.and_then(|stats| stats.ping_ms),
        }
    }

    /// At the top left of `rect`, below the streaming badge when present. The
    /// fuller panel gives way to the one line when the tile is too small for it.
    pub fn paint(&self, ui: &Ui, rect: Rect, badge: Option<Rect>) {
        let Some(track) = &self.track else { return };
        if rect.width() < MIN_TILE.x || rect.height() < MIN_TILE.y {
            return;
        }
        let top = badge.map_or(rect.top() + MARGIN, |badge| badge.bottom() + ui::SPACE_XS);
        let at = pos2(rect.left() + MARGIN, top);
        if self.nerd {
            let rows = nerd_rows(track, self.ping_ms);
            if panel(ui, rect, at, &rows) {
                return;
            }
        }
        // Nerd stats include the frame rate, so the chip stands in for a panel that
        // does not fit.
        if (self.fps || self.nerd)
            && let Some(line) = fps_line(track)
        {
            chip(ui, at, &line);
        }
    }
}

/// Just the frame rate, and the height as people say it: "60 fps · 1080p".
pub fn fps_line(track: &TrackStats) -> Option<String> {
    let video = track.video?;
    Some(format!("{:.0} fps · {}p", video.fps, video.height))
}

/// Label and value of each line in the fuller overlay. A number not reported yet
/// shows as a dash, so the rows stay put while the stats arrive.
pub fn nerd_rows(track: &TrackStats, ping_ms: Option<f64>) -> Vec<(String, String)> {
    let dash = || "—".to_owned();
    let detail = track.detail.as_ref();
    let mut rows = Vec::new();
    let mut add = |label: &str, value: Option<String>| {
        rows.push((label.to_owned(), value.unwrap_or_else(dash)));
    };
    add(
        t!("call.stat_picture"),
        track.video.map(|v| {
            let (w, h, fps) = (v.width, v.height, v.fps);
            format!("{w}×{h} · {fps:.0} fps")
        }),
    );
    add(
        t!("call.stat_bitrate"),
        Some(format!("{:.0} kbps", track.kbps)),
    );
    add(
        t!("call.stat_codec"),
        detail
            .and_then(|d| d.codec.as_deref())
            .map(|codec| codec.trim_start_matches("video/").to_owned()),
    );
    let label = if track.mine {
        t!("call.stat_encoder")
    } else {
        t!("call.stat_decoder")
    };
    add(label, detail.and_then(|d| d.implementation.clone()));
    let loss = match (track.lost, track.received) {
        (Some(lost), Some(received)) if lost + received as i64 > 0 => Some(format!(
            "{:.1}% ({lost})",
            100.0 * lost as f64 / (lost + received as i64) as f64
        )),
        (Some(lost), None) => Some(lost.to_string()),
        _ => None,
    };
    add(t!("call.stat_loss"), loss);
    add(
        t!("call.jitter"),
        track.jitter_ms.map(|jitter| format!("{jitter:.0} ms")),
    );
    add(
        t!("call.stat_rtt"),
        ping_ms.map(|ping| format!("{ping:.0} ms")),
    );
    let label = if track.mine {
        t!("call.stat_encoded")
    } else {
        t!("call.stat_decoded")
    };
    add(label, detail.map(|d| d.frames.to_string()));
    if !track.mine {
        add(
            t!("call.stat_dropped"),
            detail.and_then(|d| d.dropped).map(|n| n.to_string()),
        );
    }
    // Only said when something holds the encoder back.
    if let Some(limit) = detail.and_then(|d| d.limited_by) {
        let text = match limit {
            "cpu" => t!("call.limit_cpu"),
            "bandwidth" => t!("call.limit_bandwidth"),
            other => other,
        };
        add(t!("call.stat_limited"), Some(text.to_owned()));
    }
    rows
}

/// Dimmer than the name tag's text, so it reads as a side note.
fn weak(strength: f32) -> Color32 {
    ui::OVERLAY_TEXT.gamma_multiply(strength)
}

fn chip(ui: &Ui, at: Pos2, text: &str) {
    let painter = ui.painter();
    let galley = painter.layout_no_wrap(
        text.to_owned(),
        FontId::proportional(ui::TEXT_CAPTION),
        weak(0.8),
    );
    let tag = Rect::from_min_size(at, galley.size() + 2.0 * PAD);
    painter.rect_filled(tag, ui::RADIUS_S, ui::OVERLAY_BUTTON);
    painter.galley(tag.min + PAD, galley, weak(0.8));
}

/// A monospace table; `false` when it does not fit in `tile`.
fn panel(ui: &Ui, tile: Rect, at: Pos2, rows: &[(String, String)]) -> bool {
    if rows.is_empty() {
        return false;
    }
    let painter = ui.painter();
    let font = FontId::monospace(ui::TEXT_CAPTION);
    let block = |texts: Vec<&str>, color| {
        painter.layout(texts.join("\n"), font.clone(), color, f32::INFINITY)
    };
    let labels = block(rows.iter().map(|r| r.0.as_str()).collect(), weak(0.6));
    let values = block(rows.iter().map(|r| r.1.as_str()).collect(), weak(0.85));
    let size = vec2(
        labels.size().x + COLUMN_GAP + values.size().x,
        labels.size().y.max(values.size().y),
    ) + 2.0 * PAD;
    // Leave the bottom for the name tag.
    let room = vec2(
        tile.width() - 2.0 * MARGIN,
        tile.bottom() - at.y - 3.0 * MARGIN - ui::TEXT_SMALL,
    );
    if size.x > room.x || size.y > room.y {
        return false;
    }
    let panel = Rect::from_min_size(at, size);
    painter.rect_filled(panel, ui::RADIUS_S, ui::OVERLAY_BUTTON);
    let origin = panel.min + PAD;
    let value_at = pos2(origin.x + labels.size().x + COLUMN_GAP, origin.y);
    painter.galley(origin, labels, weak(0.6));
    painter.galley(value_at, values, weak(0.85));
    true
}

#[cfg(test)]
mod tests {
    use eframe::egui;
    use rusttalk_core::client::voice::{VideoDetail, VideoFormat};

    use super::*;

    fn track(mine: bool) -> TrackStats {
        TrackStats {
            person: [0; 32],
            sid: "TR_screen".into(),
            mine,
            screen: mine,
            app: None,
            kbps: 6120.4,
            video: Some(VideoFormat {
                width: 1920,
                height: 1080,
                fps: 59.6,
            }),
            detail: Some(VideoDetail {
                codec: Some("video/VP9".into()),
                implementation: Some("libvpx".into()),
                frames: 3600,
                dropped: (!mine).then_some(2),
                limited_by: mine.then_some("bandwidth"),
            }),
            jitter_ms: Some(3.7),
            lost: Some(12),
            received: (!mine).then_some(4000),
        }
    }

    fn text(rows: &[(String, String)]) -> Vec<String> {
        rows.iter().map(|(l, v)| format!("{l}: {v}")).collect()
    }

    #[test]
    fn fps_line_is_short() {
        assert_eq!(fps_line(&track(false)).unwrap(), "60 fps · 1080p");
        let mut audio_only = track(false);
        audio_only.video = None;
        assert_eq!(fps_line(&audio_only), None);
    }

    #[test]
    fn nerd_rows_for_a_received_screen() {
        assert_eq!(
            text(&nerd_rows(&track(false), Some(23.4))),
            [
                "Picture: 1920×1080 · 60 fps",
                "Bitrate: 6120 kbps",
                "Codec: VP9",
                "Decoder: libvpx",
                "Packet loss: 0.3% (12)",
                "Jitter: 4 ms",
                "Round trip: 23 ms",
                "Decoded: 3600",
                "Dropped: 2",
            ]
        );
    }

    #[test]
    fn nerd_rows_for_our_own_share_and_missing_numbers() {
        let rows = text(&nerd_rows(&track(true), None));
        assert!(rows.contains(&"Encoder: libvpx".to_owned()));
        assert!(rows.contains(&"Packet loss: 12".to_owned()));
        assert!(rows.contains(&"Limited by: Bandwidth".to_owned()));
        assert!(rows.contains(&"Round trip: —".to_owned()));
        // The rows stay put, with dashes, while numbers are missing.
        let mut bare = track(false);
        (bare.video, bare.detail, bare.jitter_ms) = (None, None, None);
        (bare.lost, bare.received) = (None, None);
        assert_eq!(
            text(&nerd_rows(&bare, None)),
            [
                "Picture: —",
                "Bitrate: 6120 kbps",
                "Codec: —",
                "Decoder: —",
                "Packet loss: —",
                "Jitter: —",
                "Round trip: —",
                "Decoded: —",
                "Dropped: —",
            ]
        );
    }

    /// Everything the overlay painted, as text.
    fn painted(overlay: &Overlay, size: Vec2) -> Vec<String> {
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(Default::default(), |ui| {
            overlay.paint(ui, Rect::from_min_size(pos2(0.0, 0.0), size), None);
        });
        output.textures_delta.clear();
        output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn nerd_stats_alone_still_show_a_line_on_a_tile_too_small_for_the_panel() {
        let stats = Stats {
            tracks: vec![track(false)],
            ..Stats::default()
        };
        let tile = Tile::Screen("TR_screen".into());
        let overlay = Overlay::new(&tile, false, true, Some(&stats));
        assert_eq!(painted(&overlay, vec2(200.0, 100.0)), ["60 fps · 1080p"]);
        // Room enough: the panel, not the line.
        let big = painted(&overlay, vec2(600.0, 400.0));
        assert_eq!(big.len(), 2);
        assert!(big[1].contains("1920×1080"));
        // Neither on: nothing.
        let off = Overlay::new(&tile, false, false, Some(&stats));
        assert!(painted(&off, vec2(600.0, 400.0)).is_empty());
    }

    #[test]
    fn menu_rows_by_tile() {
        let screen = Tile::Screen("s".into());
        use Row::*;
        assert_eq!(
            rows(&screen, false, true, true, true),
            [Focus, FullScreen, StopWatching, Volume, ShowFps, NerdStats]
        );
        // Not received yet: nothing to stop, no picture or sound to adjust.
        assert_eq!(
            rows(&screen, true, false, false, true),
            [Unfocus, ShowFps, NerdStats]
        );
        assert_eq!(
            rows(&Tile::Preview, false, true, false, false),
            [Focus, FullScreen, ShowFps, NerdStats]
        );
    }

    #[test]
    fn overlay_finds_the_tiles_own_track() {
        let stats = Stats {
            tracks: vec![track(false), track(true)],
            ..Stats::default()
        };
        let find = |tile: &Tile| Overlay::new(tile, true, false, Some(&stats)).track;
        assert!(
            Overlay::new(&Tile::Preview, false, false, Some(&stats))
                .track
                .is_none()
        );
        assert!(!find(&Tile::Screen("TR_screen".into())).unwrap().mine);
        assert!(find(&Tile::Preview).unwrap().mine);
        assert!(find(&Tile::Screen("other".into())).is_none());
        assert!(find(&Tile::Person([0; 32])).is_none());
    }
}
