//! Headless video measurement against a disposable dev stack; not part of `make e2e`.
//! One bot publishes a moving 1080p60 screen (or a 720p30 camera with
//! `BENCH_KIND=camera`) with its send time drawn into every frame; a second bot reports
//! each second what it receives: frames, size, end-to-end latency, the sender's
//! encoder stats and its own decoder stats. Both bots share the clock, so the
//! latency covers capture to decoded frame. Run it with a stack up, for example:
//!
//! ```sh
//! export RUSTTALK_BENCH_PROJECT=rusttalk-bench RUSTTALK_DEV_DATA=$(mktemp -d)
//! docker compose -f deploy/compose.dev.yml -p $RUSTTALK_BENCH_PROJECT up --build --wait
//! cargo test -p rusttalk-bot --test video_bench -- --nocapture
//! ```
//!
//! The stack's owner invite is single-use: start a fresh stack for every run.
#![cfg(feature = "media")]

// Shared with the e2e suite, of which this bench only needs `until`.
#[allow(dead_code)]
#[path = "wait/mod.rs"]
mod wait;

use std::{
    env,
    path::Path,
    process::Command,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use livekit::webrtc::{
    prelude::I420Buffer,
    stats::RtcStats,
    video_frame::{VideoBuffer, VideoFrame, VideoRotation},
    video_source::VideoResolution,
};
use rustls_pki_types::{CertificateDer, pem::PemObject};
use rusttalk_core::{
    auth::{InviteLink, KnownServer, decode},
    camera::CameraOptions,
    client::{
        KeyClient, Server,
        voice::{NativeVideoSource, NativeVideoStream, TrackSource},
    },
    identity::DeviceIdentity,
    keys::Scope,
    pinned_tls::fingerprint,
    screen::{ScreenOptions, frame_to_rgba},
};

/// Milliseconds are drawn as this many black or white blocks across the top.
const BITS: u32 = 24;
const MASK: u64 = (1 << BITS) - 1;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        & MASK
}

fn cli(project: &str, command: &str) -> Result<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new("docker")
        .current_dir(root)
        .args(["compose", "-f", "deploy/compose.dev.yml", "-p", project])
        .args(["exec", "-T", "api", "rusttalk-api", command])
        .output()?;
    ensure!(output.status.success(), "compose exec {command} failed");
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

async fn receive_key(client: &mut KeyClient<'_>, scope: Scope) -> Result<()> {
    wait::until(
        client,
        "envelope delivery",
        async |client| Ok(client.key(scope, 1).is_some()),
        |_| false,
    )
    .await
}

/// A tall page of text-like cells, scrolled a little every frame.
fn page(width: usize, height: usize) -> Vec<u8> {
    let mut rows = vec![0u8; width * height];
    for (y, row) in rows.chunks_mut(width).enumerate() {
        for (x, pixel) in row.iter_mut().enumerate() {
            let cell = (x / 9).wrapping_mul(31) ^ (y / 16).wrapping_mul(17);
            let ink = (y / 16) % 5 != 4 && ((cell * 2_654_435_761) >> 7) % 4 != 0;
            let glyph = ink && x % 9 < 6 && y % 16 < 11 && (x + y + cell) % 3 != 0;
            *pixel = if glyph {
                40
            } else {
                200 + (x * 40 / width) as u8
            };
        }
    }
    rows
}

fn draw(frame: &mut I420Buffer, page: &[u8], offset: usize, stamp: u64) {
    let (width, height) = (frame.width() as usize, frame.height() as usize);
    let (ys, _, _) = frame.strides();
    let ys = ys as usize;
    let (y, u, v) = frame.data_mut();
    for row in 0..height {
        let from = (row + offset) % (page.len() / width) * width;
        y[row * ys..row * ys + width].copy_from_slice(&page[from..from + width]);
    }
    // A box crossing the picture, like a dragged window.
    let side = height / 4;
    let left = offset * 3 % (width - side);
    for row in height / 2..height / 2 + side {
        y[row * ys + left..row * ys + left + side].fill(90);
    }
    let block = width / BITS as usize;
    for row in 0..block {
        for bit in 0..BITS as usize {
            let start = row * ys + bit * block;
            let on = (stamp >> bit) & 1 == 1;
            y[start..start + block].fill(if on { 235 } else { 16 });
        }
    }
    u.fill(128);
    v.fill(128);
}

fn read_stamp(frame: &I420Buffer) -> u64 {
    let (ys, _, _) = frame.strides();
    let (y, _, _) = frame.data();
    let block = frame.width() as usize / BITS as usize;
    (0..BITS as usize).fold(0, |stamp, bit| {
        let value = y[block / 2 * ys as usize + bit * block + block / 2];
        stamp | (u64::from(value > 128) << bit)
    })
}

fn percentile(sorted: &[u64], q: f64) -> u64 {
    sorted
        .get(((sorted.len() as f64 - 1.0) * q) as usize)
        .copied()
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn video_bench() -> Result<()> {
    let Ok(project) = env::var("RUSTTALK_BENCH_PROJECT") else {
        return Ok(());
    };
    let camera = env::var("BENCH_KIND").as_deref() == Ok("camera");
    let (width, height, fps) = if camera {
        (1280, 720, 30)
    } else {
        (1920, 1080, 60)
    };
    let seconds = env::var("BENCH_SECONDS").map_or(Ok(15), |s| s.parse())?;
    let data = env::var("RUSTTALK_DEV_DATA")?;
    let data = Path::new(&data);
    let cert = CertificateDer::from_pem_file(data.join("tls/cert.pem"))?;
    let port = env::var("RUSTTALK_DEV_HTTPS_PORT").unwrap_or_else(|_| "8443".into());
    let known = KnownServer {
        address: format!("https://127.0.0.1:{port}/").parse()?,
        server_id: decode(&cli(&project, "server-id")?)?,
        tls_fingerprint: Some(fingerprint(cert.as_ref())),
    };
    let server = Server::connect(
        known.address.clone(),
        known.tls_fingerprint,
        Some(&known),
        &data.join("bench"),
    )
    .await?;
    let (_, a_id) = DeviceIdentity::create("Bench A")?;
    let code = decode(&cli(&project, "owner-invite")?)?;
    let a_session = server
        .sign_in(&a_id, Some(&code))
        .await?
        .error_for_status()?
        .json()
        .await?;
    let mut a = KeyClient::connect(&server, &a_id, &a_session).await?;
    let category = a.create_category(b"Bench").await?;
    let voice = a.create_channel(&category, "voice", b"Bench", b"").await?;
    let (_, link) = a.invite(1, 300).await?;
    let link = InviteLink::parse(&link.to_string())?;
    let (_, b_id) = DeviceIdentity::create("Bench B")?;
    let b_session = server
        .sign_in(&b_id, Some(&link.code))
        .await?
        .error_for_status()?
        .json()
        .await?;
    let mut b = KeyClient::connect(&server, &b_id, &b_session).await?;
    a.sync().await?;
    receive_key(&mut b, voice).await?;
    let (room_a, mut room_b) = tokio::try_join!(a.join_voice(&voice.id), b.join_voice(&voice.id))?;

    let source = NativeVideoSource::new(VideoResolution { width, height }, !camera);
    let (kind, publication) = if camera {
        let options = CameraOptions { height, fps };
        let publication = room_a.publish_camera(options, source.clone()).await?;
        (TrackSource::Camera, publication)
    } else {
        let options = ScreenOptions {
            fps,
            ..Default::default()
        };
        let publication = room_a
            .publish_screen("Bench", options, source.clone())
            .await?;
        (TrackSource::Screenshare, publication)
    };
    let sent = publication.track().context("missing local track")?;
    let send = async {
        let page = page(width as usize, height as usize * 2);
        let mut frame = I420Buffer::new(width, height);
        let mut tick = tokio::time::interval(Duration::from_secs(1) / fps);
        for offset in (0..).step_by(4) {
            tick.tick().await;
            draw(&mut frame, &page, offset, now_ms());
            let video = VideoFrame::new(VideoRotation::VideoRotation0, frame);
            source.capture_frame(&video);
            frame = video.buffer;
        }
    };
    let receive = async {
        let sid = loop {
            let videos = room_b.remote_video_tracks();
            if let Some(video) = videos.into_iter().find(|video| video.source == kind) {
                break video.sid;
            }
            room_b.next_event().await.context("room closed")?;
        };
        room_b.set_subscribed(&sid, true)?;
        let track = loop {
            let videos = room_b.remote_video_tracks();
            if let Some(track) = videos
                .into_iter()
                .find(|v| v.sid == sid)
                .and_then(|v| v.track)
            {
                break track;
            }
            room_b.next_event().await.context("room closed")?;
        };
        // Like the client: only the newest decoded frame waits.
        let mut stream = NativeVideoStream::new(track.rtc_track());
        let start = Instant::now();
        let mut second = Instant::now();
        let (mut frames, mut last, mut convert) = (0u32, 0, Duration::ZERO);
        let (mut latencies, mut steady_latencies, mut steady_fps) = (Vec::new(), vec![], vec![]);
        while start.elapsed() < Duration::from_secs(seconds) {
            let Ok(Some(frame)) = tokio::time::timeout(Duration::from_secs(2), stream.next()).await
            else {
                println!("no frame for 2 s");
                continue;
            };
            let at = now_ms();
            let i420 = frame.buffer.to_i420();
            let stamp = read_stamp(&i420);
            // A repeated stamp is a repeated picture: not counted as a frame.
            if stamp != last {
                frames += 1;
                last = stamp;
                latencies.push(at.wrapping_sub(stamp) & MASK);
            }
            let converting = Instant::now();
            frame_to_rgba(frame.buffer.as_ref())?;
            convert += converting.elapsed();
            if second.elapsed() < Duration::from_secs(1) {
                continue;
            }
            let rate = f64::from(frames) / second.elapsed().as_secs_f64();
            latencies.sort_unstable();
            println!(
                "recv {rate:>5.1} fps {}x{} latency p50 {} ms p95 {} ms, RGBA {:.1} ms/frame",
                i420.width(),
                i420.height(),
                percentile(&latencies, 0.5),
                percentile(&latencies, 0.95),
                convert.as_secs_f64() * 1000.0 / f64::from(frames.max(1)),
            );
            // The first seconds measure the bandwidth estimate ramping up.
            if start.elapsed() > Duration::from_secs(3) {
                steady_latencies.append(&mut latencies);
                steady_fps.push(rate);
            }
            for stat in sent.get_stats().await? {
                if let RtcStats::OutboundRtp(stat) = stat {
                    let o = &stat.outbound;
                    println!(
                        "  sent {:>1} {}x{} {:>4.1} fps, target {:>5.0} kbps, limited by {:?}, encode {:.1} ms, {} key frames, {} huge",
                        o.rid,
                        o.frame_width,
                        o.frame_height,
                        o.frames_per_second,
                        o.target_bitrate / 1000.0,
                        o.quality_limitation_reason,
                        o.total_encode_time * 1000.0 / f64::from(o.frames_encoded.max(1)),
                        o.key_frames_encoded,
                        o.huge_frames_sent,
                    );
                }
            }
            for stat in track.get_stats().await? {
                if let RtcStats::InboundRtp(stat) = stat {
                    let i = &stat.inbound;
                    println!(
                        "  decoded {}x{} {:>4.1} fps, decode {:.1} ms, jitter buffer {:.0} ms, {} dropped, {} freezes, {}",
                        i.frame_width,
                        i.frame_height,
                        i.frames_per_second,
                        i.total_decode_time * 1000.0 / f64::from(i.frames_decoded.max(1)),
                        i.jitter_buffer_delay * 1000.0
                            / (i.jitter_buffer_emitted_count.max(1) as f64),
                        i.frames_dropped,
                        i.freeze_count,
                        i.decoder_implementation,
                    );
                }
            }
            (frames, convert, second) = (0, Duration::ZERO, Instant::now());
            latencies.clear();
        }
        steady_latencies.sort_unstable();
        println!(
            "SUMMARY {}x{}@{fps}: fps mean {:.1} min {:.1}, latency p50 {} ms p95 {} ms",
            width,
            height,
            steady_fps.iter().sum::<f64>() / steady_fps.len().max(1) as f64,
            steady_fps.iter().copied().fold(f64::INFINITY, f64::min),
            percentile(&steady_latencies, 0.5),
            percentile(&steady_latencies, 0.95),
        );
        Ok::<_, anyhow::Error>(())
    };
    tokio::select! {
        result = receive => result?,
        () = send => (),
    }
    room_a.leave().await?;
    room_b.leave().await?;
    Ok(())
}
