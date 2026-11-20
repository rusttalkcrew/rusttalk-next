use std::path::PathBuf;

use anyhow::{Context, Result, ensure};

pub(crate) fn pipewire_helper() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe.parent().context("locate application executable")?;
    // Cargo integration tests live one level below the desktop and bot executables.
    let dir = if dir.ends_with("deps") {
        dir.parent().context("locate Cargo target directory")?
    } else {
        dir
    };
    let helper = dir.join("rusttalk-pipewire");
    ensure!(helper.is_file(), "PipeWire capture helper is missing");
    Ok(helper)
}
