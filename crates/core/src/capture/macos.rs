//! macOS app audio. Core Audio process taps come in a later change; until then the
//! feature reports itself unsupported so the call panel disables it.

use anyhow::{Context, Result};

use super::{App, Capture, Unsupported};

pub(super) fn supported() -> Result<()> {
    Err(Unsupported).context("app audio capture is not available on macOS yet")
}

pub fn list_apps() -> Result<Vec<App>> {
    supported().map(|()| Vec::new())
}

pub fn capture(_: &App, _: impl FnMut(&[i16]) + Send + 'static) -> Result<Capture> {
    supported()?;
    unreachable!("supported() always fails on macOS")
}
