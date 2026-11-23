//! The RustTalk app. A library, so phones can load it: `main.rs` starts it on
//! desktops, `android_main` on Android.

// On phones, desktop code without a phone version yet is unused (docs/mobile.md).
#![cfg_attr(
    any(target_os = "android", target_os = "ios"),
    allow(dead_code, unused_imports)
)]

#[macro_use]
mod i18n;

mod admin;
#[cfg(any(target_os = "android", test))]
mod android;
mod app;
mod audio;
mod call;
mod camera;
mod crop;
mod denoise;
mod device_sync;
mod dm;
mod emoji;
mod emoji_zip;
mod files;
mod friends;
mod home;
mod hotkeys;
#[cfg(target_os = "ios")]
mod ios;
mod key_storage;
mod keystore;
mod link_cards;
mod log_file;
mod mic_check;
mod net;
mod notify;
mod official;
mod paste;
#[cfg(any(target_os = "android", target_os = "ios", test))]
mod phone_clipboard;
mod pokes;
mod previews;
mod profile;
mod reorder;
mod screen;
mod server;
mod settings;
mod share;
mod sounds;
mod sticker_files;
mod terminal;
mod ui;
mod update;
#[cfg(target_os = "linux")]
mod wayland_drops;
mod whats_new;
mod words;

use std::{
    cell::Cell,
    env, fs,
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Mutex,
};

use anyhow::{Context, Result, anyhow, bail};
use eframe::egui::ViewportBuilder;
use rusttalk_core::identity::fingerprint;
use tracing::Level;
use tracing_subscriber::{filter::Targets, prelude::*};

/// Read a JSON settings file; a missing file gives the default.
pub fn load_json<T: serde::de::DeserializeOwned + Default>(file: &Path) -> Result<T> {
    match fs::read(file) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("read {}", file.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).with_context(|| format!("open {}", file.display())),
    }
}

/// The running Android activity: the window and the app's private folder come from it.
#[cfg(target_os = "android")]
type Android = winit::platform::android::activity::AndroidApp;
#[cfg(not(target_os = "android"))]
type Android = ();

/// Android's GameActivity loads this library and calls this on its own thread, again
/// for each new activity while the process lives.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: Android) {
    android::init(app.clone());
    start(Some(app));
    // winit 0.30 makes one event loop per process, so a later activity couldn't
    // start in this one; Android starts a fresh process for it. `_exit`: the exit
    // handlers call into Java from this thread, which Android aborts.
    // SAFETY: ends the process at once; nothing runs after it.
    unsafe { libc::_exit(0) }
}

pub fn main() -> ExitCode {
    start(None)
}

fn start(android: Option<Android>) -> ExitCode {
    let command = env::args_os().nth(1);
    if command.as_deref() == Some("--version".as_ref()) {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    // `rusttalk --toggle-mute` and friends: hand the shortcut to the running app.
    #[cfg(target_os = "linux")]
    if let Some(message) = (command.as_ref().and_then(|c| c.to_str()))
        .and_then(|c| c.strip_prefix("--"))
        .filter(|message| hotkeys::command(message).is_some())
    {
        return match hotkeys::send(message) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error:#}");
                ExitCode::FAILURE
            }
        };
    }
    let check_update =
        cfg!(target_os = "linux") && command.as_deref() == Some("--check-update-now".as_ref());
    let window_started = Cell::new(false);
    let mut log_path = None;
    let error = match panic::catch_unwind(AssertUnwindSafe(|| {
        run(&window_started, &mut log_path, check_update, android)
    })) {
        Ok(Ok(())) => return ExitCode::SUCCESS,
        Ok(Err(error)) => format!("{error:#}"),
        Err(payload) if window_started.get() || check_update => panic::resume_unwind(payload),
        Err(payload) => {
            let reason = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or(t!("startup.unknown_error"));
            t!("startup.unexpected", reason = reason)
        }
    };

    tracing::error!(%error, "client failed");
    if window_started.get() || check_update {
        eprintln!("RustTalk client failed: {error}");
    } else {
        let message = startup_message(log_path.as_deref(), &error);
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        let shown = rfd::MessageDialog::new()
            .set_title("RustTalk")
            .set_level(rfd::MessageLevel::Error)
            .set_description(&message)
            .show()
            == rfd::MessageDialogResult::Ok;
        // shortcut: phones have no startup alert yet; the log file has the error.
        #[cfg(any(target_os = "android", target_os = "ios"))]
        let shown = false;
        if !shown {
            eprintln!("{message}");
        }
    }
    ExitCode::FAILURE
}

fn startup_message(log_path: Option<&Path>, error: &str) -> String {
    let log = match log_path {
        Some(path) => t!("startup.log_file", path = path.display()),
        None => t!("startup.no_log_file").to_owned(),
    };
    t!("startup.message", error = error, log = log)
}

fn run(
    window_started: &Cell<bool>,
    log_path: &mut Option<PathBuf>,
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))] check_update: bool,
    android: Option<Android>,
) -> Result<()> {
    let (config, service) = match env::var_os("RUSTTALK_PROFILE") {
        None => (settings_folder(android.as_ref())?, "RustTalk".to_owned()),
        Some(profile) => {
            let [letter @ b'a'..=b'z'] = profile.as_encoded_bytes() else {
                bail!("RUSTTALK_PROFILE must be one lowercase letter (a-z)");
            };
            let service = format!("RustTalk-dev-{}", char::from(*letter));
            let config = PathBuf::from(".dev/profiles").join(profile);
            (config, service)
        }
    };
    let path = std::path::absolute(config.join("rusttalk.log")).context("find log file path")?;
    *log_path = Some(path.clone());
    fs::create_dir_all(&config).context("create settings folder")?;
    #[cfg(target_os = "ios")]
    exclude_from_backup(&config)?;
    start_logging(path)?;

    #[cfg(target_os = "linux")]
    if check_update {
        return update::check_now(&config);
    }

    files::clear_opened();
    let (keystore, start) = keystore::Keystore::open(&config, &service)?;
    match &start {
        keystore::Start::Ready(Some(identity)) => tracing::info!(
            fingerprint = %fingerprint(identity.certificate.person_id()),
            "RustTalk client started"
        ),
        keystore::Start::Ready(None) => {
            tracing::info!("RustTalk client started without an account")
        }
        _ => tracing::info!("RustTalk client started, waiting for the key storage"),
    }

    // Test runs set an `agent.` app id so the compositor can route the window.
    let app_id = env::var("RUSTTALK_APP_ID").unwrap_or_else(|_| "rusttalk".to_owned());
    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_app_id(app_id)
            .with_title("RustTalk")
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/rusttalk.png"))
                    .expect("bundled app icon is a valid PNG"),
            )
            .with_inner_size([1100.0, 700.0])
            .with_min_inner_size([760.0, 480.0]),
        // Ctrl+V arrives as a key, so pictures and copied files can be pasted
        // (paste.rs); it turns the other clipboard keys back into what they were.
        clipboard_shortcuts: false,
        #[cfg(target_os = "android")]
        android_app: android,
        // wgpu's OpenGL ES backend can't configure the window surface on phones
        // without Vulkan (and the emulator); OpenGL ES through glow works on all.
        #[cfg(target_os = "android")]
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    let creator: eframe::AppCreator = Box::new(|cc| {
        Ok(Box::new(app::App::new(
            cc,
            keystore,
            start,
            config,
            window_started,
        )))
    });
    #[cfg(target_os = "android")]
    android::run(options, creator).context("run window")?;
    #[cfg(not(target_os = "android"))]
    eframe::run_native("RustTalk", options, creator)
        .map_err(|error| anyhow!("run window: {error}"))?;
    window_started.set(true);
    update::restart_if_requested()
}

/// Start the log file once per process: Android can start the app again in the same
/// process, and the logger can be set only once.
fn start_logging(path: PathBuf) -> Result<()> {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTED.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(());
    }
    let log = log_file::LogFile::new(path).context("open log file")?;
    tracing_subscriber::registry()
        .with(
            Targets::new()
                .with_default(Level::WARN)
                .with_target("rusttalk_", Level::INFO),
        )
        .with(no_typed_text())
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .log_internal_errors(true)
                .with_writer(Mutex::new(log)),
        )
        .try_init()
        .context("start logging")?;
    STARTED.store(true, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// Android: winit logs every input event it doesn't handle with its contents, and
/// that includes the keyboard's text (passwords, recovery words).
#[cfg(target_os = "android")]
fn no_typed_text() -> android::NoTypedText {
    android::NoTypedText
}

#[cfg(not(target_os = "android"))]
fn no_typed_text() -> tracing_subscriber::layer::Identity {
    tracing_subscriber::layer::Identity::new()
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn settings_folder(_: Option<&Android>) -> Result<PathBuf> {
    use etcetera::{BaseStrategy, base_strategy::choose_native_strategy};
    Ok(choose_native_strategy()
        .context("find settings folder")?
        .config_dir()
        .join("RustTalk"))
}

/// iOS: Application Support in the app's sandbox, never shown to the person.
#[cfg(target_os = "ios")]
fn settings_folder(_: Option<&Android>) -> Result<PathBuf> {
    use etcetera::{BaseStrategy, base_strategy::choose_native_strategy};
    Ok(choose_native_strategy()
        .context("find settings folder")?
        .data_dir()
        .join("RustTalk"))
}

/// iOS backs up Application Support to iCloud and computers; the key file must not go
/// there. Set before the key storage opens, so it covers the first key written.
#[cfg(target_os = "ios")]
fn exclude_from_backup(folder: &Path) -> Result<()> {
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSNumber, NSString, NSURL, NSURLIsExcludedFromBackupKey};
    let path = folder
        .to_str()
        .context("settings folder path is not UTF-8")?;
    let url = NSURL::fileURLWithPath_isDirectory(&NSString::from_str(path), true);
    let yes = NSNumber::new_bool(true);
    let value: &AnyObject = &yes;
    // SAFETY: the key is a Foundation constant, and it takes a boolean NSNumber.
    unsafe { url.setResourceValue_forKey_error(Some(value), NSURLIsExcludedFromBackupKey) }
        .map_err(|error| anyhow!("keep the settings folder out of backups: {error}"))
}

/// Android has no home folder: settings go in the app's private storage.
#[cfg(target_os = "android")]
fn settings_folder(android: Option<&Android>) -> Result<PathBuf> {
    android
        .and_then(|app| app.internal_data_path())
        .context("find settings folder")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_message_includes_the_error_and_log_location() {
        assert_eq!(
            startup_message(Some(Path::new("profile/rusttalk.log")), "Permission denied"),
            "RustTalk couldn't start.\n\nPermission denied\n\nLog file (if available): profile/rusttalk.log"
        );
        assert_eq!(
            startup_message(None, "Settings folder unavailable"),
            "RustTalk couldn't start.\n\nSettings folder unavailable\n\nThe log file location is unavailable."
        );
    }
}
