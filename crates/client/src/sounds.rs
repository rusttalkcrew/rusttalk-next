use std::{
    collections::{HashMap, HashSet},
    io::Cursor,
    sync::OnceLock,
};

use livekit::prelude::TrackSid;
use serde::{Deserialize, Serialize};

use crate::server::Id;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cue {
    YouJoined,
    YouLeft,
    /// Moved from one voice channel straight into another, in place of a leave and a join.
    ChannelChanged,
    /// Joined a channel where this person may only listen, in place of the join sound.
    ListenOnly,
    /// The call was ended by the server or a lost connection, not by this person.
    Dropped,
    Reconnecting,
    Reconnected,
    /// A moderator muted or unmuted this person for the server.
    ServerMuted,
    ServerUnmuted,
    PeerJoined,
    PeerLeft,
    /// Someone on this person's friends list joins: decided on the device, from the
    /// friends list only it knows.
    FriendJoined,
    StreamStarted,
    StreamStopped,
    /// Someone else in the call starts a camera or screen share.
    PeerStreamStarted,
    ViewerStarted,
    ViewerStopped,
    Deafened,
    Undeafened,
    Muted,
    Unmuted,
    Ring,
    Message,
}

impl Cue {
    pub const ALL: [Self; 23] = [
        Self::YouJoined,
        Self::YouLeft,
        Self::ChannelChanged,
        Self::ListenOnly,
        Self::Dropped,
        Self::Reconnecting,
        Self::Reconnected,
        Self::ServerMuted,
        Self::ServerUnmuted,
        Self::PeerJoined,
        Self::PeerLeft,
        Self::FriendJoined,
        Self::StreamStarted,
        Self::StreamStopped,
        Self::PeerStreamStarted,
        Self::ViewerStarted,
        Self::ViewerStopped,
        Self::Deafened,
        Self::Undeafened,
        Self::Muted,
        Self::Unmuted,
        Self::Ring,
        Self::Message,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::YouJoined => t!("sounds.you_joined"),
            Self::YouLeft => t!("sounds.you_left"),
            Self::ChannelChanged => t!("sounds.channel_changed"),
            Self::ListenOnly => t!("sounds.listen_only"),
            Self::Dropped => t!("sounds.dropped"),
            Self::Reconnecting => t!("sounds.reconnecting"),
            Self::Reconnected => t!("sounds.reconnected"),
            Self::ServerMuted => t!("sounds.server_muted"),
            Self::ServerUnmuted => t!("sounds.server_unmuted"),
            Self::PeerJoined => t!("sounds.peer_joined"),
            Self::PeerLeft => t!("sounds.peer_left"),
            Self::FriendJoined => t!("sounds.friend_joined"),
            Self::StreamStarted => t!("sounds.stream_started"),
            Self::StreamStopped => t!("sounds.stream_stopped"),
            Self::PeerStreamStarted => t!("sounds.peer_stream_started"),
            Self::ViewerStarted => t!("sounds.viewer_started"),
            Self::ViewerStopped => t!("sounds.viewer_stopped"),
            Self::Deafened => t!("sounds.deafened"),
            Self::Undeafened => t!("sounds.undeafened"),
            Self::Muted => t!("sounds.muted"),
            Self::Unmuted => t!("sounds.unmuted"),
            Self::Ring => t!("sounds.ring"),
            Self::Message => t!("sounds.message"),
        }
    }

    /// Original sine chimes with a soft attack, decay and short gap between notes.
    pub fn samples(self, rate: u32, volume: f32) -> impl Iterator<Item = i16> {
        let notes: &[f32] = match self {
            Self::YouJoined => &[392.0, 523.25, 659.25],
            Self::YouLeft => &[659.25, 523.25, 392.0],
            // Down and back up: out of one channel and into the next.
            Self::ChannelChanged => &[523.25, 392.0, 523.25],
            // The join chime's first two notes, then a low one: in, but quiet.
            Self::ListenOnly => &[392.0, 523.25, 329.63],
            // The leave chime ending lower than any other.
            Self::Dropped => &[523.25, 349.23, 261.63],
            // A repeated note with a rest: waiting.
            Self::Reconnecting => &[440.0, 0.0, 440.0],
            Self::Reconnected => &[440.0, 587.33],
            // The mute and unmute notes, twice: someone else did it.
            Self::ServerMuted => &[369.99, 369.99],
            Self::ServerUnmuted => &[554.37, 554.37],
            Self::PeerJoined => &[523.25, 659.25],
            Self::PeerLeft => &[523.25, 392.0],
            // A someone-joins chime with one more note on top.
            Self::FriendJoined => &[523.25, 659.25, 783.99],
            Self::StreamStarted => &[349.23, 523.25, 698.46],
            Self::StreamStopped => &[698.46, 523.25, 349.23],
            Self::PeerStreamStarted => &[523.25, 698.46],
            Self::ViewerStarted => &[783.99, 880.0],
            Self::ViewerStopped => &[880.0, 698.46],
            // Lower than every other chime, so they are not mistaken for a join or leave.
            Self::Deafened => &[293.66, 220.0],
            Self::Undeafened => &[220.0, 293.66],
            // A single note, so it is not mistaken for any two- or three-note chime.
            Self::Muted => &[369.99],
            Self::Unmuted => &[554.37],
            // Two short trills like a small phone, with a rest (0 Hz) between.
            Self::Ring => &[
                783.99, 987.77, 783.99, 987.77, 0.0, 783.99, 987.77, 783.99, 987.77,
            ],
            // Higher than the call chimes, so a message is not mistaken for one.
            Self::Message => &[1046.5, 1318.51],
        };
        let volume = volume.clamp(0.0, 1.0);
        notes.iter().flat_map(move |&hz| {
            (0..rate / 12).map(move |i| {
                let secs = i as f32 / rate as f32;
                let gain = (secs / 0.008).min(1.0) * (1.0 - secs / 0.075).max(0.0).powi(2);
                let phase = std::f32::consts::TAU * hz * secs;
                ((phase.sin() + 0.15 * (2.0 * phase).sin()) * gain * volume * 6000.0) as i16
            })
        })
    }
}

/// The bundled lines of one pack folder in `assets/voice/`, one Ogg file per cue,
/// named like the cue in `phrases.toml`.
macro_rules! lines {
    ($pack:literal: $($cue:ident => $file:literal),* $(,)?) => {
        &[$((
            Cue::$cue,
            include_bytes!(concat!("../assets/voice/", $pack, "/", $file, ".ogg")) as &[u8],
        )),*]
    };
}

/// A pack with a line for every cue.
macro_rules! every_line {
    ($pack:literal) => {
        lines!($pack:
            YouJoined => "you_joined",
            YouLeft => "you_left",
            ChannelChanged => "channel_changed",
            ListenOnly => "listen_only",
            Dropped => "dropped",
            Reconnecting => "reconnecting",
            Reconnected => "reconnected",
            ServerMuted => "server_muted",
            ServerUnmuted => "server_unmuted",
            PeerJoined => "peer_joined",
            PeerLeft => "peer_left",
            FriendJoined => "friend_joined",
            StreamStarted => "stream_started",
            StreamStopped => "stream_stopped",
            PeerStreamStarted => "peer_stream_started",
            ViewerStarted => "viewer_started",
            ViewerStopped => "viewer_stopped",
            Deafened => "deafened",
            Undeafened => "undeafened",
            Muted => "muted",
            Unmuted => "unmuted",
            Ring => "ring",
            Message => "message",
        )
    };
}

/// Spoken announcements played in place of the chimes. Rendered ahead of time
/// (see docs/voice-packs.md), so nothing is generated or downloaded on the device.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VoicePack {
    EnglishHeart,
    EnglishMichael,
    /// Also for a pack this version doesn't know, from a newer app's settings.
    #[default]
    #[serde(other)]
    Off,
}

impl VoicePack {
    pub const ALL: [Self; 3] = [Self::Off, Self::EnglishHeart, Self::EnglishMichael];

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => t!("sounds.voice_off"),
            Self::EnglishHeart => t!("sounds.voice_english_heart"),
            Self::EnglishMichael => t!("sounds.voice_english_michael"),
        }
    }

    /// Its encoded lines. A cue missing here keeps its chime.
    fn files(self) -> &'static [(Cue, &'static [u8])] {
        match self {
            Self::Off => &[],
            Self::EnglishHeart => every_line!("en_heart"),
            Self::EnglishMichael => every_line!("en_michael"),
        }
    }

    /// The line for `cue` as 48 kHz mono samples, decoded once on first use.
    pub fn line(self, cue: Cue) -> Option<&'static [i16]> {
        // People without a pack never pay for decoding one.
        if self == Self::Off {
            return None;
        }
        static DECODED: OnceLock<HashMap<(VoicePack, Cue), Vec<i16>>> = OnceLock::new();
        let decoded = DECODED.get_or_init(|| {
            (Self::ALL.iter())
                .flat_map(|&pack| {
                    pack.files()
                        .iter()
                        .map(move |&(cue, file)| (pack, cue, file))
                })
                .filter_map(|(pack, cue, file)| match decode(file) {
                    Ok(samples) => Some(((pack, cue), samples)),
                    Err(error) => {
                        tracing::warn!("voice line {pack:?} {cue:?}: {error:#}");
                        None
                    }
                })
                .collect()
        });
        decoded.get(&(self, cue)).map(Vec::as_slice)
    }
}

fn decode(file: &[u8]) -> anyhow::Result<Vec<i16>> {
    let mut reader = lewton::inside_ogg::OggStreamReader::new(Cursor::new(file))?;
    let header = &reader.ident_hdr;
    anyhow::ensure!(
        header.audio_sample_rate == crate::audio::RATE && header.audio_channels == 1,
        "not 48 kHz mono"
    );
    let mut samples = Vec::new();
    while let Some(packet) = reader.read_dec_packet_itl()? {
        samples.extend(packet);
    }
    Ok(samples)
}

#[derive(Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Preferences {
    pub enabled: bool,
    pub volume: f32,
    #[serde(deserialize_with = "known_cues")]
    pub disabled: Vec<Cue>,
    pub voice: VoicePack,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            enabled: true,
            volume: 0.5,
            disabled: Vec::new(),
            voice: VoicePack::Off,
        }
    }
}

/// Skips sounds this version doesn't know, so settings saved by a newer app still load.
fn known_cues<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<Cue>, D::Error> {
    let values = Vec::<serde_json::Value>::deserialize(deserializer)?;
    Ok(values
        .into_iter()
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect())
}

impl Preferences {
    /// What plays for `cue`, or `None` when it is switched off. A friend joining
    /// with that sound off still plays the normal someone-joins sound.
    pub fn sound(&self, cue: Cue) -> Option<Box<dyn Iterator<Item = i16>>> {
        let off = |cue| self.disabled.contains(&cue);
        let cue = match cue {
            Cue::FriendJoined if off(cue) => Cue::PeerJoined,
            cue => cue,
        };
        if !self.enabled || self.volume <= 0.0 || off(cue) {
            return None;
        }
        let volume = self.volume.clamp(0.0, 1.0);
        Some(match self.voice.line(cue) {
            Some(line) => Box::new(line.iter().map(move |&s| (f32::from(s) * volume) as i16)),
            None => Box::new(cue.samples(crate::audio::RATE, volume)),
        })
    }
}

#[derive(Default)]
pub struct Snapshot {
    pub people: HashSet<Id>,
    pub camera: Option<TrackSid>,
    pub screen: Option<TrackSid>,
    pub viewers: HashSet<(Id, TrackSid)>,
    /// Other people's camera and screen shares.
    pub streams: HashSet<TrackSid>,
}

impl Snapshot {
    /// `friend` tells whether someone is on this person's friends list.
    pub fn changes(&self, previous: Option<&Self>, friend: impl Fn(&Id) -> bool) -> Vec<Cue> {
        let Some(previous) = previous else {
            return vec![Cue::YouJoined];
        };
        let mut cues = Vec::new();
        cues.extend(self.people.difference(&previous.people).map(|id| {
            if friend(id) {
                Cue::FriendJoined
            } else {
                Cue::PeerJoined
            }
        }));
        cues.extend(
            previous
                .people
                .difference(&self.people)
                .map(|_| Cue::PeerLeft),
        );
        for (before, after) in [
            (&previous.camera, &self.camera),
            (&previous.screen, &self.screen),
        ] {
            match (before, after) {
                (None, Some(_)) => cues.push(Cue::StreamStarted),
                (Some(_), None) => cues.push(Cue::StreamStopped),
                _ => {}
            }
        }
        let continuing = |sid: &TrackSid| {
            (self.camera.as_ref() == Some(sid) && self.camera == previous.camera)
                || (self.screen.as_ref() == Some(sid) && self.screen == previous.screen)
        };
        cues.extend(
            self.streams
                .difference(&previous.streams)
                .map(|_| Cue::PeerStreamStarted),
        );
        cues.extend(
            self.viewers
                .difference(&previous.viewers)
                .map(|_| Cue::ViewerStarted),
        );
        cues.extend(
            previous
                .viewers
                .difference(&self.viewers)
                .filter(|(_, sid)| continuing(sid))
                .map(|_| Cue::ViewerStopped),
        );
        cues
    }
}

#[derive(Default)]
pub struct Detector {
    previous: Option<Snapshot>,
    resuming: bool,
    /// The next `end` is a leave for a switch to another channel.
    switching: bool,
    /// The left call was for a switch, so the next join is a channel change.
    switched: bool,
    /// The new call only lets this person listen.
    listen_only: bool,
    /// The next `end` comes from the server or the connection, not this person.
    dropped: bool,
}

impl Detector {
    pub fn update(&mut self, snapshot: Snapshot, friend: impl Fn(&Id) -> bool) -> Vec<Cue> {
        let mut cues = if std::mem::take(&mut self.resuming) {
            Vec::new()
        } else {
            snapshot.changes(self.previous.as_ref(), friend)
        };
        let switched = std::mem::take(&mut self.switched);
        let listen_only = std::mem::take(&mut self.listen_only);
        for cue in &mut cues {
            if *cue == Cue::YouJoined {
                *cue = match (listen_only, switched) {
                    (true, _) => Cue::ListenOnly,
                    (false, true) => Cue::ChannelChanged,
                    (false, false) => Cue::YouJoined,
                };
            }
        }
        self.previous = Some(snapshot);
        cues
    }

    pub fn stop_video(&mut self, sid: &TrackSid) -> Option<Cue> {
        let snapshot = self.previous.as_mut()?;
        let active = [&mut snapshot.camera, &mut snapshot.screen]
            .into_iter()
            .find(|track| track.as_ref() == Some(sid))?;
        *active = None;
        snapshot.viewers.retain(|(_, track)| track != sid);
        Some(Cue::StreamStopped)
    }

    pub fn reconnecting(&mut self) {
        self.resuming = self.previous.is_some();
    }

    /// The call is left to join another channel: stay quiet until the new one is joined.
    pub fn switching(&mut self) {
        self.switching = self.previous.is_some();
    }

    /// The joining call lets this person only listen; its join sound says so.
    pub fn listen_only(&mut self) {
        self.listen_only = self.previous.is_none();
    }

    /// The server or the connection is ending the call.
    pub fn dropped(&mut self) {
        self.dropped = true;
    }

    /// A moderator moved this person after the server already ended the old call:
    /// the next join is a channel change.
    pub fn moved(&mut self) {
        self.switched = true;
    }

    pub fn end(&mut self) -> Option<Cue> {
        let joined = self.previous.is_some();
        let (switching, dropped) = (self.switching, self.dropped);
        *self = Self::default();
        self.switched = switching;
        match (joined, switching, dropped) {
            (false, ..) | (true, true, _) => None,
            (true, false, true) => Some(Cue::Dropped),
            (true, false, false) => Some(Cue::YouLeft),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_friends(_: &Id) -> bool {
        false
    }

    fn track(id: &str) -> TrackSid {
        TrackSid::try_from(format!("TR_{id}")).unwrap()
    }

    #[test]
    fn every_cue_sounds_different() {
        let sounds: Vec<Vec<i16>> = (Cue::ALL.iter())
            .map(|cue| cue.samples(48_000, 0.5).collect())
            .collect();
        for (index, sound) in sounds.iter().enumerate() {
            assert!(!sound.is_empty(), "{:?}", Cue::ALL[index]);
            for other in &sounds[index + 1..] {
                assert_ne!(sound, other, "{:?}", Cue::ALL[index]);
            }
        }
    }

    #[test]
    fn friends_joining_get_their_own_sound() {
        let empty = Snapshot::default();
        let joined = Snapshot {
            people: HashSet::from([[1; 32], [2; 32]]),
            ..Snapshot::default()
        };
        let mut cues = joined.changes(Some(&empty), |id| *id == [2; 32]);
        cues.sort_by_key(|cue| *cue as u8);
        assert_eq!(cues, [Cue::PeerJoined, Cue::FriendJoined]);
    }

    #[test]
    fn a_friend_with_their_sound_off_plays_the_normal_join() {
        let mut prefs = Preferences::default();
        let sound = |prefs: &Preferences, cue| prefs.sound(cue).map(Iterator::collect::<Vec<_>>);
        assert_ne!(
            sound(&prefs, Cue::FriendJoined),
            sound(&prefs, Cue::PeerJoined)
        );
        prefs.disabled.push(Cue::FriendJoined);
        assert_eq!(
            sound(&prefs, Cue::FriendJoined),
            sound(&prefs, Cue::PeerJoined)
        );
        prefs.disabled.push(Cue::PeerJoined);
        assert!(sound(&prefs, Cue::FriendJoined).is_none());
    }

    #[test]
    fn a_voice_pack_replaces_the_chimes() {
        let mut prefs = Preferences::default();
        let chime: Vec<_> = prefs.sound(Cue::Muted).unwrap().collect();
        prefs.voice = VoicePack::EnglishHeart;
        let voice: Vec<_> = prefs.sound(Cue::Muted).unwrap().collect();
        assert_ne!(chime, voice);
        assert!(voice.len() > crate::audio::RATE as usize / 4);
        let newer = r#"{"voice":"klingon","disabled":["muted","hand_raised"]}"#;
        let newer: Preferences = serde_json::from_str(newer).unwrap();
        assert_eq!(newer.voice, VoicePack::Off);
        assert_eq!(newer.disabled, [Cue::Muted]);
    }

    /// Every pack's files decode and match the lines its `phrases.toml` lists, so a
    /// line added there without rendering or listing it here fails.
    #[test]
    fn voice_packs_match_their_phrases() {
        for pack in VoicePack::ALL {
            let folder = match pack {
                VoicePack::Off => continue,
                VoicePack::EnglishHeart => "en_heart",
                VoicePack::EnglishMichael => "en_michael",
            };
            let phrases = std::fs::read_to_string(format!(
                "{}/assets/voice/{folder}/phrases.toml",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap();
            let mut listed: Vec<String> = (phrases.lines())
                .skip_while(|line| line.trim() != "[lines]")
                .filter_map(|line| Some(line.split_once('=')?.0.trim().to_owned()))
                .collect();
            let mut bundled: Vec<String> = (pack.files().iter())
                .map(|(cue, _)| {
                    serde_json::to_value(cue)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .to_owned()
                })
                .collect();
            listed.sort();
            bundled.sort();
            assert_eq!(listed, bundled, "{pack:?}");
            for &(cue, _) in pack.files() {
                assert!(
                    pack.line(cue).is_some_and(|line| !line.is_empty()),
                    "{pack:?} {cue:?}"
                );
            }
        }
    }

    #[test]
    fn switching_channels_plays_one_change_instead_of_leave_and_join() {
        let mut detector = Detector::default();
        detector.switching();
        assert_eq!(detector.end(), None);
        assert_eq!(
            detector.update(Snapshot::default(), no_friends),
            [Cue::YouJoined]
        );
        detector.switching();
        assert_eq!(detector.end(), None);
        assert_eq!(
            detector.update(Snapshot::default(), no_friends),
            [Cue::ChannelChanged]
        );
        // A switch whose join fails leaves nothing behind for the next join.
        detector.switching();
        detector.end();
        detector.end();
        assert_eq!(
            detector.update(Snapshot::default(), no_friends),
            [Cue::YouJoined]
        );
        assert_eq!(detector.end(), Some(Cue::YouLeft));
    }

    #[test]
    fn dropped_calls_moves_and_listen_only_joins_have_their_own_sounds() {
        let mut detector = Detector::default();
        detector.listen_only();
        assert_eq!(
            detector.update(Snapshot::default(), no_friends),
            [Cue::ListenOnly]
        );
        detector.dropped();
        assert_eq!(detector.end(), Some(Cue::Dropped));
        // Moved by a moderator after the server ended the old call.
        detector.moved();
        assert_eq!(
            detector.update(Snapshot::default(), no_friends),
            [Cue::ChannelChanged]
        );
        assert_eq!(detector.end(), Some(Cue::YouLeft));
    }

    #[test]
    fn other_peoples_new_streams_play_once() {
        let empty = Snapshot::default();
        let streaming = Snapshot {
            streams: HashSet::from([track("theirs")]),
            ..Snapshot::default()
        };
        assert_eq!(
            streaming.changes(Some(&empty), no_friends),
            [Cue::PeerStreamStarted]
        );
        assert!(streaming.changes(Some(&streaming), no_friends).is_empty());
        assert!(empty.changes(Some(&streaming), no_friends).is_empty());
    }

    #[test]
    fn reconnect_reseeds_and_leaving_plays_once_after_a_successful_join() {
        let mut detector = Detector::default();
        assert_eq!(detector.end(), None);
        assert_eq!(
            detector.update(Snapshot::default(), no_friends),
            [Cue::YouJoined]
        );
        detector.reconnecting();
        assert!(
            detector
                .update(
                    Snapshot {
                        people: HashSet::from([[1; 32]]),
                        ..Snapshot::default()
                    },
                    no_friends
                )
                .is_empty()
        );
        detector.reconnecting();
        assert_eq!(detector.end(), Some(Cue::YouLeft));
        assert_eq!(detector.end(), None);
        assert_eq!(
            detector.update(Snapshot::default(), no_friends),
            [Cue::YouJoined]
        );
    }

    #[test]
    fn joining_seeds_the_roster_and_only_actual_changes_play() {
        let mut roster = Snapshot {
            people: HashSet::from([[1; 32], [2; 32], [3; 32]]),
            ..Snapshot::default()
        };
        assert_eq!(roster.changes(None, no_friends), [Cue::YouJoined]);
        assert!(roster.changes(Some(&roster), no_friends).is_empty());
        let next = Snapshot {
            people: HashSet::from([[1; 32], [2; 32], [4; 32]]),
            ..Snapshot::default()
        };
        assert_eq!(
            next.changes(Some(&roster), no_friends),
            [Cue::PeerJoined, Cue::PeerLeft]
        );
        roster.people = next.people.clone();
        assert!(next.changes(Some(&roster), no_friends).is_empty());
    }

    #[test]
    fn a_local_stop_followed_by_a_room_update_only_plays_once() {
        let sid = track("screen");
        let mut detector = Detector::default();
        detector.update(
            Snapshot {
                screen: Some(sid.clone()),
                viewers: HashSet::from([([1; 32], sid.clone())]),
                ..Snapshot::default()
            },
            no_friends,
        );
        assert_eq!(detector.stop_video(&sid), Some(Cue::StreamStopped));
        assert_eq!(detector.stop_video(&sid), None);
        assert!(detector.update(Snapshot::default(), no_friends).is_empty());
    }

    #[test]
    fn streams_are_independent_and_republishing_is_quiet() {
        let empty = Snapshot::default();
        let mut screen = Snapshot {
            screen: Some(track("screen")),
            ..Snapshot::default()
        };
        assert_eq!(
            screen.changes(Some(&empty), no_friends),
            [Cue::StreamStarted]
        );
        let both = Snapshot {
            camera: Some(track("camera")),
            screen: screen.screen.clone(),
            ..Snapshot::default()
        };
        assert_eq!(
            both.changes(Some(&screen), no_friends),
            [Cue::StreamStarted]
        );
        assert_eq!(
            screen.changes(Some(&both), no_friends),
            [Cue::StreamStopped]
        );
        assert_eq!(
            empty.changes(Some(&screen), no_friends),
            [Cue::StreamStopped]
        );
        screen.camera = Some(track("replacement"));
        assert!(screen.changes(Some(&both), no_friends).is_empty());
    }

    #[test]
    fn watching_changes_only_play_for_a_continuing_local_track() {
        let sid = track("screen");
        let screen = Snapshot {
            screen: Some(sid.clone()),
            ..Snapshot::default()
        };
        let watched = Snapshot {
            screen: Some(sid.clone()),
            viewers: HashSet::from([([1; 32], sid)]),
            ..Snapshot::default()
        };
        assert_eq!(
            watched.changes(Some(&screen), no_friends),
            [Cue::ViewerStarted]
        );
        assert!(watched.changes(Some(&watched), no_friends).is_empty());
        assert_eq!(
            screen.changes(Some(&watched), no_friends),
            [Cue::ViewerStopped]
        );
        assert_eq!(
            Snapshot::default().changes(Some(&watched), no_friends),
            [Cue::StreamStopped]
        );
    }
}
