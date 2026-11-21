use std::{
    io::{BufReader, Read},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering::Relaxed},
        mpsc,
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, ensure};

use crate::pipewire_helper::pipewire_helper;

use super::{App, Capture, FRAME_SAMPLES, deliver};

pub fn list_apps() -> Result<Vec<App>> {
    let output = Command::new(pipewire_helper()?)
        .arg("list")
        .stderr(Stdio::piped())
        .output()
        .context("start PipeWire app audio helper")?;
    ensure!(
        output.status.success(),
        "could not list PipeWire playback streams: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    serde_json::from_slice(&output.stdout).context("read PipeWire app list")
}

pub fn capture(app: &App, mut sink: impl FnMut(&[i16]) + Send + 'static) -> Result<Capture> {
    let arg = if app.id == super::SYSTEM {
        super::SYSTEM.to_owned()
    } else {
        let (cookie, serial) = app
            .id
            .split_once(':')
            .context("invalid PipeWire stream ID")?;
        let cookie: u32 = cookie.parse().context("invalid PipeWire daemon cookie")?;
        let serial: u64 = serial.parse().context("invalid PipeWire object serial")?;
        format!("{cookie}:{serial}")
    };
    let mut child = Command::new(pipewire_helper()?)
        .arg(arg)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start PipeWire app audio helper")?;
    let mut pcm = BufReader::new(child.stdout.take().expect("piped PCM"));
    let mut stderr = child.stderr.take().expect("piped stderr");
    let child = Arc::new(Mutex::new(child));
    let child_for_worker = child.clone();
    let stopped = Arc::new(AtomicBool::new(false));
    let stopped_worker = stopped.clone();
    let (ready, startup) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let mut bytes = [0; FRAME_SAMPLES * 2];
        let first = pcm.read_exact(&mut bytes);
        let started = first.is_ok();
        if ready.send(first).is_ok() && started {
            while !stopped_worker.load(Relaxed) {
                deliver(&bytes, &mut sink);
                if pcm.read_exact(&mut bytes).is_err() {
                    break;
                }
            }
        }
        let mut child = child_for_worker.lock().expect("capture child");
        // EOF usually means the child exited. Reap it even if it closed stdout first.
        let _ = child.kill();
        if let Err(error) = child.wait() {
            tracing::warn!(%error, "could not reap PipeWire capture helper");
        }
        drop(child);
        let mut reason = String::new();
        let _ = stderr.read_to_string(&mut reason);
        if !reason.trim().is_empty() {
            tracing::warn!(reason = reason.trim(), "PipeWire capture helper stopped");
        }
    });
    let capture = Capture {
        stop: Some(Box::new(move || {
            stopped.store(true, Relaxed);
            let _ = child.lock().expect("capture child").kill();
        })),
        worker,
    };
    startup
        .recv_timeout(Duration::from_secs(7))
        .context("timed out starting PipeWire app capture")?
        .context("PipeWire app capture ended before supplying audio")?;
    Ok(capture)
}
