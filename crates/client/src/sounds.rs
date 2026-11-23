use std::collections::HashSet;

use livekit::prelude::TrackSid;
use serde::{Deserialize, Serialize};

use crate::server::Id;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cue {
    YouJoined,
    YouLeft,
    PeerJoined,
    PeerLeft,
    StreamStarted,
    StreamStopped,
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
    pub const ALL: [Self; 14] = [
        Self::YouJoined,
        Self::YouLeft,
        Self::PeerJoined,
        Self::PeerLeft,
        Self::StreamStarted,
        Self::StreamStopped,
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
            Self::PeerJoined => t!("sounds.peer_joined"),
            Self::PeerLeft => t!("sounds.peer_left"),
            Self::StreamStarted => t!("sounds.stream_started"),
            Self::StreamStopped => t!("sounds.stream_stopped"),
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
            Self::PeerJoined => &[523.25, 659.25],
            Self::PeerLeft => &[523.25, 392.0],
            Self::StreamStarted => &[349.23, 523.25, 698.46],
            Self::StreamStopped => &[698.46, 523.25, 349.23],
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

#[derive(Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Preferences {
    pub enabled: bool,
    pub volume: f32,
    pub disabled: Vec<Cue>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            enabled: true,
            volume: 0.5,
            disabled: Vec::new(),
        }
    }
}

#[derive(Default)]
pub struct Snapshot {
    pub people: HashSet<Id>,
    pub camera: Option<TrackSid>,
    pub screen: Option<TrackSid>,
    pub viewers: HashSet<(Id, TrackSid)>,
}

impl Snapshot {
    pub fn changes(&self, previous: Option<&Self>) -> Vec<Cue> {
        let Some(previous) = previous else {
            return vec![Cue::YouJoined];
        };
        let mut cues = Vec::new();
        cues.extend(
            self.people
                .difference(&previous.people)
                .map(|_| Cue::PeerJoined),
        );
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
}

impl Detector {
    pub fn update(&mut self, snapshot: Snapshot) -> Vec<Cue> {
        let cues = if std::mem::take(&mut self.resuming) {
            Vec::new()
        } else {
            snapshot.changes(self.previous.as_ref())
        };
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

    pub fn end(&mut self) -> Option<Cue> {
        let joined = self.previous.is_some();
        *self = Self::default();
        joined.then_some(Cue::YouLeft)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn reconnect_reseeds_and_leaving_plays_once_after_a_successful_join() {
        let mut detector = Detector::default();
        assert_eq!(detector.end(), None);
        assert_eq!(detector.update(Snapshot::default()), [Cue::YouJoined]);
        detector.reconnecting();
        assert!(
            detector
                .update(Snapshot {
                    people: HashSet::from([[1; 32]]),
                    ..Snapshot::default()
                })
                .is_empty()
        );
        detector.reconnecting();
        assert_eq!(detector.end(), Some(Cue::YouLeft));
        assert_eq!(detector.end(), None);
        assert_eq!(detector.update(Snapshot::default()), [Cue::YouJoined]);
    }

    #[test]
    fn joining_seeds_the_roster_and_only_actual_changes_play() {
        let mut roster = Snapshot {
            people: HashSet::from([[1; 32], [2; 32], [3; 32]]),
            ..Snapshot::default()
        };
        assert_eq!(roster.changes(None), [Cue::YouJoined]);
        assert!(roster.changes(Some(&roster)).is_empty());
        let next = Snapshot {
            people: HashSet::from([[1; 32], [2; 32], [4; 32]]),
            ..Snapshot::default()
        };
        assert_eq!(
            next.changes(Some(&roster)),
            [Cue::PeerJoined, Cue::PeerLeft]
        );
        roster.people = next.people.clone();
        assert!(next.changes(Some(&roster)).is_empty());
    }

    #[test]
    fn a_local_stop_followed_by_a_room_update_only_plays_once() {
        let sid = track("screen");
        let mut detector = Detector::default();
        detector.update(Snapshot {
            screen: Some(sid.clone()),
            viewers: HashSet::from([([1; 32], sid.clone())]),
            ..Snapshot::default()
        });
        assert_eq!(detector.stop_video(&sid), Some(Cue::StreamStopped));
        assert_eq!(detector.stop_video(&sid), None);
        assert!(detector.update(Snapshot::default()).is_empty());
    }

    #[test]
    fn streams_are_independent_and_republishing_is_quiet() {
        let empty = Snapshot::default();
        let mut screen = Snapshot {
            screen: Some(track("screen")),
            ..Snapshot::default()
        };
        assert_eq!(screen.changes(Some(&empty)), [Cue::StreamStarted]);
        let both = Snapshot {
            camera: Some(track("camera")),
            screen: screen.screen.clone(),
            ..Snapshot::default()
        };
        assert_eq!(both.changes(Some(&screen)), [Cue::StreamStarted]);
        assert_eq!(screen.changes(Some(&both)), [Cue::StreamStopped]);
        assert_eq!(empty.changes(Some(&screen)), [Cue::StreamStopped]);
        screen.camera = Some(track("replacement"));
        assert!(screen.changes(Some(&both)).is_empty());
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
        assert_eq!(watched.changes(Some(&screen)), [Cue::ViewerStarted]);
        assert!(watched.changes(Some(&watched)).is_empty());
        assert_eq!(screen.changes(Some(&watched)), [Cue::ViewerStopped]);
        assert_eq!(
            Snapshot::default().changes(Some(&watched)),
            [Cue::StreamStopped]
        );
    }
}
