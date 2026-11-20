use std::{
    env,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use rustls_pki_types::{CertificateDer, pem::PemObject};
use rusttalk_bot::voice::{hear_tone_with_events, send_tone, source};
use rusttalk_core::{
    auth::{KnownServer, decode},
    client::{
        KeyClient, MessageBody, Server,
        voice::{ParticipantIdentity, RoomEvent, VoiceRoom},
    },
    identity::DeviceIdentity,
    keys::Scope,
    pinned_tls::fingerprint,
};
use tokio::time::{Instant, interval_at, sleep_until, timeout};

use super::{chat::wait_event, compose, receive_key};

const LIMIT: Duration = Duration::from_secs(5);
const MINUTE: Duration = Duration::from_secs(60);
const TONE_INTERVAL: Duration = Duration::from_secs(30);

#[tokio::test]
#[ignore = "requires the disposable make soak stack"]
async fn docker_soak() -> Result<()> {
    let project = env::var("RUSTTALK_SOAK_PROJECT")?;
    let data = env::var("RUSTTALK_DEV_DATA")?;
    let data = Path::new(&data);
    let minutes: u32 = env::var("SOAK_MINUTES")?.parse()?;
    ensure!(minutes > 0, "SOAK_MINUTES must be positive");
    let ttl: u64 = env::var("RUSTTALK_DEV_SESSION_TTL_SECS")?.parse()?;
    let cli = |command| compose(&project, &["exec", "-T", "api", "rusttalk-api", command]);
    let cert = CertificateDer::from_pem_file(data.join("tls/cert.pem"))?;
    let port = env::var("RUSTTALK_DEV_HTTPS_PORT").unwrap_or_else(|_| "8443".into());
    let known = KnownServer {
        address: format!("https://127.0.0.1:{port}/").parse()?,
        server_id: decode(&cli("server-id")?)?,
        tls_fingerprint: Some(fingerprint(cert.as_ref())),
    };
    let server = Server::connect(
        known.address.clone(),
        known.tls_fingerprint,
        Some(&known),
        &data.join("bots"),
    )
    .await?;
    let (_, owner) = DeviceIdentity::create("Soak owner")?;
    let (_, member) = DeviceIdentity::create("Soak member")?;
    let code = decode(&cli("owner-invite")?)?;
    let owner_session = server
        .sign_in(&owner, Some(&code))
        .await?
        .error_for_status()?
        .json()
        .await?;
    let mut a = KeyClient::connect(&server, &owner, &owner_session).await?;
    let category = a.create_category(b"Soak").await?;
    let text = a.create_channel(&category, "text", b"Chat", b"").await?;
    let voice = a.create_channel(&category, "voice", b"Tones", b"").await?;
    let (_, invite) = a.invite(1, 300).await?;
    let member_session = server
        .sign_in(&member, Some(&invite.code))
        .await?
        .error_for_status()?
        .json()
        .await?;
    let mut b = KeyClient::connect(&server, &member, &member_session).await?;
    tokio::select! {
        result = async {
            receive_key(&mut b, text, 1).await?;
            receive_key(&mut b, voice, 1).await
        } => result?,
        result = async {
            loop {
                match a.next_event().await {
                    Ok(Some(_)) => (),
                    Ok(None) => break Err(anyhow::anyhow!("owner event socket closed during key delivery")),
                    Err(error) => break Err(error),
                }
            }
        } => result?,
    }
    let (mut room_a, mut room_b) =
        tokio::try_join!(a.join_voice(&voice.id), b.join_voice(&voice.id))?;
    let (a_source, b_source) = (source(), source());
    tokio::try_join!(
        room_a.publish_audio(a_source.clone()),
        room_b.publish_audio(b_source.clone())
    )?;

    let start = Instant::now();
    let end = start + MINUTE * minutes;
    let (a_id, b_id) = (room_a.identity(), room_b.identity());
    let checks = async {
        tokio::try_join!(
            check_chat(&mut a, &mut b, text, start, end, ttl),
            check_voice(&mut room_a, &b_id, 660, start, end),
            check_voice(&mut room_b, &a_id, 440, start, end),
        )
    };
    println!("Soaking for {minutes} minutes with both bots in voice");
    let outcome = tokio::select! {
        result = send_tone(a_source, || 440) => result.map(|()| unreachable!()),
        result = send_tone(b_source, || 660) => result.map(|()| unreachable!()),
        result = checks => result,
    };
    if outcome.is_ok() {
        let finished = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64();
        std::fs::write(data.join("soak-finished"), finished.to_string())?;
    }
    let left = tokio::join!(room_a.leave(), room_b.leave());
    let ((messages, refreshes), a_checks, b_checks) = outcome?;
    left.0?;
    left.1?;
    ensure!(
        messages == u64::from(minutes) * 2,
        "missed a scheduled chat exchange"
    );
    ensure!(
        a_checks == u64::from(minutes) * 2 && a_checks == b_checks,
        "missed a scheduled tone check"
    );
    if u64::from(minutes) * 60 >= ttl {
        ensure!(
            refreshes[0] > 0 && refreshes[1] > 0,
            "both sessions must refresh"
        );
    }
    println!(
        "Soak bots: duration={minutes}m messages={messages} tone checks={} token refreshes={}+{} reconnects=0",
        a_checks + b_checks,
        refreshes[0],
        refreshes[1],
    );
    Ok(())
}

async fn exchange(
    sender: &mut KeyClient<'_>,
    receiver: &mut KeyClient<'_>,
    text: Scope,
    message: String,
) -> Result<()> {
    let body = MessageBody {
        text: message,
        ..Default::default()
    };
    let sent = sender.send_message(&text.id, &body).await?;
    wait_event(receiver, text, &sent.id, false).await?;
    let received = receiver.message(&text.id, &sent.id).await?;
    ensure!(
        receiver.open_message(&text.id, &received).await? == body,
        "soak message differed"
    );
    Ok(())
}

fn refresh_at(client: &KeyClient<'_>, ttl: u64) -> Instant {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let delay = (client.expires_at() as u64).saturating_sub(now + (ttl / 3).min(600));
    Instant::now() + Duration::from_secs(delay)
}

async fn check_chat(
    a: &mut KeyClient<'_>,
    b: &mut KeyClient<'_>,
    text: Scope,
    start: Instant,
    end: Instant,
    ttl: u64,
) -> Result<(u64, [u64; 2])> {
    let mut tick = interval_at(start, MINUTE);
    let mut a_refresh = refresh_at(a, ttl);
    let mut b_refresh = refresh_at(b, ttl);
    let mut refreshes = [0; 2];
    let mut messages = 0;
    loop {
        tokio::select! {
            biased;
            event = a.next_event() => { event?.context("owner event socket closed")?; }
            event = b.next_event() => { event?.context("member event socket closed")?; }
            _ = sleep_until(end) => return Ok((messages, refreshes)),
            _ = sleep_until(a_refresh) => {
                timeout(LIMIT, a.refresh()).await.context("owner refresh timed out")??;
                a_refresh = refresh_at(a, ttl);
                refreshes[0] += 1;
            }
            _ = sleep_until(b_refresh) => {
                timeout(LIMIT, b.refresh()).await.context("member refresh timed out")??;
                b_refresh = refresh_at(b, ttl);
                refreshes[1] += 1;
            }
            _ = tick.tick() => {
                timeout(LIMIT, exchange(a, b, text, format!("Soak message {messages}"))).await.context("owner chat timed out")??;
                messages += 1;
                timeout(LIMIT, exchange(b, a, text, format!("Soak message {messages}"))).await.context("member chat timed out")??;
                messages += 1;
            }
        }
    }
}

async fn check_voice(
    room: &mut VoiceRoom,
    peer: &ParticipantIdentity,
    hz: u32,
    start: Instant,
    end: Instant,
) -> Result<u64> {
    let mut tick = interval_at(start, TONE_INTERVAL);
    let mut checks = 0;
    loop {
        tokio::select! {
            biased;
            event = room.next_event() => check_room_event(event.context("LiveKit event stream closed")?)?,
            _ = sleep_until(end) => return Ok(checks),
            _ = tick.tick() => {
                timeout(LIMIT, hear_tone_with_events(room, Some(peer), hz, check_room_event))
                    .await.with_context(|| format!("missed {hz} Hz tone check {}", checks + 1))??;
                checks += 1;
            }
        }
    }
}

fn check_room_event(event: RoomEvent) -> Result<()> {
    match event {
        RoomEvent::Reconnecting => anyhow::bail!("LiveKit started reconnecting"),
        RoomEvent::Reconnected => anyhow::bail!("LiveKit reconnected"),
        RoomEvent::Disconnected { reason } => anyhow::bail!("LiveKit disconnected: {reason:?}"),
        _ => Ok(()),
    }
}
