//! The "What's new" page: `CHANGELOG.md` (the one source, also used for the website's
//! roadmap) built into the app and drawn as a timeline. The entries stay in English.

use std::sync::LazyLock;

use eframe::egui::{self, Pos2, Stroke, Ui};

use crate::ui::{self, SPACE_L, SPACE_S, SPACE_XL, TEXT_BODY};

const CHANGELOG: &str = include_str!("../../../CHANGELOG.md");

/// Room left of the text for the line and its dots.
const GUTTER: f32 = 28.0;
const DOT: f32 = 6.0;

#[derive(Debug, PartialEq)]
pub struct Release {
    /// "v0.1.7", or "Unreleased".
    pub title: String,
    /// "2026-11-22"; `None` for the unreleased changes.
    pub date: Option<String>,
    pub notes: Vec<String>,
}

/// Read the changelog: `## Unreleased` or `## vX.Y.Z - YYYY-MM-DD` headings, each
/// followed by `- ` bullet lines. Text before the first heading is ignored.
pub fn parse(text: &str) -> Result<Vec<Release>, String> {
    let mut releases: Vec<Release> = Vec::new();
    for (number, line) in text.lines().enumerate().map(|(i, line)| (i + 1, line)) {
        let line = line.trim_end();
        if let Some(heading) = line.strip_prefix("## ") {
            let (title, date) = match heading.split_once(" - ") {
                Some((title, date)) => (title, Some(date)),
                None => (heading, None),
            };
            let valid = match date {
                None => title == "Unreleased",
                Some(date) => title.starts_with('v') && is_date(date),
            };
            if !valid {
                return Err(format!("line {number}: bad heading {line:?}"));
            }
            releases.push(Release {
                title: title.to_owned(),
                date: date.map(str::to_owned),
                notes: Vec::new(),
            });
        } else if let Some(release) = releases.last_mut() {
            if let Some(note) = line.strip_prefix("- ") {
                release.notes.push(note.to_owned());
            } else if !line.is_empty() {
                return Err(format!(
                    "line {number}: expected a \"- \" bullet, got {line:?}"
                ));
            }
        }
    }
    Ok(releases)
}

fn is_date(text: &str) -> bool {
    let digits = |part: &str, len| part.len() == len && part.bytes().all(|b| b.is_ascii_digit());
    let mut parts = text.split('-');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(y), Some(m), Some(d), None) if digits(y, 4) && digits(m, 2) && digits(d, 2)
    )
}

/// The released versions, newest first. A changelog that does not parse shows nothing:
/// the unit test keeps the real file valid.
fn shipped() -> &'static [Release] {
    static SHIPPED: LazyLock<Vec<Release>> = LazyLock::new(|| {
        let mut releases = parse(CHANGELOG).unwrap_or_else(|error| {
            tracing::error!(%error, "CHANGELOG.md unreadable");
            Vec::new()
        });
        releases.retain(|release| release.date.is_some());
        releases
    });
    &SHIPPED
}

/// Whether to open "What's new" at start: the app was updated since it last showed it.
/// `fresh` is a first start with no saved settings, where there is nothing "new" yet.
pub fn announce(seen: &str, current: &str, fresh: bool) -> bool {
    !fresh && seen != current
}

/// The timeline: a vertical line, a dot and the version and date per release, then its notes.
pub fn page(ui: &mut Ui) {
    ui::page_section(ui, t!("settings.whats_new"));
    ui.weak(t!("settings.whats_new_help"));
    ui.add_space(SPACE_L);
    let colors = ui::colors(ui);
    let mut line_from: Option<Pos2> = None;
    let mut line_to = 0.0;
    let mut dots = Vec::new();
    for release in shipped() {
        let block = ui.horizontal_top(|ui| {
            ui.add_space(GUTTER);
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(ui::title(&release.title).color(colors.strong));
                    if let Some(date) = &release.date {
                        ui.label(ui::caption(date));
                    }
                });
                ui.add_space(SPACE_S);
                for note in &release.notes {
                    ui.horizontal_top(|ui| {
                        ui.label(egui::RichText::new("•").size(TEXT_BODY).color(colors.muted));
                        ui.add(egui::Label::new(note).wrap());
                    });
                }
            });
        });
        let rect = block.response.rect;
        let dot = Pos2::new(rect.left() + DOT, rect.top() + ui::TEXT_TITLE / 2.0 + 3.0);
        line_from.get_or_insert(dot);
        line_to = rect.bottom();
        dots.push(dot);
        ui.add_space(SPACE_XL);
    }
    // Drawn after the text: the line and dots sit in the gutter and overlap nothing.
    if let Some(from) = line_from {
        let painter = ui.painter();
        painter.line_segment(
            [from, Pos2::new(from.x, line_to)],
            Stroke::new(2.0, colors.border),
        );
        for dot in dots {
            painter.circle_filled(dot, DOT, colors.success);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_real_changelog_parses() {
        let releases = parse(CHANGELOG).unwrap();
        // Unreleased is empty (and so absent) right after a release.
        let shipped = match releases[0].title.as_str() {
            "Unreleased" => {
                assert!(releases[0].date.is_none());
                &releases[1..]
            }
            _ => &releases[..],
        };
        assert!(shipped.len() >= 8, "v0.1.0 to v0.1.7 are listed");
        assert_eq!(shipped.last().unwrap().title, "v0.1.0");
        for release in shipped {
            assert!(release.date.is_some(), "{} needs a date", release.title);
            assert!(!release.notes.is_empty(), "{} has no notes", release.title);
        }
        // Newest first, so the timeline reads top to bottom.
        assert!(shipped.windows(2).all(|pair| pair[0].date >= pair[1].date));
    }

    #[test]
    fn parses_headings_and_bullets() {
        let text = "# Changelog\nintro\n## Unreleased\n- a\n\n## v1.2.3 - 2026-01-02\n- b\n- c\n";
        let releases = parse(text).unwrap();
        assert_eq!(releases.len(), 2);
        assert_eq!(releases[0].title, "Unreleased");
        assert_eq!(releases[0].notes, ["a"]);
        assert_eq!(releases[1].title, "v1.2.3");
        assert_eq!(releases[1].date.as_deref(), Some("2026-01-02"));
        assert_eq!(releases[1].notes, ["b", "c"]);
    }

    #[test]
    fn rejects_bad_headings_and_stray_text() {
        assert!(parse("## 1.2.3 - 2026-01-02\n").is_err());
        assert!(parse("## v1.2.3 - Jan 2\n").is_err());
        assert!(parse("## Soon\n").is_err());
        assert!(parse("## Unreleased\nplain text\n").is_err());
    }

    #[test]
    fn shows_once_after_an_update() {
        assert!(announce("0.1.7", "0.1.8", false), "updated");
        assert!(
            announce("", "0.1.8", false),
            "updated from a version before this existed"
        );
        assert!(!announce("0.1.8", "0.1.8", false), "already seen");
        assert!(!announce("", "0.1.8", true), "first start has nothing new");
    }
}
