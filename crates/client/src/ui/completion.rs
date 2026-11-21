use eframe::egui::{
    Align, Button, CompletionOutput, CompletionQuery, Event, EventFilter, Id, Key, LayerId, Layout,
    Popup, PopupKind, RectAlign, ScrollArea, Suggestion, SurrenderFocusOn, TextEdit, TextWrapMode,
    Ui, Vec2,
    text::{CCursor, CCursorRange, CharIndex},
};

use super::{MENU_ROW, MENU_WIDTH, SPACE_XS};

#[derive(Clone, Default)]
struct Selection {
    word: String,
    start: CharIndex,
    index: usize,
    open: bool,
    dismissed: bool,
}

/// A compact suggestion popup that leaves keyboard focus with the editor.
pub fn completion(
    ui: &mut Ui,
    id: Id,
    edit: TextEdit<'_>,
    word_boundary: impl Fn(char) -> bool,
    suggestions: impl FnOnce(&Ui, &CompletionQuery<'_>) -> Vec<Suggestion>,
) -> CompletionOutput {
    let mut state: Selection = ui.data(|data| data.get_temp(id)).unwrap_or_default();
    let popup_id = id.with("completion-popup");
    let has_focus = ui.memory(|memory| memory.has_focus(id));
    let popup_layer = LayerId::new(PopupKind::Popup.order(), popup_id);
    let pointer_on_popup = state.open
        && ui
            .ctx()
            .pointer_interact_pos()
            .and_then(|pos| ui.ctx().layer_id_at(pos))
            == Some(popup_layer);

    let (mut step, mut accept, mut dismiss) = (0_isize, false, false);
    if state.open && has_focus {
        ui.input_mut(|input| {
            input.events.retain(|event| {
                let Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } = event
                else {
                    return true;
                };
                if !modifiers.is_none() {
                    return true;
                }
                match key {
                    Key::ArrowDown => step += 1,
                    Key::ArrowUp => step -= 1,
                    Key::Enter | Key::Tab => accept = true,
                    Key::Escape => dismiss = true,
                    _ => return true,
                }
                false
            });
        });
    }

    // Clicking a suggestion or its scrollbar must keep the editor's caret.
    let surrender = ui
        .ctx()
        .options(|options| options.input_options.surrender_focus_on);
    if pointer_on_popup {
        ui.ctx().options_mut(|options| {
            options.input_options.surrender_focus_on = SurrenderFocusOn::Never;
        });
    }
    let filter = edit.get_event_filter();
    let (mut output, buffer) = edit.id(id).show_returning_text(ui);
    if pointer_on_popup {
        ui.ctx()
            .options_mut(|options| options.input_options.surrender_focus_on = surrender);
    }

    let response = &mut output.response.response;
    let end = output
        .cursor_range
        .map_or(CharIndex(0), |cursor| cursor.primary.index);
    let start = buffer
        .as_str()
        .chars()
        .take(end.0)
        .enumerate()
        .filter(|&(_, c)| word_boundary(c))
        .last()
        .map_or(CharIndex(0), |(index, _)| CharIndex(index + 1));
    let range = start..end;
    let word = buffer.char_range(range.clone());
    if state.word != word || state.start != start || !response.has_focus() {
        state.word = word.to_owned();
        state.start = start;
        state.index = 0;
        state.dismissed = false;
    }
    state.dismissed |= dismiss;

    let mut items = if response.has_focus()
        && !state.dismissed
        && output.cursor_range.is_some_and(|cursor| cursor.is_empty())
    {
        suggestions(
            ui,
            &CompletionQuery {
                text: buffer.as_str(),
                word,
                word_range: range.clone(),
            },
        )
    } else {
        Vec::new()
    };
    if !items.is_empty() {
        state.index = (state.index as isize + step).rem_euclid(items.len() as isize) as usize;
    }
    let mut accepted = (accept && !items.is_empty()).then_some(state.index);
    let is_open = !items.is_empty() && accepted.is_none();
    if is_open {
        let width = MENU_WIDTH.min(ui.ctx().content_rect().width());
        let clicked = Popup::from_response(response)
            .id(popup_id)
            .align(RectAlign::TOP_START)
            .width(width)
            .layout(Layout::top_down_justified(Align::Min))
            .accessible_name(if word.starts_with('@') {
                t!("completion.mentions")
            } else {
                t!("completion.emoji")
            })
            .show(|ui| {
                ui.set_width(width);
                ui.spacing_mut().item_spacing.y = SPACE_XS;
                ScrollArea::vertical()
                    .max_height(5.0 * MENU_ROW + 4.0 * SPACE_XS)
                    .show(ui, |ui| {
                        let mut clicked = None;
                        for (index, item) in items.iter().enumerate() {
                            let row = ui.add(
                                Button::selectable(index == state.index, item.content.clone())
                                    .min_size(Vec2::new(ui.available_width(), MENU_ROW))
                                    .wrap_mode(TextWrapMode::Truncate),
                            );
                            if row.clicked() {
                                clicked = Some(index);
                            }
                            if index == state.index && step != 0 {
                                row.scroll_to_me(None);
                            }
                        }
                        clicked
                    })
                    .inner
            })
            .and_then(|inner| inner.inner);
        accepted = clicked;
    }

    let accepted = accepted.map(|index| items.swap_remove(index));
    if let Some(item) = &accepted {
        buffer.delete_char_range(range);
        let mut cursor = CCursor::new(start);
        buffer.insert_text_at(&mut cursor, &item.insert, usize::MAX);
        let mut edit_state = TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
        edit_state
            .cursor
            .set_char_range(Some(CCursorRange::one(cursor)));
        edit_state.store(ui.ctx(), id);
        response.mark_changed();
        response.request_focus();
    }
    state.open = is_open && accepted.is_none();
    ui.memory_mut(|memory| {
        memory.set_focus_lock_filter(
            id,
            EventFilter {
                tab: filter.tab || state.open,
                escape: filter.escape || state.open,
                ..filter
            },
        )
    });
    let is_open = state.open;
    ui.data_mut(|data| data.insert_temp(id, state));
    CompletionOutput {
        text_edit: output,
        accepted,
        is_open,
    }
}
