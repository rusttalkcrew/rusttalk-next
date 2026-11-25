//! Android's ongoing-call notification (`CallService`): what it shows, and its Mute
//! and Hang up buttons. In the background Android takes the app's window away and no
//! frame runs, so a button acts at once on the audio and the server connection, and
//! the next frame brings the rest of the app along (`frame`).

use std::sync::{Mutex, MutexGuard};

use tokio::sync::mpsc::UnboundedSender;

use crate::{
    audio::Audio,
    home::{Home, user_bar},
    net::Command,
    server::{Id, Server},
    sounds::Cue,
};

/// A notification button, numbered as in `CallService`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Mute = 1,
    HangUp = 2,
}

impl Button {
    pub fn from_code(code: i32) -> Option<Self> {
        [Self::Mute, Self::HangUp]
            .into_iter()
            .find(|button| *button as i32 == code)
    }
}

/// What the notification shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shown {
    /// The channel, or the other person in a DM call.
    pub name: String,
    /// The server, for a channel.
    pub server: String,
    /// Muted or deafened.
    pub muted: bool,
    /// No Mute button while a moderator holds the mic off.
    pub can_mute: bool,
}

/// What the buttons act on while no frame runs.
struct Controls {
    audio: Audio,
    commands: UnboundedSender<Command>,
    can_mute: bool,
}

impl Controls {
    /// Like the call's own buttons; returns whether the mic is off now.
    fn press(&self, button: Button) -> bool {
        match button {
            Button::Mute if self.can_mute => {
                user_bar::toggle_mic(&self.audio);
                Server::send_to(&self.commands, Command::SyncVoiceStatus);
            }
            Button::Mute => {}
            Button::HangUp => {
                Server::send_to(&self.commands, Command::LeaveVoice);
                self.audio.end_voice(Some(Cue::YouLeft));
            }
        }
        self.audio.muted() || self.audio.deafened()
    }
}

struct State {
    /// `None` out of a call.
    controls: Option<Controls>,
    /// Last sent to Java.
    shown: Option<Shown>,
    /// Hang up was pressed; the next frame ends the call in the app.
    hung_up: bool,
    /// The notification was tapped; the next frame shows the call.
    open: bool,
}

static STATE: Mutex<State> = Mutex::new(State {
    controls: None,
    shown: None,
    hung_up: false,
    open: false,
});

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// A notification button (on Android's main thread). Returns whether the mic is off.
pub fn press(code: i32) -> bool {
    let mut state = state();
    let (Some(button), Some(controls)) = (Button::from_code(code), &state.controls) else {
        return false;
    };
    let muted = controls.press(button);
    if button == Button::HangUp {
        state.controls = None;
        state.hung_up = true;
        drop(state);
        // Back to normal audio, and the service and its notification stop.
        #[cfg(target_os = "android")]
        crate::android::communication_mode(false);
    }
    muted
}

/// The notification was tapped.
pub fn open() {
    state().open = true;
}

/// Each frame: catch up with what the buttons did, and return what the notification
/// should show when that changed.
pub fn frame(home: &mut Home) -> Option<Shown> {
    let mut state = state();
    if std::mem::take(&mut state.hung_up) && home.voice.call.take().is_some() {
        // The button already left on the server and played the leave sound.
        home.voice.sounds.end();
        home.voice.end_call();
    }
    let call = home
        .voice
        .call
        .as_ref()
        .map(|c| (c.server.clone(), c.channel));
    if std::mem::take(&mut state.open)
        && let Some((server, channel)) = &call
    {
        home.open_chat(server, *channel);
    }
    let server = call
        .as_ref()
        .and_then(|(address, _)| home.servers.iter().find(|s| s.address == *address));
    let (Some((_, channel)), Some(server)) = (call, server) else {
        state.controls = None;
        state.shown = None;
        return None;
    };
    let audio = &home.voice.audio;
    let can_mute = !server.voice_muted(&server.dir.me);
    state.controls = Some(Controls {
        audio: audio.clone(),
        commands: server.commands.clone(),
        can_mute,
    });
    let shown = Shown {
        muted: audio.muted() || audio.deafened(),
        can_mute,
        ..names(server, &channel)
    };
    if state.shown.as_ref() == Some(&shown) {
        return None;
    }
    state.shown = Some(shown.clone());
    Some(shown)
}

fn names(server: &Server, channel: &Id) -> Shown {
    if let Some(dm) = server.dir.dms.iter().find(|dm| dm.channel == *channel) {
        return Shown {
            name: server.member_name(&dm.peer),
            ..Shown::default()
        };
    }
    Shown {
        name: server
            .channel(channel)
            .map(|c| c.name.clone())
            .unwrap_or_default(),
        server: server.name().into(),
        ..Shown::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_codes_match_call_service() {
        assert_eq!(Button::from_code(1), Some(Button::Mute));
        assert_eq!(Button::from_code(2), Some(Button::HangUp));
        for unknown in [0, 3, -1] {
            assert_eq!(Button::from_code(unknown), None);
        }
    }
}
