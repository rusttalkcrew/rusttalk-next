use std::process::Stdio;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::watch,
    task::JoinHandle,
};

use super::{BgraFrame, Cancelled, ScreenOptions, ScreenSource, ScreenSourceKind, frame_bytes};
use crate::pipewire_helper::pipewire_helper;

pub fn list_sources() -> Result<Vec<ScreenSource>> {
    Ok(vec![ScreenSource {
        name: "Pick via desktop portal".into(),
        kind: ScreenSourceKind::Portal,
    }])
}

#[derive(Deserialize)]
struct Selection {
    #[serde(default)]
    name: String,
    restore_token: Option<String>,
    /// The person closed the picker.
    #[serde(default)]
    cancelled: bool,
}

pub struct Backend(JoinHandle<()>);

impl Backend {
    pub fn is_finished(&self) -> bool {
        self.0.is_finished()
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub async fn open(
    _: ScreenSource,
    options: ScreenOptions,
    restore_token: &mut Option<String>,
) -> Result<(String, watch::Receiver<Option<BgraFrame>>, Backend)> {
    let mut child = Command::new(pipewire_helper()?)
        .args([
            "screen",
            &options.fps.to_string(),
            if options.cursor { "cursor" } else { "hidden" },
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("start PipeWire screen capture helper")?;
    let mut stdin = child.stdin.take().expect("piped restore token");
    let token = serde_json::to_vec(&restore_token.take())?;
    stdin.write_all(&token).await?;
    drop(stdin);

    let mut stdout = BufReader::new(child.stdout.take().expect("piped screen capture"));
    let mut selection = String::new();
    (&mut stdout)
        .take(64 * 1024)
        .read_line(&mut selection)
        .await?;
    ensure!(
        selection.ends_with('\n'),
        "desktop portal did not return a screen selection"
    );
    let selection: Selection =
        serde_json::from_str(&selection).context("read desktop portal selection")?;
    if selection.cancelled {
        return Err(Cancelled.into());
    }
    *restore_token = selection.restore_token;

    let (output, frames) = watch::channel(None);
    let task = tokio::spawn(async move {
        let result = async {
            loop {
                if output.send(Some(read_frame(&mut stdout).await?)).is_err() {
                    break;
                }
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(%error, "PipeWire screen capture ended");
        }
        // The child is killed and reaped by Tokio when the reader stops or is cancelled.
        drop(child);
    });
    Ok((selection.name, frames, Backend(task)))
}

/// One helper frame: little-endian width and height, then tightly packed BGRx rows.
async fn read_frame(input: &mut (impl AsyncRead + Unpin)) -> Result<BgraFrame> {
    let width = input.read_u32_le().await?;
    let height = input.read_u32_le().await?;
    let mut pixels = vec![0; frame_bytes(width, height)?];
    input.read_exact(&mut pixels).await?;
    Ok(BgraFrame {
        width,
        height,
        pixels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(bytes: &[u8]) -> Result<BgraFrame> {
        tokio::runtime::Builder::new_current_thread()
            .build()?
            .block_on(read_frame(&mut &bytes[..]))
    }

    fn header(width: u32, height: u32) -> Vec<u8> {
        [width.to_le_bytes(), height.to_le_bytes()].concat()
    }

    #[test]
    fn helper_frame_bounds() {
        let mut odd = header(3, 1);
        odd.extend([7; 12]);
        let frame = read(&odd).unwrap();
        assert_eq!(
            (frame.width, frame.height, frame.pixels),
            (3, 1, vec![7; 12])
        );
        // Oversized and empty headers fail before allocating pixels.
        for (width, height) in [
            (0, 1),
            (1, 0),
            (8193, 1),
            (8192, 8192),
            (u32::MAX, u32::MAX),
        ] {
            assert!(read(&header(width, height)).is_err());
        }
        assert!(read(&odd[..odd.len() - 1]).is_err());
        assert!(read(&odd[..6]).is_err());
        assert!(read(&[]).is_err());
    }
}
