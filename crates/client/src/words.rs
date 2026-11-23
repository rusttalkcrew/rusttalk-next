//! Copying and pasting recovery words on the first-start screens. The words are
//! the account: a copy is marked sensitive and doesn't stay on the clipboard, and
//! pasted text is wiped once read and never logged.

use eframe::egui::{Context, Event, Id, Ui};
use zeroize::{Zeroize, Zeroizing};

/// How long copied words may stay on the clipboard.
pub const CLEAR_AFTER: std::time::Duration = std::time::Duration::from_secs(120);

/// The words in pasted text: split on spaces, new lines, commas and semicolons,
/// without numbering like "1." or "(1)" and in lower case.
pub fn split(text: &str) -> Vec<Zeroizing<String>> {
    text.split(|c: char| c.is_whitespace() || c == ',' || c == ';')
        // "1.apple" and "apple." both leave "apple"; "1." and "(1)" leave nothing.
        .map(|token| token.trim_matches(|c: char| !c.is_alphabetic()))
        .filter(|word| !word.is_empty())
        .map(|word| Zeroizing::new(word.to_lowercase()))
        .collect()
}

/// Pasted words into the word check's fields, asked for the words at `indices`
/// (from zero). The whole list fills each field with its own word; fewer words go
/// into the fields from `start` on.
pub fn fill(
    answers: &mut [Zeroizing<String>],
    indices: &[usize],
    pasted: &[Zeroizing<String>],
    start: usize,
) {
    let all = pasted.len() > indices.iter().copied().max().unwrap_or(0);
    for (slot, answer) in answers.iter_mut().enumerate() {
        let word = if all {
            indices.get(slot).and_then(|&index| pasted.get(index))
        } else {
            slot.checked_sub(start)
                .and_then(|offset| pasted.get(offset))
        };
        if let Some(word) = word {
            set(answer, word);
        }
    }
}

/// Pasted words into the one recovery-words field: a whole list (or into an empty
/// field) replaces what is there, a few more words go at the end.
pub fn append(words: &mut Zeroizing<String>, pasted: &[Zeroizing<String>], whole: usize) {
    if pasted.len() >= whole {
        words.clear();
    }
    for word in pasted {
        if !words.trim_end().is_empty() && !words.ends_with(' ') {
            words.push(' ');
        }
        words.push_str(word);
    }
}

/// Replace the text in place: `secret_input` reserved room for it, so no copy of
/// the old text is left behind in a freed allocation.
fn set(text: &mut Zeroizing<String>, word: &str) {
    text.clear();
    text.push_str(word);
}

fn wanted_id() -> Id {
    Id::unique("recovery words paste wanted")
}

/// The Paste button: ask for the clipboard's text, which `pasted` then takes.
pub fn request_paste(ctx: &Context) {
    ctx.data_mut(|data| data.insert_temp(wanted_id(), true));
    #[cfg(any(target_os = "android", target_os = "ios"))]
    crate::phone_clipboard::paste_for_screen(ctx);
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    ctx.send_viewport_cmd(eframe::egui::ViewportCommand::RequestPaste);
}

/// Words pasted this frame, taken before any field sees them: after the Paste
/// button, or a paste of several words into one of the fields (Ctrl+V). Call it
/// before showing the fields.
pub fn pasted(ui: &Ui) -> Option<Vec<Zeroizing<String>>> {
    let wanted = ui
        .data(|data| data.get_temp::<bool>(wanted_id()))
        .unwrap_or(false);
    let words = ui.input_mut(|input| {
        let at = input.events.iter().position(|event| match event {
            Event::Paste(text) => wanted || split(text).len() > 1,
            _ => false,
        })?;
        let Event::Paste(mut text) = input.events.remove(at) else {
            unreachable!("found above");
        };
        let words = split(&text);
        text.zeroize();
        // egui keeps the frame's raw input too.
        for event in &mut input.raw.events {
            if let Event::Paste(text) = event {
                text.zeroize();
            }
        }
        Some(words)
    })?;
    ui.data_mut(|data| data.remove_temp::<bool>(wanted_id()));
    Some(words)
}

/// The Copy button: the words to the clipboard, marked sensitive on phones and
/// taken off it again after two minutes (iOS and desktops).
pub fn copy(ctx: &Context, words: &str) {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = ctx;
        crate::phone_clipboard::copy_secret(Zeroizing::new(words.to_owned()));
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    desktop::copy(ctx, words);
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub use desktop::frame;

/// Desktops: two minutes after a copy, the clipboard is emptied if it still holds
/// the words (compared by hash), read and emptied in one step through the system
/// clipboard. A newer copy made in the app cancels it. Only while the app runs.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod desktop {
    use std::{sync::Mutex, time::Instant};

    use eframe::egui::{Context, OutputCommand};
    use sha2::{Digest, Sha256};
    use zeroize::Zeroizing;

    use super::CLEAR_AFTER;

    struct Clear {
        hash: [u8; 32],
        due: Instant,
        /// The pass that copied the words, whose own copy doesn't count as newer.
        pass: u64,
    }

    static CLEAR: Mutex<Option<Clear>> = Mutex::new(None);

    fn lock() -> std::sync::MutexGuard<'static, Option<Clear>> {
        CLEAR.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn hash(text: &str) -> [u8; 32] {
        Sha256::digest(text.as_bytes()).into()
    }

    pub fn copy(ctx: &Context, words: &str) {
        ctx.copy_text(words.to_owned());
        *lock() = Some(Clear {
            hash: hash(words),
            due: Instant::now() + CLEAR_AFTER,
            pass: ctx.cumulative_pass_nr(),
        });
        ctx.request_repaint_after(CLEAR_AFTER);
    }

    /// At the end of each frame (`App::ui`), after everything the frame copied.
    pub fn frame(ctx: &Context) {
        let mut clear = lock();
        let Some(state) = clear.as_ref() else { return };
        let newer = ctx.cumulative_pass_nr() != state.pass
            && ctx.output(|output| {
                (output.commands.iter())
                    .any(|command| matches!(command, OutputCommand::CopyText(_)))
            });
        let now = Instant::now();
        if !newer && now < state.due {
            ctx.request_repaint_after(state.due - now);
            return;
        }
        let words = state.hash;
        *clear = None;
        if newer {
            return;
        }
        // Off the window's thread: an X11 clipboard can take a moment to answer.
        std::thread::spawn(move || match arboard::Clipboard::new() {
            Ok(mut clipboard) => {
                clear_if_same(&mut clipboard, &words);
            }
            Err(error) => tracing::warn!(%error, "opening the clipboard to clear it failed"),
        });
    }

    /// The system clipboard, as far as clearing it needs.
    pub trait Board {
        fn text(&mut self) -> Option<Zeroizing<String>>;
        fn clear(&mut self) -> bool;
    }

    impl Board for arboard::Clipboard {
        fn text(&mut self) -> Option<Zeroizing<String>> {
            self.get_text().ok().map(Zeroizing::new)
        }

        fn clear(&mut self) -> bool {
            arboard::Clipboard::clear(self)
                .inspect_err(|error| tracing::warn!(%error, "clearing the clipboard failed"))
                .is_ok()
        }
    }

    /// Empty the clipboard only if it still holds the words; true when it did.
    pub fn clear_if_same(board: &mut impl Board, words: &[u8; 32]) -> bool {
        board.text().is_some_and(|text| hash(&text) == *words) && board.clear()
    }

    #[cfg(test)]
    pub fn pending() -> bool {
        lock().is_some()
    }

    #[cfg(test)]
    pub fn hash_of(text: &str) -> [u8; 32] {
        hash(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{OutputCommand, RawInput};

    fn texts(words: &[Zeroizing<String>]) -> Vec<&str> {
        words.iter().map(|word| word.as_str()).collect()
    }

    #[test]
    fn pasted_text_splits_into_words() {
        assert_eq!(
            texts(&split("Apple banana,cherry;\n\tdate  ")),
            ["apple", "banana", "cherry", "date"]
        );
        assert_eq!(
            texts(&split("1. apple\n2. banana\n3) cherry\n(4) date 5.egg")),
            ["apple", "banana", "cherry", "date", "egg"]
        );
        assert!(split(" 1. , \n").is_empty());
    }

    fn answers() -> [Zeroizing<String>; 3] {
        std::array::from_fn(|_| crate::key_storage::secret_input())
    }

    #[test]
    fn the_whole_list_fills_each_field_with_its_own_word() {
        let list: Vec<String> = (b'a'..b'a' + 24)
            .map(|c| format!("{}. word{}", c - b'a' + 1, c as char))
            .collect();
        let mut answers = answers();
        fill(&mut answers, &[2, 9, 20], &split(&list.join("\n")), 1);
        assert_eq!(texts(&answers), ["wordc", "wordj", "wordu"]);
    }

    #[test]
    fn a_few_words_fill_from_the_field() {
        let mut answers = answers();
        answers[0].push_str("kept");
        fill(&mut answers, &[2, 9, 20], &split("one two three"), 1);
        assert_eq!(texts(&answers), ["kept", "one", "two"]);
    }

    #[test]
    fn restore_pastes_replace_a_whole_list_and_add_a_few() {
        let mut words = crate::key_storage::secret_input();
        words.push_str("apple ");
        append(&mut words, &split("banana cherry"), 24);
        assert_eq!(*words, "apple banana cherry");
        let list = vec!["zoo"; 24].join("\n");
        append(&mut words, &split(&list), 24);
        assert_eq!(*words, vec!["zoo"; 24].join(" "));
    }

    #[test]
    fn the_paste_button_takes_the_paste_before_a_field_does() {
        let ctx = Context::default();
        let run = |events: Vec<Event>| {
            let mut field = String::new();
            let mut taken = None;
            let mut output = ctx.run_ui(
                RawInput {
                    events,
                    ..RawInput::default()
                },
                |ui| {
                    taken = pasted(ui);
                    ui.text_edit_singleline(&mut field).request_focus();
                },
            );
            output.textures_delta.clear();
            (taken.map(|words| texts(&words).join(" ")), field)
        };
        run(Vec::new());
        // One word without the button goes to the field as usual.
        let (taken, field) = run(vec![Event::Paste("apple".into())]);
        assert_eq!((taken, field.as_str()), (None, "apple"));
        // Several words are taken.
        let (taken, field) = run(vec![Event::Paste("1. apple 2. banana".into())]);
        assert_eq!(
            (taken.as_deref(), field.as_str()),
            (Some("apple banana"), "")
        );
        // After the button, also one word.
        ctx.data_mut(|data| data.insert_temp(wanted_id(), true));
        let (taken, _) = run(vec![Event::Paste("cherry".into())]);
        assert_eq!(taken.as_deref(), Some("cherry"));
    }

    #[test]
    fn copied_words_are_cleared_only_while_still_there() {
        struct Fake(Option<String>);
        impl desktop::Board for Fake {
            fn text(&mut self) -> Option<Zeroizing<String>> {
                self.0.clone().map(Zeroizing::new)
            }
            fn clear(&mut self) -> bool {
                self.0 = None;
                true
            }
        }
        let words = desktop::hash_of("apple banana");
        let mut board = Fake(Some("apple banana".into()));
        assert!(desktop::clear_if_same(&mut board, &words));
        assert_eq!(board.0, None);
        // Copied over by another app since: it stays.
        let mut board = Fake(Some("other".into()));
        assert!(!desktop::clear_if_same(&mut board, &words));
        assert_eq!(board.0.as_deref(), Some("other"));
    }

    #[test]
    fn a_newer_copy_in_the_app_cancels_the_clearing() {
        let ctx = Context::default();
        let run = |copy: Option<&str>, words: bool| {
            let mut output = ctx.run_ui(RawInput::default(), |ui| {
                if words {
                    super::copy(ui.ctx(), "apple banana");
                }
                if let Some(text) = copy {
                    ui.ctx().copy_text(text.to_owned());
                }
                frame(ui.ctx());
            });
            output.textures_delta.clear();
            output.platform_output.commands
        };
        let commands = run(None, true);
        assert!(commands.contains(&OutputCommand::CopyText("apple banana".into())));
        assert!(desktop::pending(), "its own copy is not newer");
        run(None, false);
        assert!(desktop::pending());
        run(Some("hello"), false);
        assert!(!desktop::pending(), "a newer copy stays on the clipboard");
    }
}
