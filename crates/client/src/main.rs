#![cfg_attr(windows, windows_subsystem = "windows")]

mod log_file;

use std::{fs, sync::Mutex};

use etcetera::{BaseStrategy, base_strategy::choose_native_strategy};
use tracing::Level;
use tracing_subscriber::{filter::Targets, prelude::*};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = choose_native_strategy()?.config_dir().join("RustTalk");
    fs::create_dir_all(&config)?;

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

    tracing::info!("RustTalk client started");
    Ok(())
}
