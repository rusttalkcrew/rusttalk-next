//! The voice call as the phone's own call (CallKit on iOS, `ios/callkit.rs`): joining
//! starts it, connecting reports it connected, mute and leave follow the app, and the
//! system's End and Mute (lock screen, Dynamic Island, Control Center, another phone
//! call) leave or mute the voice call. The phone's side sits behind `System`, so the
//! mapping is tested here without a phone.

use std::collections::VecDeque;

use crate::{
    call,
    home::{Home, user_bar},
    server::{CallState, Id},
};

/// What the phone does with the call. Requests are asynchronous; refusals come back
/// as `Event::Refused`.
pub trait System {
    /// The phone's name for one call.
    type Call: Clone + PartialEq;
    /// Start an outgoing call shown as `name`.
    fn start(&mut self, name: &str, video: bool) -> Self::Call;
    fn connected(&mut self, call: &Self::Call);
    fn set_video(&mut self, call: &Self::Call, on: bool);
    fn set_muted(&mut self, call: &Self::Call, muted: bool);
    fn end(&mut self, call: &Self::Call);
}

/// What the phone reported.
#[derive(Clone)]
pub enum Event<C> {
    /// The person ended the call outside the app.
    Ended(C),
    /// Muted or unmuted outside the app, or the phone carrying out our own request.
    Muted(C, bool),
    /// The phone refused to start the call, or to mute it.
    Refused(C, Request),
    /// The phone dropped every call.
    Reset,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Request {
    Start,
    Mute,
}

/// What the app has to do for the phone.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Effect {
    Leave,
    Mute(bool),
}

/// The app's voice call, as the phone should show it.
#[derive(Clone, Debug, PartialEq)]
pub struct Want {
    pub room: (url::Url, Id),
    pub name: String,
    pub connected: bool,
    pub muted: bool,
    pub video: bool,
}

/// The call the phone was told about.
struct Shown<C> {
    call: C,
    room: (url::Url, Id),
    /// False once the phone refused to start it: nothing more is asked for it.
    live: bool,
    connected: bool,
    muted: bool,
    video: bool,
    /// Our mute requests the phone has yet to carry out; they come back as `Muted`.
    echoes: VecDeque<bool>,
}

pub struct CallKit<S: System> {
    pub system: S,
    shown: Option<Shown<S::Call>>,
}

impl<S: System> CallKit<S> {
    pub fn new(system: S) -> Self {
        Self {
            system,
            shown: None,
        }
    }

    /// Something the phone reported; what the app must do about it, if anything.
    pub fn event(&mut self, event: Event<S::Call>) -> Option<Effect> {
        match event {
            Event::Reset => self.shown.take().map(|_| Effect::Leave),
            Event::Ended(call) => {
                self.shown.take_if(|s| s.call == call)?;
                Some(Effect::Leave)
            }
            Event::Muted(call, muted) => {
                let shown = self.shown.as_mut().filter(|s| s.call == call)?;
                if shown.echoes.front() == Some(&muted) {
                    shown.echoes.pop_front();
                    return None;
                }
                shown.muted = muted;
                Some(Effect::Mute(muted))
            }
            Event::Refused(call, request) => {
                let shown = self.shown.as_mut().filter(|s| s.call == call)?;
                match request {
                    Request::Start => shown.live = false,
                    Request::Mute => {
                        shown.echoes.pop_front();
                    }
                }
                None
            }
        }
    }

    /// Bring the phone's call in line with the app's.
    pub fn sync(&mut self, want: Option<Want>) {
        if let Some(shown) = &self.shown
            && want.as_ref().is_none_or(|want| want.room != shown.room)
        {
            if shown.live {
                self.system.end(&shown.call);
            }
            self.shown = None;
        }
        let Some(want) = want else { return };
        let shown = self.shown.get_or_insert_with(|| Shown {
            call: self.system.start(&want.name, want.video),
            room: want.room.clone(),
            live: true,
            connected: false,
            muted: false,
            video: want.video,
            echoes: VecDeque::new(),
        });
        if !shown.live {
            return;
        }
        if want.connected && !shown.connected {
            shown.connected = true;
            self.system.connected(&shown.call);
        }
        if want.muted != shown.muted {
            shown.muted = want.muted;
            shown.echoes.push_back(want.muted);
            self.system.set_muted(&shown.call, want.muted);
        }
        if want.video != shown.video {
            shown.video = want.video;
            self.system.set_video(&shown.call, want.video);
        }
    }
}

/// The app's call as the phone should show it: the channel and server name, as in the
/// call panel.
pub fn want(home: &Home) -> Option<Want> {
    let call = home.voice.call.as_ref()?;
    let server = home.servers.iter().find(|s| s.address == call.server);
    let name = match server {
        Some(server) => match server.chat_title(&call.channel) {
            Some((name, _)) => format!("{name} / {}", server.name()),
            None => server.name().to_owned(),
        },
        None => "RustTalk".to_owned(),
    };
    let audio = &home.voice.audio;
    Some(Want {
        room: (call.server.clone(), call.channel),
        name,
        connected: call.state != CallState::Connecting,
        muted: audio.muted() || audio.deafened() || user_bar::server_muted(home),
        video: home.voice.camera.on,
    })
}

/// Carry out what the phone asked for.
pub fn apply(home: &mut Home, effect: Effect) {
    match effect {
        Effect::Leave => call::leave(home),
        Effect::Mute(on) => user_bar::set_mute(home, on),
    }
}

/// Once a frame: what the phone asked for, then the app's call to the phone.
pub fn frame<S: System>(home: &mut Home, kit: &mut CallKit<S>, effects: Vec<Effect>) {
    for effect in effects {
        apply(home, effect);
    }
    kit.sync(want(home));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::Command;

    /// Records what the app asked the phone for.
    #[derive(Default)]
    struct Fake {
        next: u32,
        log: Vec<String>,
    }

    impl System for Fake {
        type Call = u32;
        fn start(&mut self, name: &str, video: bool) -> u32 {
            self.next += 1;
            self.log
                .push(format!("start {} {name} video={video}", self.next));
            self.next
        }
        fn connected(&mut self, call: &u32) {
            self.log.push(format!("connected {call}"));
        }
        fn set_video(&mut self, call: &u32, on: bool) {
            self.log.push(format!("video {call} {on}"));
        }
        fn set_muted(&mut self, call: &u32, muted: bool) {
            self.log.push(format!("muted {call} {muted}"));
        }
        fn end(&mut self, call: &u32) {
            self.log.push(format!("end {call}"));
        }
    }

    fn home() -> (
        Home,
        tokio::sync::mpsc::UnboundedReceiver<Command>,
        tempfile::TempDir,
    ) {
        let ctx = eframe::egui::Context::default();
        let (_, identity) = rusttalk_core::identity::DeviceIdentity::create("Phone").unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut home = Home::new(identity, &ctx, data.path().to_owned()).unwrap();
        let (commands, sent) = tokio::sync::mpsc::unbounded_channel();
        let known = rusttalk_core::auth::KnownServer {
            address: "https://friends.example.org/".parse().unwrap(),
            server_id: [2; 32],
            tls_fingerprint: None,
        };
        let mut server = crate::home::new_server(known, "Friends", commands);
        server.status = crate::server::Status::Online;
        home.servers.push(server);
        (home, sent, data)
    }

    fn join(home: &mut Home, channel: Id, state: CallState) {
        home.voice.call = Some(crate::server::Call {
            server: home.servers[0].address.clone(),
            channel,
            state,
            stats: Default::default(),
            can_talk: true,
            can_share_video: true,
        });
    }

    fn take(kit: &mut CallKit<Fake>) -> Vec<String> {
        std::mem::take(&mut kit.system.log)
    }

    #[test]
    fn joining_starts_and_leaving_ends_the_phone_call() {
        let (mut home, _sent, _data) = home();
        let mut kit = CallKit::new(Fake::default());
        frame(&mut home, &mut kit, vec![]);
        assert!(take(&mut kit).is_empty());

        join(&mut home, [1; 32], CallState::Connecting);
        frame(&mut home, &mut kit, vec![]);
        assert_eq!(take(&mut kit), ["start 1 Friends video=false"]);
        frame(&mut home, &mut kit, vec![]);
        assert!(take(&mut kit).is_empty());

        home.voice.call.as_mut().unwrap().state = CallState::Connected;
        frame(&mut home, &mut kit, vec![]);
        assert_eq!(take(&mut kit), ["connected 1"]);

        // Muting in the app shows on the phone; the phone's echo changes nothing.
        user_bar::toggle_mute(&home);
        frame(&mut home, &mut kit, vec![]);
        assert_eq!(take(&mut kit), ["muted 1 true"]);
        assert_eq!(kit.event(Event::Muted(1, true)), None);
        assert!(home.voice.audio.muted());

        // Another room: the old phone call ends, a new one starts.
        join(&mut home, [2; 32], CallState::Connected);
        frame(&mut home, &mut kit, vec![]);
        let log = take(&mut kit);
        assert_eq!(log[0], "end 1");
        assert!(log[1].starts_with("start 2 "));
        assert_eq!(&log[2..], ["connected 2", "muted 2 true"]);

        call::leave(&mut home);
        frame(&mut home, &mut kit, vec![]);
        assert_eq!(take(&mut kit), ["end 2"]);
        // The phone carrying out our own End is not the person leaving again.
        assert_eq!(kit.event(Event::Ended(2)), None);
    }

    #[test]
    fn the_phones_end_and_mute_reach_the_call() {
        let (mut home, mut sent, _data) = home();
        let mut kit = CallKit::new(Fake::default());
        join(&mut home, [1; 32], CallState::Connected);
        frame(&mut home, &mut kit, vec![]);
        take(&mut kit);

        let mute = kit.event(Event::Muted(1, true)).unwrap();
        frame(&mut home, &mut kit, vec![mute]);
        assert!(home.voice.audio.muted());
        assert!(matches!(sent.try_recv(), Ok(Command::SyncVoiceStatus)));
        // Already muted on the phone: nothing to send back.
        assert!(take(&mut kit).is_empty());

        let unmute = kit.event(Event::Muted(1, false)).unwrap();
        frame(&mut home, &mut kit, vec![unmute]);
        assert!(!home.voice.audio.muted());
        assert!(take(&mut kit).is_empty());

        // A stale call's events are ignored.
        assert_eq!(kit.event(Event::Ended(7)), None);
        assert_eq!(kit.event(Event::Muted(7, true)), None);

        let end = kit.event(Event::Ended(1)).unwrap();
        assert_eq!(end, Effect::Leave);
        while sent.try_recv().is_ok() {}
        frame(&mut home, &mut kit, vec![end]);
        assert!(home.voice.call.is_none());
        assert!(matches!(sent.try_recv(), Ok(Command::LeaveVoice)));
        // The phone ended it already: no End request back.
        assert!(take(&mut kit).is_empty());
    }

    #[test]
    fn quick_mute_changes_and_refusals_keep_in_step() {
        let (mut home, _sent, _data) = home();
        let mut kit = CallKit::new(Fake::default());
        join(&mut home, [1; 32], CallState::Connected);
        frame(&mut home, &mut kit, vec![]);
        user_bar::toggle_mute(&home);
        frame(&mut home, &mut kit, vec![]);
        user_bar::toggle_mute(&home);
        frame(&mut home, &mut kit, vec![]);
        take(&mut kit);
        // The first echo arrives after the second change: it doesn't mute the app again.
        assert_eq!(kit.event(Event::Muted(1, true)), None);
        // The second request was refused; a later mute on the phone is the person's.
        assert_eq!(kit.event(Event::Refused(1, Request::Mute)), None);
        assert_eq!(kit.event(Event::Muted(1, true)), Some(Effect::Mute(true)));
        assert!(!home.voice.audio.muted());

        // A refused start asks for nothing more about that call, not even End.
        assert_eq!(kit.event(Event::Refused(1, Request::Start)), None);
        user_bar::toggle_mute(&home);
        call::leave(&mut home);
        frame(&mut home, &mut kit, vec![]);
        assert!(take(&mut kit).is_empty());

        // The phone resetting leaves the call.
        join(&mut home, [1; 32], CallState::Connected);
        frame(&mut home, &mut kit, vec![]);
        assert_eq!(kit.event(Event::Reset), Some(Effect::Leave));
        assert_eq!(kit.event(Event::Reset), None);
    }
}
