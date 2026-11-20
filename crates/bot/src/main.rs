use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use clap::{Parser, ValueEnum};
use rustls_pki_types::{CertificateDer, pem::PemObject};
use rusttalk_bot::{
    app_audio, screen,
    voice::{hear_tone, send_tone, source},
};
use rusttalk_core::{
    auth::{InviteLink, decode},
    client::{KeyClient, Server, voice::RoomEvent},
    identity::DeviceIdentity,
    pinned_tls, screen as capture,
};
use tracing::Level;
use tracing_subscriber::{filter::Targets, prelude::*};

#[derive(Parser)]
#[command(about = "Join an encrypted RustTalk voice channel and exchange media")]
struct Args {
    /// Invite with an available redemption for this bot.
    #[arg(long, env = "RUSTTALK_BOT_INVITE", hide_env_values = true)]
    invite: String,
    /// Opaque voice channel ID from the server catalog.
    #[arg(long)]
    channel: String,
    #[arg(long)]
    identity: String,
    #[arg(long, value_parser = clap::value_parser!(u32).range(100..=2000), required_unless_present_any = ["share_app_audio", "share_screen", "expect_screen"], conflicts_with = "share_app_audio")]
    tone: Option<u32>,
    #[arg(long, value_parser = clap::value_parser!(u32).range(100..=2000), required_unless_present_any = ["expect_app_audio", "share_screen", "expect_screen"], conflicts_with = "expect_app_audio")]
    expect_tone: Option<u32>,
    /// Publish two PipeWire tone players on separate app audio tracks (Linux).
    #[arg(long)]
    share_app_audio: bool,
    /// Verify two separate received app tone tracks with independent gain.
    #[arg(long)]
    expect_app_audio: bool,
    /// Publish a synthetic pattern or a Linux desktop portal capture.
    #[arg(long)]
    share_screen: Option<ScreenInput>,
    /// Subscribe to a synthetic screen share and verify its pattern.
    #[arg(long, conflicts_with_all = ["expect_tone", "expect_app_audio"])]
    expect_screen: bool,
    /// Location of the bot's saved server pins.
    #[arg(long, default_value = ".dev/bots")]
    data_dir: PathBuf,
    /// Exit after verifying the expected media.
    #[arg(long)]
    once: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum ScreenInput {
    Synthetic,
    Portal,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::registry()
        .with(
            Targets::new()
                .with_default(Level::WARN)
                .with_target("rusttalk_", Level::INFO),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();
    if let Err(error) = run(Args::parse()).await {
        tracing::error!(error = %format_args!("{error:#}"), "bot failed");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn run(args: Args) -> Result<()> {
    let mut link = InviteLink::parse(&args.invite)?;
    if link.server.tls_fingerprint.is_none() {
        let cert = CertificateDer::from_pem_file(Path::new(".dev/tls/cert.pem"))
            .context("read .dev/tls/cert.pem or include the TLS pin in the invite")?;
        link.server.tls_fingerprint = Some(pinned_tls::fingerprint(cert.as_ref()));
    }
    let channel = decode(&args.channel)?;
    let server = Server::connect(
        link.server.address.clone(),
        link.server.tls_fingerprint,
        Some(&link.server),
        &args.data_dir,
    )
    .await?;
    let (_, device) = DeviceIdentity::create(&args.identity)?;
    let session = server
        .sign_in(&device, Some(&link.code))
        .await?
        .error_for_status()?
        .json()
        .await?;
    let mut keys = KeyClient::connect(&server, &device, &session).await?;
    let scope = rusttalk_core::keys::Scope {
        kind: rusttalk_core::keys::Kind::Channel,
        id: channel,
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        while keys.current_key(scope).is_err() {
            keys.next_event()
                .await?
                .context("event stream closed waiting for channel key")?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("waiting for channel key")??;

    let mut room = keys.join_voice(&channel).await?;
    let result = async {
        let mut _apps = None;
        let mut microphone = None;
        if args.share_app_audio {
            _apps = Some(app_audio::share(&room).await?);
        } else if args.tone.is_some() {
            let source = source();
            room.publish_audio(source.clone()).await?;
            microphone = Some(source);
        }
        let mut portal_capture = None;
        let mut synthetic = None;
        match args.share_screen {
            Some(ScreenInput::Synthetic) => {
                let source = screen::source();
                room.publish_screen(screen::NAME, screen::options(), source.clone()).await?;
                synthetic = Some(source);
            }
            Some(ScreenInput::Portal) => {
                let target = capture::list_sources()?.into_iter()
                    .find(|target| target.kind == capture::ScreenSourceKind::Portal)
                    .context("desktop portal capture requires Linux")?;
                let options = capture::ScreenOptions::default();
                let capture = capture::start(target, options, &mut None).await?;
                room.publish_screen(&capture.name, options, capture.source.clone()).await?;
                portal_capture = Some(capture);
            }
            None => (),
        }
        let send_audio = async {
            if let Some((source, hz)) = microphone.zip(args.tone) {
                send_tone(source, || hz).await
            } else if args.share_app_audio {
                // The local players are finite.
                tokio::time::sleep(Duration::from_secs(25)).await;
                Ok(())
            } else {
                std::future::pending().await
            }
        };
        let send_screen = async {
            if let Some(source) = synthetic {
                screen::send(source).await
            } else if let Some(capture) = portal_capture {
                loop {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    ensure!(!capture.is_finished(), "portal screen capture ended");
                }
            } else {
                std::future::pending().await
            }
        };
        tracing::info!(identity = %args.identity, "joined encrypted voice");
        let voice = async {
            tokio::time::timeout(Duration::from_secs(10), async {
                if let Some(hz) = args.expect_tone {
                    hear_tone(&mut room, None, hz).await?;
                    tracing::info!(hz, "verified peer tone");
                } else if args.expect_app_audio {
                    app_audio::hear(&mut room, None).await?;
                } else if args.expect_screen {
                    screen::hear(&mut room).await?;
                    tracing::info!("verified encrypted synthetic screen");
                }
                Ok::<_, anyhow::Error>(())
            }).await.context("waiting for the peer's audio")??;
            if args.once && (args.expect_screen || args.expect_tone.is_some() || args.expect_app_audio) {
                return Ok(());
            }
            loop {
                tokio::select! {
                    event = keys.next_event() => {
                        ensure!(event?.is_some(), "API event stream closed: {:?}", keys.close_reason);
                        if let Err(error) = room.sync_key(&keys) {
                            tracing::warn!(%error, "voice key update pending");
                        }
                    }
                    event = room.next_event() => match event.context("LiveKit event stream closed")? {
                        RoomEvent::Disconnected { reason } => anyhow::bail!("LiveKit disconnected: {reason:?}"),
                        RoomEvent::E2eeStateChanged { state, .. } => tracing::info!(?state, "media encryption state changed"),
                        _ => (),
                    }
                }
            }
        };
        tokio::select! {
            result = tokio::signal::ctrl_c() => Ok(result?),
            result = send_audio => result,
            result = send_screen => result,
            result = voice => result,
        }
    }.await;
    let closed = room.leave().await;
    result.and(closed)
}
