# Listening to shared audio

Each shared app/system-audio publication starts silent. Open the person's sound
menu and choose **Listen** beside that share. **Stop listening** removes its local
playback queue immediately and unsubscribes it. Microphones continue normally.
The normal per-person/app volume, mute, block and deafen mixer remains in effect.

Screen sound also requires Listen. Watching alone never authorizes sound, even
when the stage automatically starts video after a layout change. Screen sound
still requires watching, because publishers grant it only to announced viewers.
Stopping video clears consent to its screen sound. Standalone apps are independent.

Consent is local, in memory, keyed by publication ID and participant identity plus
LiveKit participant session ID. Unpublication, participant departure, reconnect
and leaving the call discard it. Replacement publications (including bitrate
republishing) need Listen again, even when their name is unchanged. Labels and
watch announcements remain encrypted; no protocol field or outbound service was
added. Name decryption after a key update never grants consent.

## Path audit

- Initial join/late join: SDK auto-subscribe is off; reconciliation subscribes
  microphones but requires consent for every ScreenshareAudio publication.
- New publications, key sync and revocation/unblock reconciliation use that same
  policy. Unknown encrypted shared labels stay unsubscribed.
- TrackSubscribed events are checked again before reaching consumers; snapshots
  also filter consent. Late subscription events after Stop cannot restart playback.
- Watch only changes video subscriptions and reconciles sound. It never grants
  consent. Stopping Watch removes screen-sound consent and local queues.
- Reconnecting/Reconnected clear consent and reconcile subscriptions. The desktop
  removes shared playback on reconnect; call teardown clears the entire mixer.
- The desktop mixer independently rejects shared tracks without its local Listen
  flag. Stop clears the flag and queue under the mixer lock; pending PCM readers
  cannot reinsert them. Already submitted device samples cannot be recalled.

## Compatibility and limits

This is receiver-side consent, not a new cryptographic access boundary. Older
receivers may still auto-play standalone app audio. Existing publisher-side
restrictions for screen sound remain intact. Older senders using RustTalk's
ScreenshareAudio labels work with the new controls. Audio falsely labeled as a
microphone cannot be distinguished from conversation and retains microphone
behavior. New receivers require Listen even for older screen sound.

The screen/audio association in the existing protocol is participant-wide, not
an authenticated screen-publication ID. Therefore Watch is never itself consent.
Each sound publication requires its own Listen; stopping a screen clears that
participant's screen-sound consent. Volume preferences remain name-based, while
consent never is. Muting/deafening does not erase a deliberate Listen selection.

Synthetic media coverage lives in `crates/bot/tests/voice/consent.rs`: no desired
or actual shared subscription before consent/after Stop/republication, audible
990 Hz PCM after opt-in, and normal microphone PCM before opt-in. Existing app
and screen bot scenarios now explicitly opt in. This checks decoded media without
routing it to hardware speakers, not physical device latency.
