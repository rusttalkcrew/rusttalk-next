use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use clap::Parser;
use rustls_pki_types::{CertificateDer, pem::PemObject};
use rusttalk_bot::voice::{hear_tone, send_tone, source};
use rusttalk_core::{
    auth::{InviteLink, decode},
    client::{KeyClient, Server, voice::RoomEvent},
    identity::DeviceIdentity,
    pinned_tls,
};
use tracing::Level;
use tracing_subscriber::{filter::Targets, prelude::*};

#[derive(Parser)]
#[command(about = "Join an encrypted RustTalk voice channel and exchange synthetic audio")]
struct Args {
    /// Invite with an available redemption for this bot.
    #[arg(long, env = "RUSTTALK_BOT_INVITE", hide_env_values = true)]
    invite: String,
    /// Opaque voice channel ID from the server catalog.
    #[arg(long)]
    channel: String,
    #[arg(long)]
    identity: String,
    #[arg(long, value_parser = clap::value_parser!(u32).range(100..=2000))]
    tone: u32,
    #[arg(long, value_parser = clap::value_parser!(u32).range(100..=2000))]
    expect_tone: u32,
    /// Location of the bot's saved server pins.
    #[arg(long, default_value = ".dev/bots")]
    data_dir: PathBuf,
    /// Exit after receiving the peer's tone.
    #[arg(long)]
    once: bool,
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
    let source = source();
    let result = async {
        room.publish_audio(source.clone()).await?;
        tracing::info!(identity = %args.identity, "joined encrypted voice");
        let voice = async {
            tokio::time::timeout(Duration::from_secs(10), hear_tone(&mut room, None, args.expect_tone))
                .await.context("waiting for the peer's tone")??;
            tracing::info!(hz = args.expect_tone, "verified peer tone");
            if args.once {
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
            result = send_tone(source, || args.tone) => result,
            result = voice => result,
        }
    }.await;
    let closed = room.leave().await;
    result.and(closed)
}
