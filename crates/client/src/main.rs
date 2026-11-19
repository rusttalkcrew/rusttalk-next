mod keystore;
mod log_file;

use std::{env, fs, path::PathBuf, process::ExitCode, sync::Mutex};

use anyhow::{Context, Result, bail};
use etcetera::{BaseStrategy, base_strategy::choose_native_strategy};
use rusttalk_core::identity::fingerprint;
use tracing::Level;
use tracing_subscriber::{filter::Targets, prelude::*};

fn main() -> ExitCode {
    if let Err(error) = run() {
        tracing::error!(error = %format_args!("{error:#}"), "client startup failed");
        eprintln!("RustTalk client startup failed: {error:#}");
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

    let identity = keystore::load_or_create(&config, &service)?;
    tracing::info!(
        fingerprint = %fingerprint(identity.certificate.person_id()),
        "RustTalk client started"
    );
    Ok(())
}
