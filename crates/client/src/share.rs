//! App audio capture for sharing in a voice room.

use anyhow::Result;
use rusttalk_core::{capture, client::voice::NativeAudioSource};

pub use capture::{App, Capture};

/// macOS: System Audio Recording is off for RustTalk.
pub fn blocked(error: &anyhow::Error) -> bool {
    error.is::<capture::NoAccess>()
}

/// The text for a share that failed: macOS says when access is off.
pub fn failed(error: &anyhow::Error, app: &App) -> String {
    if blocked(error) {
        t!("voice.app_audio_blocked").into()
    } else {
        t!("voice.app_failed", app = app.name)
    }
}

/// False on Windows builds that can't capture single apps (Windows 10).
pub fn supported() -> bool {
    capture::supported().is_ok()
}

/// Apps playing sound now. Starts a helper process on Linux; keep it off the UI thread.
pub fn list() -> Result<Vec<App>> {
    // Never offer our own playback: sharing it would echo the call.
    // shortcut: PipeWire only shows our streams by cpal's client name, so this breaks if cpal
    // renames it; then match the stream's process ID through its bound client instead.
    let own = format!("cpal-pulseaudio-{}", std::process::id());
    let mut apps = capture::list_apps()?;
    apps.retain(|app| app.name != own);
    Ok(apps)
}

/// "All system audio": everything this computer plays except RustTalk itself.
pub fn system() -> App {
    App {
        id: capture::SYSTEM.into(),
        name: t!("call.all_system_audio").into(),
    }
}

pub fn is_system(app: &App) -> bool {
    app.id == capture::SYSTEM
}

/// Start capturing an app into a new source for its track. Blocks until audio flows.
pub fn capture(app: &App) -> Result<(Capture, NativeAudioSource)> {
    let source = capture::audio_source();
    let capture = capture::capture_to_source(app, source.clone())?;
    Ok((capture, source))
}
