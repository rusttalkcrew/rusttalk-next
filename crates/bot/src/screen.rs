use anyhow::{Context, Result};
use futures_util::StreamExt;
use livekit::webrtc::{
    video_frame::{VideoFrame, VideoRotation},
    video_source::VideoResolution,
};
use rusttalk_core::{
    client::voice::{NativeVideoSource, NativeVideoStream, RemoteVideo, TrackSource, VoiceRoom},
    screen::{ScreenOptions, bgra_to_i420, frame_to_rgba},
};
use tokio::time::{MissedTickBehavior, interval};

pub const NAME: &str = "RustTalk Synthetic Screen";
const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;

pub fn options() -> ScreenOptions {
    ScreenOptions {
        fps: 15,
        ..Default::default()
    }
}

pub fn source() -> NativeVideoSource {
    NativeVideoSource::new(
        VideoResolution {
            width: WIDTH,
            height: HEIGHT,
        },
        true,
    )
}

pub async fn send(source: NativeVideoSource) -> Result<()> {
    let pixels: Vec<_> = (0..WIDTH * HEIGHT)
        .flat_map(|i| {
            if i % WIDTH < WIDTH / 2 {
                [0, 0, 255, 255]
            } else {
                [255, 0, 0, 255]
            }
        })
        .collect();
    let frame = VideoFrame::new(
        VideoRotation::VideoRotation0,
        bgra_to_i420(WIDTH, HEIGHT, &pixels, None)?,
    );
    let mut tick = interval(std::time::Duration::from_secs(1) / options().fps);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        source.capture_frame(&frame);
    }
}

pub async fn find(room: &mut VoiceRoom) -> Result<RemoteVideo> {
    loop {
        if let Some(video) = room
            .remote_video_tracks()
            .into_iter()
            .find(|video| video.source == TrackSource::Screenshare && video.name == NAME)
        {
            return Ok(video);
        }
        room.next_event()
            .await
            .context("room closed waiting for synthetic screen share")?;
    }
}

pub async fn hear(room: &mut VoiceRoom) -> Result<()> {
    let share = find(room).await?;
    room.set_subscribed(&share.sid, true)?;
    let track = loop {
        if let Some(track) = find(room).await?.track {
            break track;
        }
        room.next_event()
            .await
            .context("room closed waiting for screen subscription")?;
    };
    anyhow::ensure!(
        track.name() != NAME,
        "LiveKit received a plaintext share name"
    );
    let mut stream = NativeVideoStream::new(track.rtc_track());
    while let Some(frame) = stream.next().await {
        if (frame.buffer.width(), frame.buffer.height()) != (WIDTH, HEIGHT) {
            continue;
        }
        let pixels = frame_to_rgba(frame.buffer.as_ref())?;
        let red = ((HEIGHT / 2 * WIDTH + WIDTH / 4) * 4) as usize;
        let blue = red + (WIDTH * 2) as usize;
        if pixels[red] > 200
            && pixels[red + 1] < 50
            && pixels[red + 2] < 50
            && pixels[blue] < 50
            && pixels[blue + 1] < 50
            && pixels[blue + 2] > 200
        {
            return Ok(());
        }
    }
    anyhow::bail!("screen track ended before receiving 320x180 red and blue bars")
}
