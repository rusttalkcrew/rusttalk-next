use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use clap::Parser;
use livekit::{DataPacket, Room, RoomEvent, RoomOptions};
use livekit_api::access_token::{AccessToken, VideoGrants};
use rustls_pki_types::{CertificateDer, pem::PemObject};
use rusttalk_core::client::Server;
use rusttalk_core::pinned_tls::{self, Fingerprint};
use tracing::Level;
use tracing_subscriber::{filter::Targets, prelude::*};
use url::{Host, Url};

#[derive(Parser)]
#[command(about = "Join a RustTalk dev room with pinned TLS")]
struct Args {
    #[arg(long, default_value = "wss://127.0.0.1:8443")]
    server: Url,
    /// SHA-256 leaf fingerprint in hex, with optional colons. Defaults to hashing .dev/tls/cert.pem.
    #[arg(long)]
    fingerprint: Option<String>,
    #[arg(long, default_value = "dev")]
    room: String,
    #[arg(long)]
    identity: String,
    /// Location of the bot's saved server pins.
    #[arg(long, default_value = ".dev/bots")]
    data_dir: PathBuf,
    #[arg(long, env = "LIVEKIT_API_KEY", default_value = "rusttalk-dev")]
    api_key: String,
    #[arg(
        long,
        env = "LIVEKIT_API_SECRET",
        hide_env_values = true,
        default_value = "dev-only-livekit-secret-do-not-deploy",
        hide_default_value = true
    )]
    api_secret: String,
    /// Exit once the room and WebRTC data channel are up.
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
    ensure!(
        args.server.scheme() == "wss"
            && matches!(args.server.host(), Some(Host::Ipv4(_)))
            && args.server.username().is_empty()
            && args.server.password().is_none()
            && args.server.path() == "/"
            && args.server.query().is_none()
            && args.server.fragment().is_none(),
        "server must be a wss://IPv4[:port] address"
    );
    let pin = match args.fingerprint {
        Some(hex) => {
            let mut pin = Fingerprint::default();
            hex::decode_to_slice(hex.replace(':', ""), &mut pin)
                .context("expected a 32-byte hexadecimal TLS fingerprint")?;
            pin
        }
        None => {
            let path = Path::new(".dev/tls/cert.pem");
            let cert = CertificateDer::from_pem_file(path)
                .context("read .dev/tls/cert.pem or pass --fingerprint")?;
            pinned_tls::fingerprint(cert.as_ref())
        }
    };
    let api = Url::parse(&format!(
        "https://{}",
        &args.server[url::Position::BeforeHost..]
    ))?;
    Server::connect(api, Some(pin), None, &args.data_dir).await?;
    pinned_tls::register(&args.server, Some(pin)).map_err(anyhow::Error::from_boxed)?;

    // shortcut: local dev token, replaced by api-minted tokens in W08
    let token = AccessToken::with_api_key(&args.api_key, &args.api_secret)
        .with_identity(&args.identity)
        .with_ttl(Duration::from_secs(600))
        .with_grants(VideoGrants {
            room_join: true,
            room: args.room.clone(),
            can_publish: Some(false),
            can_publish_data: Some(true),
            ..Default::default()
        })
        .to_jwt()
        .context("mint dev room token")?;

    let mut options = RoomOptions::default();
    options.join_retries = 0;
    let (room, mut events) = Room::connect(args.server.as_str(), &token, options)
        .await
        .context("connect to LiveKit room")?;
    room.local_participant()
        .publish_data(DataPacket {
            reliable: true,
            ..Default::default()
        })
        .await
        .context("open the WebRTC data channel")?;
    tracing::info!(room = %args.room, identity = %args.identity, "joined");

    if !args.once {
        let disconnected = async {
            while let Some(event) = events.recv().await {
                if let RoomEvent::Disconnected { reason } = event {
                    anyhow::bail!("disconnected from LiveKit: {reason:?}");
                }
            }
            anyhow::bail!("LiveKit event stream closed");
        };
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            result = disconnected => result?,
        }
    }
    room.close().await.context("close LiveKit room")
}
