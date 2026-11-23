//! Polls (#204): the editor that takes the message box's place, and the poll card
//! in the chat. Votes and ends are encrypted messages too; `Chat` counts them.

use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui::{
    self, Align, Align2, CursorIcon, Key, Layout, Margin, Rect, RichText, Sense, Ui, Vec2,
};
use rusttalk_core::messages::{
    MAX_POLL_OPTION, MAX_POLL_OPTIONS, MAX_POLL_QUESTION, MIN_POLL_OPTIONS, Poll, PollPart, Tally,
    valid_poll,
};

use crate::{
    emoji,
    server::Id,
    ui::{self, Look},
};

/// How long a poll stays open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Length {
    #[default]
    Open,
    Hour,
    Day,
    Week,
}

impl Length {
    fn seconds(self) -> i64 {
        match self {
            Self::Open => 0,
            Self::Hour => 3_600,
            Self::Day => 86_400,
            Self::Week => 7 * 86_400,
        }
    }
}

/// A poll being written.
#[derive(Clone, Debug)]
pub struct Editor {
    pub question: String,
    pub options: Vec<String>,
    pub multiple: bool,
    pub length: Length,
    /// The option row to focus next frame.
    focus: Option<usize>,
    focus_question: bool,
}

impl Default for Editor {
    fn default() -> Self {
        Self {
            question: String::new(),
            options: vec![String::new(); MIN_POLL_OPTIONS],
            multiple: false,
            length: Length::Open,
            focus: None,
            focus_question: true,
        }
    }
}

impl Editor {
    /// Enter in a row: a new empty row below it, focused, up to the maximum.
    pub fn enter(&mut self, at: usize) {
        if self.options.len() < MAX_POLL_OPTIONS {
            self.options.insert(at + 1, String::new());
            self.focus = Some(at + 1);
        }
    }

    /// Backspace in an empty row removes it, down to the minimum, and focuses the row above.
    pub fn backspace(&mut self, at: usize) {
        if self.options[at].is_empty() && self.options.len() > MIN_POLL_OPTIONS {
            self.options.remove(at);
            self.focus = Some(at.saturating_sub(1));
        }
    }

    /// The row's × button.
    pub fn remove(&mut self, at: usize) {
        if self.options.len() > MIN_POLL_OPTIONS {
            self.options.remove(at);
        }
    }

    /// Blank rows are left out, so a row just added with Enter doesn't block posting.
    fn trimmed(&self) -> (String, Vec<String>) {
        let options = (self.options.iter())
            .map(|option| option.trim().to_owned())
            .filter(|option| !option.is_empty())
            .collect();
        (self.question.trim().to_owned(), options)
    }

    pub fn valid(&self) -> bool {
        let (question, options) = self.trimmed();
        valid_poll(&question, &options)
    }

    /// The question and the poll to send, closing `length` after `now` (unix seconds).
    pub fn poll(&self, now: i64) -> (String, PollPart) {
        let (question, options) = self.trimmed();
        let ends_at = match self.length {
            Length::Open => 0,
            length => now + length.seconds(),
        };
        let poll = Poll {
            options,
            multiple: self.multiple,
            ends_at,
        };
        (question, PollPart::Open(poll))
    }
}

pub enum Action {
    Cancel,
    Post,
}

/// The poll editor, in the composer's place, or `attached` to a new forum post: then it
/// has no Post button, and Cancel takes the poll off the post.
pub fn editor(ui: &mut Ui, editor: &mut Editor, attached: bool) -> Option<Action> {
    let mut action = None;
    let escape = !attached && ui.input(|input| input.key_pressed(Key::Escape));
    egui::Frame::new()
        // The page colour with a border, as text fields are made for it.
        .fill(ui::colors(ui).main)
        .stroke(egui::Stroke::new(1.0, ui::colors(ui).border))
        .corner_radius(ui::RADIUS_WINDOW)
        .inner_margin(Margin::same(ui::SPACE_M as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(ui::bold(t!("polls.create")));
            ui.add_space(ui::SPACE_S);
            let question = ui.add(
                ui::field(&mut editor.question)
                    .hint_text(t!("polls.question"))
                    .font(emoji::font(ui::TEXT_BODY))
                    .char_limit(MAX_POLL_QUESTION)
                    .desired_width(f32::INFINITY),
            );
            if std::mem::take(&mut editor.focus_question) {
                question.request_focus();
            }
            ui.add_space(ui::SPACE_S);
            ui.label(ui::caption(t!("polls.answers")));
            let (mut enter, mut backspace, mut remove) = (None, None, None);
            let can_remove = editor.options.len() > MIN_POLL_OPTIONS;
            let close = Vec2::splat(ui::CONTROL_HEIGHT);
            for (at, option) in editor.options.iter_mut().enumerate() {
                let was_empty = option.is_empty();
                ui.horizontal(|ui| {
                    let width = ui.available_width() - close.x - ui.spacing().item_spacing.x;
                    let row = ui.add(
                        ui::field(option)
                            .hint_text(t!("polls.answer", number = at + 1))
                            .font(emoji::font(ui::TEXT_BODY))
                            .char_limit(MAX_POLL_OPTION)
                            .desired_width(width),
                    );
                    if editor.focus == Some(at) {
                        row.request_focus();
                    }
                    let (enter_key, backspace_key) = ui.input(|input| {
                        (
                            input.key_pressed(Key::Enter),
                            input.key_pressed(Key::Backspace),
                        )
                    });
                    // A one-line field gives up focus on Enter.
                    if row.lost_focus() && enter_key {
                        enter = Some(at);
                    }
                    if row.has_focus() && backspace_key && was_empty {
                        backspace = Some(at);
                    }
                    let tip = t!("polls.remove_answer");
                    if ui
                        .add_enabled_ui(can_remove, |ui| {
                            ui::icon_button(ui, "×", tip, Look::Ghost, close)
                        })
                        .inner
                        .clicked()
                    {
                        remove = Some(at);
                    }
                });
            }
            editor.focus = None;
            if let Some(at) = enter {
                editor.enter(at);
            }
            if let Some(at) = backspace {
                editor.backspace(at);
            }
            if let Some(at) = remove {
                editor.remove(at);
            }
            ui.add_space(ui::SPACE_S);
            ui.horizontal(|ui| {
                ui::toggle(ui, &mut editor.multiple, t!("polls.multiple"));
                ui.label(t!("polls.multiple"));
            });
            ui.add_space(ui::SPACE_S);
            ui.label(ui::caption(t!("polls.duration")));
            ui::segmented(
                ui,
                &mut editor.length,
                &[
                    (Length::Open, t!("polls.no_end"), ""),
                    (Length::Hour, t!("polls.hour"), ""),
                    (Length::Day, t!("polls.day"), ""),
                    (Length::Week, t!("polls.week"), ""),
                ],
            );
            ui.add_space(ui::SPACE_M);
            // In a post form the form's own buttons send it; a right-to-left layout
            // there would take all the height left in the form.
            if attached {
                if ui::secondary(ui, t!("forum.remove_poll")).clicked() {
                    action = Some(Action::Cancel);
                }
                return;
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let valid = editor.valid();
                if ui
                    .add_enabled_ui(valid, |ui| ui::primary(ui, t!("polls.post")))
                    .inner
                    .clicked()
                {
                    action = Some(Action::Post);
                }
                if ui::secondary(ui, t!("common.cancel")).clicked() || escape {
                    action = Some(Action::Cancel);
                }
            });
        });
    action
}

/// Your choices after clicking an option: on a single-answer poll it replaces your
/// vote, or takes it back when clicked again; on a multiple-answer poll it toggles.
pub fn toggle(mine: &[u8], option: u8, multiple: bool) -> Vec<u8> {
    if !multiple {
        return if mine == [option] {
            Vec::new()
        } else {
            vec![option]
        };
    }
    let mut choices: Vec<u8> = mine.iter().copied().filter(|c| *c != option).collect();
    if choices.len() == mine.len() {
        choices.push(option);
        choices.sort_unstable();
    }
    choices
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// This device's clock in milliseconds, which orders one person's votes.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// "Ends in 3 hours", "Ended" or nothing for a poll without an end.
fn status(tally: &Tally, now: i64) -> Option<String> {
    if !tally.open(now) {
        return Some(t!("polls.ended").into());
    }
    let left = tally.closes_at? - now;
    Some(if left >= 86_400 {
        t!("polls.days_left", count = left / 86_400)
    } else if left >= 3_600 {
        t!("polls.hours_left", count = left / 3_600)
    } else {
        t!("polls.minutes_left", count = (left / 60).max(1))
    })
}

/// A poll in the chat. Returns your new choices when you click an option. `more` goes
/// on the footer line, after the vote count.
pub fn card(
    ui: &mut Ui,
    question: &str,
    poll: &Poll,
    tally: &Tally,
    me: &Id,
    names: impl Fn(&Id) -> String,
    more: Option<&str>,
) -> Option<Vec<u8>> {
    let now = now();
    let open = tally.open(now);
    if open && let Some(at) = tally.closes_at {
        // Keep the time left and the end current.
        let wait = (at - now).clamp(1, 60) as u64;
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs(wait));
    }
    let mine: Vec<u8> = (tally.voters.iter().enumerate())
        .filter(|(_, voters)| voters.contains(me))
        .map(|(at, _)| at as u8)
        .collect();
    let mut picked = None;
    let c = ui::colors(ui);
    let accent = ui::accent(ui);
    ui::card_frame(ui)
        .inner_margin(Margin::same(ui::SPACE_M as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width().min(440.0));
            ui.label(ui::bold(question).size(ui::TEXT_TITLE));
            let hint = if !open {
                t!("polls.results")
            } else if poll.multiple {
                t!("polls.pick_several")
            } else {
                t!("polls.pick_one")
            };
            ui.label(ui::caption(hint));
            ui.add_space(ui::SPACE_S);
            for (at, option) in poll.options.iter().enumerate() {
                let voters = &tally.voters[at];
                let share = if tally.people == 0 {
                    0.0
                } else {
                    voters.len() as f32 / tally.people as f32
                };
                let chosen = mine.contains(&(at as u8));
                let size = Vec2::new(ui.available_width(), ui::CONTROL_HEIGHT + 4.0);
                let sense = if open { Sense::click() } else { Sense::hover() };
                let (rect, response) = ui.allocate_exact_size(size, sense);
                let label = format!("{option}, {}", t!("polls.votes", count = voters.len()));
                response.widget_info(|| {
                    egui::WidgetInfo::selected(egui::Role::CheckBox, open, chosen, &label)
                });
                let painter = ui.painter();
                let hovered = open && response.hovered();
                painter.rect_filled(
                    rect,
                    ui::RADIUS,
                    if hovered { c.button_hover } else { c.button },
                );
                let bar =
                    Rect::from_min_size(rect.min, Vec2::new(rect.width() * share, rect.height()));
                let fill = if chosen { accent.base } else { c.muted };
                painter.rect_filled(bar, ui::RADIUS, fill.gamma_multiply(0.45));
                if chosen {
                    painter.rect_stroke(
                        rect,
                        ui::RADIUS,
                        egui::Stroke::new(1.5, accent.base),
                        egui::StrokeKind::Inside,
                    );
                }
                let text = if chosen {
                    format!("✔ {option}")
                } else {
                    option.clone()
                };
                let font = emoji::font(ui::TEXT_BODY);
                let left = rect.left_center() + Vec2::new(ui::SPACE_S + 2.0, 0.0);
                let counts = format!("{}%  ·  {}", (share * 100.0).round() as u32, voters.len());
                let right = rect.right_center() - Vec2::new(ui::SPACE_S + 2.0, 0.0);
                let counts =
                    painter.text(right, Align2::RIGHT_CENTER, counts, font.clone(), c.muted);
                let room = (counts.left() - left.x - ui::SPACE_S).max(0.0);
                let galley = painter.layout(text, font, c.strong, room);
                painter.galley(
                    left - Vec2::new(0.0, galley.size().y / 2.0),
                    galley,
                    c.strong,
                );
                let response = if voters.is_empty() {
                    response
                } else {
                    let who: Vec<String> = voters.iter().map(&names).collect();
                    response.on_hover_text(emoji::text(who.join(", ")))
                };
                if open {
                    let response = response.on_hover_cursor(CursorIcon::PointingHand);
                    if response.clicked() {
                        picked = Some(toggle(&mine, at as u8, poll.multiple));
                    }
                }
                ui.add_space(2.0);
            }
            ui.add_space(ui::SPACE_XS);
            let mut footer = t!("polls.voters", count = tally.people);
            if let Some(status) = status(tally, now) {
                footer = format!("{footer}  ·  {status}");
            }
            if let Some(more) = more {
                footer = format!("{footer}  ·  {more}");
            }
            ui.label(RichText::new(footer).size(ui::TEXT_CAPTION).color(c.muted));
        });
    picked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_adds_a_row_below_up_to_ten() {
        let mut editor = Editor {
            options: vec!["a".into(), "b".into()],
            ..Default::default()
        };
        editor.enter(0);
        assert_eq!(editor.options, ["a", "", "b"]);
        assert_eq!(editor.focus, Some(1));
        while editor.options.len() < MAX_POLL_OPTIONS {
            editor.enter(editor.options.len() - 1);
        }
        editor.enter(0);
        assert_eq!(editor.options.len(), MAX_POLL_OPTIONS);
    }

    #[test]
    fn backspace_removes_only_an_empty_row_down_to_two() {
        let mut editor = Editor {
            options: vec!["a".into(), String::new(), "c".into()],
            ..Default::default()
        };
        editor.backspace(0);
        assert_eq!(editor.options.len(), 3);
        editor.backspace(1);
        assert_eq!(editor.options, ["a", "c"]);
        assert_eq!(editor.focus, Some(0));
        editor.options[1].clear();
        editor.backspace(1);
        editor.remove(0);
        assert_eq!(editor.options.len(), MIN_POLL_OPTIONS);
    }

    #[test]
    fn posting_needs_a_question_and_filled_answers() {
        let mut editor = Editor::default();
        assert!(!editor.valid());
        editor.question = " Lunch? ".into();
        editor.options = vec!["Pizza".into(), " ".into()];
        assert!(!editor.valid());
        editor.options = vec!["Pizza".into(), "Sushi ".into(), String::new()];
        editor.length = Length::Day;
        assert!(editor.valid());
        let (question, part) = editor.poll(1_000);
        assert_eq!(question, "Lunch?");
        assert_eq!(
            part,
            PollPart::Open(Poll {
                options: vec!["Pizza".into(), "Sushi".into()],
                multiple: false,
                ends_at: 1_000 + 86_400,
            })
        );
    }

    #[test]
    fn keys_in_the_editor_add_and_remove_rows() {
        use egui_kittest::{Harness, kittest::Queryable};
        let mut harness = Harness::builder()
            .with_size(Vec2::new(500.0, 700.0))
            .build_ui_state(
                |ui, (fonts, editor): &mut (bool, Editor)| {
                    // Fonts before the first text is laid out.
                    if !std::mem::replace(fonts, true) {
                        crate::ui::install(ui.ctx());
                        return;
                    }
                    super::editor(ui, editor, false);
                },
                (false, Editor::default()),
            );
        harness.run_steps(2);
        harness
            .get_by(|node| node.placeholder() == Some("Answer 1"))
            .click();
        harness.run_steps(2);
        harness
            .get_by(|node| node.placeholder() == Some("Answer 1"))
            .type_text("Pizza");
        harness.key_press(Key::Enter);
        harness.run_steps(3);
        assert_eq!(harness.state().1.options, ["Pizza", "", ""]);
        // The new row has focus: Backspace in it, while empty, removes it again.
        harness.key_press(Key::Backspace);
        harness.run_steps(3);
        assert_eq!(harness.state().1.options, ["Pizza", ""]);
    }

    #[test]
    fn clicking_votes_and_takes_back() {
        assert_eq!(toggle(&[], 1, false), [1]);
        assert_eq!(toggle(&[1], 2, false), [2]);
        assert!(toggle(&[1], 1, false).is_empty());
        assert_eq!(toggle(&[2], 0, true), [0, 2]);
        assert_eq!(toggle(&[0, 2], 2, true), [0]);
    }
}
