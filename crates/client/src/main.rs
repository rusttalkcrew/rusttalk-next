// Release builds open no console window on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod admin;
mod app;
mod audio;
mod call;
mod camera;
mod crop;
mod denoise;
mod device_sync;
mod dm;
mod emoji;
mod files;
mod home;
mod keystore;
mod log_file;
mod net;
mod notify;
mod official;
mod reorder;
mod screen;
mod server;
mod settings;
mod share;
mod ui;
mod update;

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
use etcetera::{BaseStrategy, base_strategy::choose_native_strategy};
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

fn main() -> ExitCode {
    let command = env::args_os().nth(1);
    if command.as_deref() == Some("--version".as_ref()) {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let check_update =
        cfg!(target_os = "linux") && command.as_deref() == Some("--check-update-now".as_ref());
    let window_started = Cell::new(false);
    let mut log_path = None;
    let error = match panic::catch_unwind(AssertUnwindSafe(|| {
        run(&window_started, &mut log_path, check_update)
    })) {
        Ok(Ok(())) => return ExitCode::SUCCESS,
        Ok(Err(error)) => format!("{error:#}"),
        Err(payload) if window_started.get() || check_update => panic::resume_unwind(payload),
        Err(payload) => {
            let reason = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("Unknown error");
            format!("Unexpected error: {reason}")
        }
    };

    tracing::error!(%error, "client failed");
    if window_started.get() || check_update {
        eprintln!("RustTalk client failed: {error}");
    } else {
        let message = startup_message(log_path.as_deref(), &error);
        if rfd::MessageDialog::new()
            .set_title("RustTalk")
            .set_level(rfd::MessageLevel::Error)
            .set_description(&message)
            .show()
            != rfd::MessageDialogResult::Ok
        {
            eprintln!("{message}");
        }
    }
    ExitCode::FAILURE
}

fn startup_message(log_path: Option<&Path>, error: &str) -> String {
    let log = match log_path {
        Some(path) => format!("Log file (if available): {}", path.display()),
        None => "The log file location is unavailable.".to_owned(),
    };
    format!("RustTalk couldn't start.\n\n{error}\n\n{log}")
}

fn run(
    window_started: &Cell<bool>,
    log_path: &mut Option<PathBuf>,
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))] check_update: bool,
) -> Result<()> {
    let (config, service) = match env::var_os("RUSTTALK_PROFILE") {
        None => (
            choose_native_strategy()
                .context("find settings folder")?
                .config_dir()
                .join("RustTalk"),
            "RustTalk".to_owned(),
        ),
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
    let log = log_file::LogFile::new(path).context("open log file")?;

    tracing_subscriber::registry()
        .with(
            Targets::new()
                .with_default(Level::WARN)
                .with_target("rusttalk_", Level::INFO),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .log_internal_errors(true)
                .with_writer(Mutex::new(log)),
        )
        .try_init()
        .context("start logging")?;

    #[cfg(target_os = "linux")]
    if check_update {
        return update::check_now(&config);
    }

    files::clear_opened();
    let (keystore, identity) = keystore::Keystore::open(&config, &service)?;
    match &identity {
        Some(identity) => tracing::info!(
            fingerprint = %fingerprint(identity.certificate.person_id()),
            "RustTalk client started"
        ),
        None => tracing::info!("RustTalk client started without an account"),
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
        ..Default::default()
    };
    eframe::run_native(
        "RustTalk",
        options,
        Box::new(|cc| {
            Ok(Box::new(app::App::new(
                cc,
                keystore,
                identity,
                config,
                window_started,
            )))
        }),
    )
    .map_err(|error| anyhow!("run window: {error}"))?;
    window_started.set(true);
    update::restart_if_requested()
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
