//! Race-free waits for the e2e scenarios.
//!
//! An event can arrive, or its effect can be stored, before a test starts listening, and
//! an earlier wait may already have consumed it. So a wait checks the state first, then
//! re-checks on every event and, in case the event was missed, every `POLL`. The timeout
//! only guards against a hang; it is not a latency bound.

use std::{future::Future, sync::LazyLock, time::Duration};

use anyhow::{Context, Result};
use rusttalk_core::{client::KeyClient, presence::Update};

const POLL: Duration = Duration::from_millis(500);

/// How long any wait may take before the test counts as hung: 15 s, or
/// `RUSTTALK_E2E_WAIT_SECS`.
pub fn hang_guard() -> Duration {
    static GUARD: LazyLock<Duration> = LazyLock::new(|| {
        let secs = std::env::var("RUSTTALK_E2E_WAIT_SECS").ok();
        Duration::from_secs(secs.and_then(|secs| secs.parse().ok()).unwrap_or(15))
    });
    *GUARD
}

/// Run `future` within the hang guard.
pub async fn within<T>(what: &str, future: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(hang_guard(), future)
        .await
        .with_context(|| format!("waiting for {what}"))?
}

/// Wait until `check` holds or an update matching `done` arrives. Key state is synced
/// first and on every poll, so a missed key notice can't stall the wait. Pass
/// `async |_| Ok(false)` to wait for the notice itself.
pub async fn until<'a>(
    client: &mut KeyClient<'a>,
    what: &str,
    mut check: impl AsyncFnMut(&mut KeyClient<'a>) -> Result<bool>,
    mut done: impl FnMut(&Update) -> bool,
) -> Result<()> {
    within(what, async {
        client.sync().await?;
        loop {
            if check(client).await? {
                return Ok(());
            }
            match tokio::time::timeout(POLL, client.next_update()).await {
                Ok(update) => {
                    let update = update?.context("event socket closed")?;
                    if done(&update) {
                        return Ok(());
                    }
                }
                Err(_) => client.sync().await?,
            }
        }
    })
    .await
}

/// Wait for an update matching `done`, for checks about the notice itself.
pub async fn notice<'a>(
    client: &mut KeyClient<'a>,
    what: &str,
    done: impl FnMut(&Update) -> bool,
) -> Result<()> {
    until(client, what, async |_| Ok(false), done).await
}
