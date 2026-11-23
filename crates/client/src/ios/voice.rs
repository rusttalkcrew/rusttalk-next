//! The app owns the audio session. cpal's RemoteIO units use its active route.
use std::{
    ptr::NonNull,
    sync::{
        Mutex, Once,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Result, anyhow};
use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_avf_audio::{
    AVAudioSession, AVAudioSessionCategoryOptions, AVAudioSessionCategoryPlayAndRecord,
    AVAudioSessionMediaServicesWereResetNotification, AVAudioSessionModeVoiceChat,
    AVAudioSessionRecordPermission, AVAudioSessionSetActiveOptions,
};
use objc2_foundation::{NSNotification, NSNotificationCenter};

// AVAudioApplication needs iOS 17. RustTalk supports iOS 16.
#[allow(deprecated)]
pub fn voice_permission(request: bool) -> i32 {
    // SAFETY: Called by the main-thread join flow. iOS copies the completion block.
    unsafe {
        let session = AVAudioSession::sharedInstance();
        match session.recordPermission() {
            AVAudioSessionRecordPermission::Granted => {
                tracing::info!("iOS microphone permission granted");
                1
            }
            AVAudioSessionRecordPermission::Undetermined => {
                if request {
                    tracing::info!("iOS microphone permission requested");
                    session.requestRecordPermission(&RcBlock::new(|_: Bool| ()));
                }
                0
            }
            _ => {
                tracing::info!("iOS microphone permission denied");
                -1
            }
        }
    }
}

// Switching rooms can start a new call before the old leave cue has finished.
static USERS: Mutex<usize> = Mutex::new(0);
pub struct Session(());

static RESET: AtomicBool = AtomicBool::new(false);

pub fn needs_rejoin() -> bool {
    RESET.load(Ordering::Relaxed)
}

impl Session {
    pub fn start() -> Result<Self> {
        let mut users = USERS.lock().unwrap();
        // SAFETY: The framework constants and selectors are available on iOS 16.
        unsafe {
            static OBSERVE_RESET: Once = Once::new();
            OBSERVE_RESET.call_once(|| {
                // NotificationCenter retains this process-wide observer. The callback may
                // run off the main thread and touches only the atomic flag.
                let block =
                    RcBlock::new(|_: NonNull<NSNotification>| RESET.store(true, Ordering::Relaxed));
                let _observer = NSNotificationCenter::defaultCenter()
                    .addObserverForName_object_queue_usingBlock(
                        Some(
                            AVAudioSessionMediaServicesWereResetNotification
                                .expect("available since iOS 6"),
                        ),
                        None,
                        None,
                        &block,
                    );
            });
            // Only an explicit join starts recording again after a media-services reset.
            RESET.store(false, Ordering::Relaxed);
            let session = AVAudioSession::sharedInstance();
            session
                .setCategory_mode_options_error(
                    AVAudioSessionCategoryPlayAndRecord
                        .ok_or_else(|| anyhow!("missing PlayAndRecord category"))?,
                    AVAudioSessionModeVoiceChat.ok_or_else(|| anyhow!("missing VoiceChat mode"))?,
                    AVAudioSessionCategoryOptions::AllowBluetoothHFP
                        | AVAudioSessionCategoryOptions::DefaultToSpeaker,
                )
                .map_err(|e| anyhow!("set PlayAndRecord/VoiceChat: {e}"))?;
            session
                .setActive_error(true)
                .map_err(|e| anyhow!("activate PlayAndRecord/VoiceChat: {e}"))?;
            // Port type identifies the route without logging a person's Bluetooth device name.
            let input = session
                .currentRoute()
                .inputs()
                .firstObject()
                .map(|port| port.portType().to_string());
            tracing::info!(?input, "iOS PlayAndRecord/VoiceChat active");
        }
        *users += 1;
        Ok(Self(()))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let mut users = USERS.lock().unwrap();
        *users -= 1;
        if *users == 0 {
            // SAFETY: AVAudioSession supports activation on the audio worker. Its streams
            // have closed and the lock serializes deactivation with the next join.
            if let Err(error) = unsafe {
                AVAudioSession::sharedInstance().setActive_withOptions_error(
                    false,
                    AVAudioSessionSetActiveOptions::NotifyOthersOnDeactivation,
                )
            } {
                tracing::warn!(%error, "deactivate iOS voice session failed");
            }
        }
    }
}
