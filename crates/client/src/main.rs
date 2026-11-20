// Release builds open no console window on Windows; startup errors go to the log.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod admin;
mod app;
mod audio;
mod call;
mod camera;
mod device_sync;
mod files;
mod home;
mod keystore;
mod log_file;
mod net;
mod screen;
mod server;
mod share;

use std::{env, fs, path::PathBuf, process::ExitCode, sync::Mutex};

use anyhow::{Context, Result, anyhow, bail};
use eframe::egui::ViewportBuilder;
use etcetera::{BaseStrategy, base_strategy::choose_native_strategy};
use rusttalk_core::identity::fingerprint;
use tracing::Level;
use tracing_subscriber::{filter::Targets, prelude::*};

fn main() -> ExitCode {
    if let Err(error) = run() {
        tracing::error!(error = %format_args!("{error:#}"), "client failed");
        eprintln!("RustTalk client failed: {error:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run() -> Result<()> {
    let (config, service) = match env::var_os("RUSTTALK_PROFILE") {
        None => (
            choose_native_strategy()?.config_dir().join("RustTalk"),
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
    fs::create_dir_all(&config).context("create client config directory")?;

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
                .with_writer(Mutex::new(log_file::LogFile::new(
                    config.join("rusttalk.log"),
                )?)),
        )
        .init();

    let opened = keystore::Keystore::open(&config, &service);
    match &opened {
        Ok((_, Some(identity))) => tracing::info!(
            fingerprint = %fingerprint(identity.certificate.person_id()),
            "RustTalk client started"
        ),
        Ok((_, None)) => tracing::info!("RustTalk client started without an account"),
        Err(error) => {
            tracing::error!(error = %format_args!("{error:#}"), "opening identity failed")
        }
    }

    // Test runs set an `agent.` app id so the compositor can route the window.
    let app_id = env::var("RUSTTALK_APP_ID").unwrap_or_else(|_| "rusttalk".to_owned());
    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_app_id(app_id)
            .with_title("RustTalk")
            .with_inner_size([1100.0, 700.0])
            .with_min_inner_size([760.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "RustTalk",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc, opened, config)))),
    )
    .map_err(|error| anyhow!("run window: {error}"))
}
