use std::{env, path::Path};

use anyhow::{Context, Result};
use reqwest::{Method, StatusCode};
use rustls_pki_types::{CertificateDer, pem::PemObject};
use rusttalk_core::{
    auth::{KnownServer, decode, encode},
    client::{KeyClient, Server},
    identity::DeviceIdentity,
    pinned_tls::fingerprint,
};
use serde_json::{Value, json};

use super::{chat, compose, invite, receive_key, rejected, session};

mod dm;

const SETTINGS: &str = "/v1/settings/open-joining";

pub async fn exercise(
    first_project: &str,
    project: &str,
    server_a: &Server,
    owner_a: &mut KeyClient<'_>,
    pins: &Path,
    secrets: &mut Vec<String>,
) -> Result<()> {
    let data = env::var("RUSTTALK_E2E_SECOND_DATA")?;
    let port = env::var("RUSTTALK_E2E_SECOND_HTTPS_PORT")?;
    let cert = CertificateDer::from_pem_file(Path::new(&data).join("tls/cert.pem"))?;
    let cli = |command| compose(project, &["exec", "-T", "api", "rusttalk-api", command]);
    let known = KnownServer {
        address: format!("https://127.0.0.1:{port}/").parse()?,
        server_id: decode(&cli("server-id")?)?,
        tls_fingerprint: Some(fingerprint(cert.as_ref())),
    };
    assert_ne!(
        known.tls_fingerprint,
        server_a.known_server().tls_fingerprint
    );
    assert_ne!(known.server_id, server_a.known_server().server_id);
    let server_b = Server::connect(
        known.address.clone(),
        known.tls_fingerprint,
        Some(&known),
        pins,
    )
    .await?;
    let owner_code = cli("owner-invite")?;
    let owner_token = session(
        server_b
            .sign_in(owner_a.identity, Some(&decode(&owner_code)?))
            .await?,
        secrets,
    )
    .await?;
    secrets.push(owner_code);
    let mut owner_b = KeyClient::connect(&server_b, owner_a.identity, &owner_token).await?;

    let (_, peer) = DeviceIdentity::create("W15 peer")?;
    assert!(!server_b.info().await?.open_joining);
    rejected(
        server_b.join_open(&peer).await?,
        StatusCode::FORBIDDEN,
        "open_joining_disabled",
    )
    .await?;
    configure(&owner_b, 2).await?;
    assert!(server_b.info().await?.open_joining);
    assert!(!server_a.info().await?.open_joining);
    rejected(
        server_b.sign_in(&peer, None).await?,
        StatusCode::FORBIDDEN,
        "invite_required",
    )
    .await?;
    let peer_b_token = session(server_b.join_open(&peer).await?, secrets).await?;
    let mut peer_b = KeyClient::connect(&server_b, &peer, &peer_b_token).await?;
    let peer_id = encode(peer.certificate.person_id());
    let roster: Vec<Value> = peer_b
        .request(Method::GET, "/v1/members")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(roster.iter().any(|member| member["person_id"] == peer_id
        && member["role_ids"] == json!(["member"])));
    rejected(
        peer_b
            .request(Method::PUT, SETTINGS)
            .json(&json!({"open_joining": true, "member_cap": 10}))
            .send()
            .await?,
        StatusCode::FORBIDDEN,
        "permission_denied",
    )
    .await?;
    let (_, outsider) = DeviceIdentity::create("W15 outsider")?;
    rejected(
        server_b.join_open(&outsider).await?,
        StatusCode::CONFLICT,
        "member_cap_reached",
    )
    .await?;

    let (_, link) = invite(owner_a, secrets).await?;
    let peer_a_token = session(server_a.sign_in(&peer, Some(&link.code)).await?, secrets).await?;
    let mut peer_a = KeyClient::connect(server_a, &peer, &peer_a_token).await?;
    let category_a = owner_a.create_category(b"W15 server A").await?;
    let category_b = owner_b.create_category(b"W15 server B").await?;
    let text_a = owner_a
        .create_channel(&category_a, "text", b"Test chat", b"")
        .await?;
    let text_b = owner_b
        .create_channel(&category_b, "text", b"Test chat", b"")
        .await?;
    owner_a.sync().await?;
    owner_b.sync().await?;
    receive_key(&mut peer_a, text_a, 1).await?;
    receive_key(&mut peer_b, text_b, 1).await?;
    chat::exchange(owner_a, &mut peer_a, text_a, ["W15 A hello", "W15 A reply"]).await?;
    chat::exchange(
        &mut owner_b,
        &mut peer_b,
        text_b,
        ["W15 B hello", "W15 B reply"],
    )
    .await?;
    println!("W15 same bot identity exchanged encrypted chat on both servers");
    let dms = dm::replicas(
        project,
        &server_b,
        (&mut *owner_a, &mut owner_b),
        (&mut peer_a, &mut peer_b),
    )
    .await
    .context("W70 DM over two servers")?;
    // Server B's API restarted, which closed the event streams there.
    owner_b = KeyClient::connect(&server_b, owner_a.identity, &owner_token).await?;
    peer_b = KeyClient::connect(&server_b, &peer, &peer_b_token).await?;

    #[cfg(feature = "media")]
    {
        tone(owner_a, &mut peer_a, &category_a, 440)
            .await
            .context("tone on server A")?;
        tone(&mut owner_b, &mut peer_b, &category_b, 660)
            .await
            .context("tone on server B")?;
        println!("W15 same bot process heard encrypted tones on server A then server B");
    }

    owner_b
        .request(Method::PUT, &format!("/v1/members/{peer_id}/roles"))
        .json(&json!({"role_ids": ["admin", "member"]}))
        .send()
        .await?
        .error_for_status()?;
    configure(&peer_b, 100).await?;
    let settings: Value = owner_b
        .request(Method::GET, SETTINGS)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(settings, json!({"open_joining": true, "member_cap": 100}));
    session(server_b.join_open(&outsider).await?, secrets).await?;
    for _ in 0..3 {
        let (_, newcomer) = DeviceIdentity::create("W15 newcomer")?;
        session(server_b.join_open(&newcomer).await?, secrets).await?;
    }
    let (_, limited) = DeviceIdentity::create("W15 rate-limited")?;
    rejected(
        server_b.join_open(&limited).await?,
        StatusCode::TOO_MANY_REQUESTS,
        "open_join_limit",
    )
    .await?;
    println!("W15 open admission respected the setting, member cap and hourly source limit");

    let ban_path = format!("/v1/bans/{peer_id}");
    owner_b
        .request(Method::PUT, &ban_path)
        .json(&json!({}))
        .send()
        .await?
        .error_for_status()?;
    rejected(
        server_b.join_open(&peer).await?,
        StatusCode::FORBIDDEN,
        "person_banned",
    )
    .await?;
    let bans: Vec<Value> = owner_b
        .request(Method::GET, "/v1/bans")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(bans.len(), 1);
    assert_eq!(bans[0]["person_id"], peer_id);
    assert_eq!(bans[0]["ban_ip"], false);
    let page: Vec<Value> = owner_b
        .request(Method::GET, "/v1/audit?action=member_banned")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(page.len(), 1);
    assert_eq!(page[0]["at"], bans[0]["banned_at"]);
    assert!(page[0]["at"].as_i64().unwrap() > 0);
    owner_b
        .request(Method::DELETE, &ban_path)
        .send()
        .await?
        .error_for_status()?;
    let bans: Vec<Value> = owner_b
        .request(Method::GET, "/v1/bans")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(bans.is_empty());

    let filter = "/v1/audit?action=open_joining_changed&limit=1";
    let first: Vec<Value> = owner_b
        .request(Method::GET, filter)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let next = format!("{filter}&after={}", first[0]["id"]);
    let second: Vec<Value> = owner_b
        .request(Method::GET, &next)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_eq!(first[0]["action"], "open_joining_changed");
    assert_eq!(second[0]["action"], "open_joining_changed");
    assert!(first[0]["id"].as_i64() < second[0]["id"].as_i64());
    assert_eq!(second[0]["actor_person_id"], peer_id);
    println!("W15 bans had timestamps, and filtered audit paging advanced the cursor");
    dm::after_leaving(
        [first_project, project],
        (&mut *owner_a, &mut owner_b),
        &mut peer_a,
        dms,
    )
    .await
    .context("W70 DM after leaving server B")?;
    Ok(())
}

async fn configure(admin: &KeyClient<'_>, member_cap: i64) -> Result<()> {
    let reply = admin
        .request(Method::PUT, SETTINGS)
        .json(&json!({"open_joining": true, "member_cap": member_cap}))
        .send()
        .await?
        .error_for_status()?;
    assert_eq!(reply.status(), StatusCode::NO_CONTENT);
    Ok(())
}

#[cfg(feature = "media")]
async fn tone(
    owner: &mut KeyClient<'_>,
    peer: &mut KeyClient<'_>,
    category: &[u8; 32],
    hz: u32,
) -> Result<()> {
    use rusttalk_bot::voice::{hear_tone, send_tone, source};
    use std::time::Duration;
    use tokio::{task::JoinSet, time::timeout};

    let voice = owner
        .create_channel(category, "voice", b"Test voice", b"")
        .await?;
    owner.sync().await?;
    receive_key(peer, voice, 1).await?;
    let (mut listener, sender) =
        tokio::try_join!(owner.join_voice(&voice.id), peer.join_voice(&voice.id))?;
    let source = source();
    sender.publish_audio(source.clone()).await?;
    let mut tones = JoinSet::new();
    tones.spawn(send_tone(source, move || hz));
    timeout(
        Duration::from_secs(5),
        hear_tone(&mut listener, Some(&sender.identity()), hz),
    )
    .await??;
    tones.shutdown().await;
    tokio::try_join!(listener.leave(), sender.leave())?;
    Ok(())
}
