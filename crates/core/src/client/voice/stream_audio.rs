//! Which audio to receive, and who may receive the sound of our screen share.
//!
//! A screen share's sound is meant for the people watching it. Receivers only ask for
//! it while they watch, but media uses the channel key, so an older client that takes
//! every audio track could still listen. The publisher therefore also allows each track
//! only to the members who announced (see `watch`) that they watch the share.

use std::collections::HashMap;

use anyhow::Result;
use livekit::{
    e2ee::EncryptionType,
    participant::ParticipantTrackPermission,
    prelude::{RemoteTrackPublication, TrackKind},
};

use super::{ParticipantIdentity, TrackSid, TrackSource, VoiceRoom, track_name};
use crate::keys::Key;

/// Who may receive which of our tracks. `None` lets everyone have everything; otherwise
/// each member gets all tracks (`None`) or only the listed ones.
pub(super) type Permissions = Option<Vec<(ParticipantIdentity, Option<Vec<TrackSid>>)>>;

/// Whether to receive an audio track: voices always, shared apps once their name
/// opens as app audio, and a share's sound only while watching the share. A label we
/// cannot open yet (a newer epoch's key is still on its way) stays off until it opens.
fn wanted(
    source: TrackSource,
    name: &str,
    app_names: &HashMap<i64, Key>,
    stream_names: &HashMap<i64, Key>,
    screen_watched: bool,
) -> bool {
    if source != TrackSource::ScreenshareAudio {
        return true;
    }
    if track_name::decrypt(app_names, track_name::APP_AUDIO, name).is_some() {
        return true;
    }
    screen_watched && track_name::decrypt(stream_names, track_name::STREAM_AUDIO, name).is_some()
}

pub(super) fn is_stream_audio(
    publication: &RemoteTrackPublication,
    stream_names: &HashMap<i64, Key>,
) -> bool {
    publication.source() == TrackSource::ScreenshareAudio
        && track_name::decrypt(stream_names, track_name::STREAM_AUDIO, &publication.name())
            .is_some()
}

/// Subscribe a publication when `wanted`; never unsubscribes (see `set_subscribed`).
pub(super) fn subscribe_audio(
    publication: &RemoteTrackPublication,
    app_names: &HashMap<i64, Key>,
    stream_names: &HashMap<i64, Key>,
    screen_watched: bool,
) {
    if publication.kind() == TrackKind::Audio
        && publication.encryption_type() == EncryptionType::Gcm
        && !publication.is_desired()
        && wanted(
            publication.source(),
            &publication.name(),
            app_names,
            stream_names,
            screen_watched,
        )
    {
        publication.set_subscribed(true);
    }
}

/// While any of our tracks is stream sound (or `restrict` is set, just before
/// publishing one), members who don't watch our share get every other track by ID.
fn permissions(
    restrict: bool,
    local: &[(TrackSid, bool)],
    members: &[(ParticipantIdentity, bool)],
) -> Permissions {
    if !restrict && !local.iter().any(|(_, sound)| *sound) {
        return None;
    }
    let mut others: Vec<_> = (local.iter())
        .filter(|(_, sound)| !sound)
        .map(|(sid, _)| sid.clone())
        .collect();
    others.sort();
    let mut list: Vec<_> = (members.iter())
        .map(|(id, watching)| (id.clone(), (!watching).then(|| others.clone())))
        .collect();
    list.sort_by(|a, b| a.0.cmp(&b.0));
    Some(list)
}

impl VoiceRoom {
    /// Ask for every audio track we want now: after a key arrives, a name may open.
    pub(super) fn reconcile_audio(&self) {
        for participant in self.room.remote_participants().into_values() {
            let publications = participant.track_publications();
            let watched = (publications.values())
                .any(|p| p.source() == TrackSource::Screenshare && p.is_desired());
            for publication in publications.values() {
                subscribe_audio(publication, &self.name_keys, &self.stream_names, watched);
            }
        }
    }

    /// Allow our share's sound only to its viewers; see the module comment.
    pub(super) async fn sync_stream_permissions(&self, restrict: bool) -> Result<()> {
        let local_participant = self.room.local_participant();
        let publications = local_participant.track_publications();
        let screen = (publications.values())
            .find(|p| p.source() == TrackSource::Screenshare)
            .map(|p| p.sid());
        let local: Vec<_> = (publications.values())
            .map(|p| {
                let sound = p.source() == TrackSource::ScreenshareAudio
                    && track_name::decrypt(&self.stream_names, track_name::STREAM_AUDIO, &p.name())
                        .is_some();
                (p.sid(), sound)
            })
            .collect();
        let members: Vec<_> = (self.room.remote_participants().into_values())
            .map(|member| {
                let watching = screen
                    .as_ref()
                    .is_some_and(|screen| self.video_watches(&member).contains(screen));
                (member.identity(), watching)
            })
            .collect();
        let wanted = permissions(restrict, &local, &members);
        {
            let mut sent = self.permissions.lock().expect("permissions lock");
            if sent.as_ref() == Some(&wanted) {
                return Ok(());
            }
            *sent = Some(wanted.clone());
        }
        let list = (wanted.iter().flatten())
            .map(|(id, allowed)| ParticipantTrackPermission {
                participant_identity: id.clone(),
                allow_all: allowed.is_none(),
                allowed_track_sids: allowed.clone().unwrap_or_default(),
            })
            .collect();
        local_participant
            .set_track_subscription_permissions(wanted.is_none(), list)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use zeroize::Zeroizing;

    use super::*;

    fn sid(s: &str) -> TrackSid {
        TrackSid::try_from(format!("TR_{s}")).unwrap()
    }

    fn id(s: &str) -> ParticipantIdentity {
        ParticipantIdentity::from(s.to_owned())
    }

    #[test]
    fn audio_is_received_only_once_its_label_opens() {
        let old = HashMap::from([(3, Zeroizing::new([1; 32]))]);
        let new = Zeroizing::new([2; 32]);
        let both = HashMap::from([(3, old[&3].clone()), (4, new.clone())]);
        let app = track_name::encrypt(&new, track_name::APP_AUDIO, 4, "Spotify").unwrap();
        let sound = track_name::encrypt(&new, track_name::STREAM_AUDIO, 4, "Game").unwrap();
        let mic = TrackSource::Microphone;
        let shared = TrackSource::ScreenshareAudio;
        assert!(wanted(mic, "microphone", &old, &old, false));
        // Published in epoch 4 before its key arrived: neither app audio nor sound yet.
        assert!(!wanted(shared, &app, &old, &old, false));
        assert!(!wanted(shared, &sound, &old, &old, true));
        // With the key, the app plays; the share's sound only while watching.
        assert!(wanted(shared, &app, &both, &both, false));
        assert!(!wanted(shared, &sound, &both, &both, false));
        assert!(wanted(shared, &sound, &both, &both, true));
        assert!(!wanted(shared, "plain name", &both, &both, true));
    }

    #[test]
    fn only_viewers_may_receive_the_share_sound() {
        let local = [(sid("mic"), false), (sid("screen"), false)];
        let members = [(id("b"), true), (id("a"), false)];
        // No sound published: nothing is restricted.
        assert_eq!(permissions(false, &local, &members), None);
        let others = Some(vec![sid("mic"), sid("screen")]);
        let expected = Some(vec![(id("a"), others.clone()), (id("b"), None)]);
        // About to publish sound: non-viewers already get only the other tracks.
        assert_eq!(permissions(true, &local, &members), expected);
        let with_sound = [local[0].clone(), local[1].clone(), (sid("sound"), true)];
        assert_eq!(permissions(false, &with_sound, &members), expected);
        assert_eq!(
            permissions(false, &with_sound, &[]),
            Some(Vec::new()),
            "alone: nobody may subscribe to anything, and newcomers are re-checked"
        );
    }
}
