use std::{
    f32::consts::TAU,
    io::Write,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use rusttalk_core::capture::{App, SAMPLE_RATE, list_apps};

use super::TONES;

pub(super) fn available() -> bool {
    Command::new("pw-cli")
        .args(["info", "0"])
        .output()
        .is_ok_and(|output| output.status.success())
        && Command::new("pactl")
            .args(["info"])
            .env("LC_ALL", "C")
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .any(|line| line.starts_with("Server Name:") && line.contains("PipeWire"))
            })
}

pub(super) struct Playback {
    module: String,
    children: Vec<Child>,
}

impl Drop for Playback {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        if !Command::new("pactl")
            .args(["unload-module", &self.module])
            .status()
            .is_ok_and(|status| status.success())
        {
            tracing::warn!("could not unload the test null sink");
        }
    }
}

pub(super) fn play() -> Result<(Playback, Vec<App>)> {
    ensure!(
        available(),
        "PipeWire with pipewire-pulse is required for the test null sink"
    );
    let sink = format!("rusttalk_e2e_{}", std::process::id());
    let module = Command::new("pactl")
        .args([
            "load-module",
            "module-null-sink",
            &format!("sink_name={sink}"),
        ])
        .output()?;
    ensure!(
        module.status.success(),
        "pactl could not load a test null sink"
    );
    let mut playback = Playback {
        module: String::from_utf8_lossy(&module.stdout).trim().to_owned(),
        children: Vec::new(),
    };
    let files = tempfile::tempdir()?;
    for (i, (name, hz)) in TONES.into_iter().enumerate() {
        let path = files.path().join(format!("tone-{i}.wav"));
        std::fs::write(&path, tone_wav(hz)?)?;
        let props = format!(
            "{{ application.name = \"{name}\" node.dont-fallback = true node.dont-move = true }}"
        );
        playback.children.push(
            Command::new("pw-play")
                .args(["--target", &sink, "--properties", &props])
                .arg(&path)
                .stdin(Stdio::null())
                .spawn()
                .context("start local PipeWire tone player")?,
        );
    }

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let apps = list_apps()?;
        let selected: Vec<_> = TONES
            .iter()
            .filter_map(|(name, _)| apps.iter().find(|app| app.name == *name).cloned())
            .collect();
        if selected.len() == TONES.len() {
            return Ok((playback, selected));
        }
        ensure!(
            Instant::now() < deadline,
            "two local tone players did not start"
        );
        thread::sleep(Duration::from_millis(30));
    }
}

fn tone_wav(hz: u32) -> Result<Vec<u8>> {
    const SECONDS: u32 = 30;
    let bytes = 2 * SAMPLE_RATE * SECONDS;
    let mut wav = Vec::with_capacity(44 + bytes as usize);
    wav.write_all(b"RIFF")?;
    wav.write_all(&(36 + bytes).to_le_bytes())?;
    wav.write_all(b"WAVEfmt ")?;
    wav.write_all(&[16, 0, 0, 0, 1, 0, 1, 0])?; // 16-byte PCM format, one channel.
    wav.write_all(&SAMPLE_RATE.to_le_bytes())?;
    wav.write_all(&(SAMPLE_RATE * 2).to_le_bytes())?;
    wav.write_all(&[2, 0, 16, 0])?;
    wav.write_all(b"data")?;
    wav.write_all(&bytes.to_le_bytes())?;
    let second: Vec<_> = (0..SAMPLE_RATE)
        .flat_map(|i| {
            ((12_000.0 * (TAU * hz as f32 * i as f32 / SAMPLE_RATE as f32).sin()) as i16)
                .to_le_bytes()
        })
        .collect();
    for _ in 0..SECONDS {
        wav.extend_from_slice(&second);
    }
    Ok(wav)
}
