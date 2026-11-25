use std::{env, path::Path};

use anyhow::{Context, Result};
use reqwest::{Method, StatusCode};
use rustls_pki_types::{CertificateDer, pem::PemObject};
use rusttalk_core::{
    auth::{KnownServer, MessageHistory, decode, encode},
    client::{KeyClient, Server},
    identity::DeviceIdentity,
    pinned_tls::fingerprint,
};
use serde_json::{Value, json};

use super::{chat, compose, invite, receive_key, rejected, session};

mod dm;

const SETTINGS: &str = "/v1/settings/open-joining";

/// A sticker of server A used on server B: the sender attaches its picture as a small file
/// of the sticker type on B, and a member of B downloads the same bytes.
async fn sticker_across_servers(
    owner_a: &KeyClient<'_>,
    owner_b: &mut KeyClient<'_>,
    peer_b: &mut KeyClient<'_>,
    text_b: rusttalk_core::keys::Scope,
) -> Result<()> {
    use rusttalk_core::client::{MessageBody, prepare_file};
    const TYPE: &str = "application/vnd.rusttalk.sticker";
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(8, 8).write_to(&mut png, image::ImageFormat::Png)?;
    let png = png.into_inner();
    let id = [61; 32];
    owner_a
        .request(Method::POST, "/v1/emoji")
        .json(&json!({
            "id": encode(&id),
            "kind": "sticker",
            "name_ct": owner_a.encrypt_metadata(rusttalk_core::keys::Metadata::EmojiName, &id, b"hug")?,
            "image_ct": owner_a.encrypt_metadata(rusttalk_core::keys::Metadata::EmojiImage, &id, &png)?,
        }))
        .send()
        .await?
        .error_for_status()?;
    // The app reads the picture from A, then uploads it to B without a thumbnail.
    let image: Value = owner_a
        .request(Method::GET, &format!("/v1/emoji/{}/image", encode(&id)))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let bytes = owner_a.decrypt_metadata(
        rusttalk_core::keys::Metadata::EmojiImage,
        &id,
        image["image_ct"].as_str().context("sticker picture")?,
    )?;
    let (attachment, _) = prepare_file("hug.png", TYPE, &bytes, &[])?;
    owner_b
        .upload_prepared(&text_b.id, &attachment, &bytes, None, None)
        .await?;
    let post = owner_b
        .send_message(
            &text_b.id,
            &MessageBody {
                text: ":hug:".into(),
                attachments: vec![attachment],
                ..Default::default()
            },
        )
        .await?;
    let record = peer_b.message(&text_b.id, &post.id).await?;
    let body = peer_b.open_message(&text_b.id, &record).await?;
    let [file] = body.attachments.as_slice() else {
        anyhow::bail!("expected the sticker file")
    };
    assert_eq!((&*file.name, &*file.media_type), ("hug.png", TYPE));
    assert!(file.thumbnail.is_none());
    assert_eq!(peer_b.download_file(&file.file, None).await?, png);
    owner_a
        .request(Method::DELETE, &format!("/v1/emoji/{}", encode(&id)))
        .send()
        .await?
        .error_for_status()?;
    println!("W203 a sticker of server A reached members of server B as a small file");
    Ok(())
}

async fn avatar_across_servers(
    source: &KeyClient<'_>,
    destination: &KeyClient<'_>,
    observer: &KeyClient<'_>,
) -> Result<()> {
    use rusttalk_core::keys::Metadata;
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        8,
        8,
        image::Rgba([50, 160, 230, 255]),
    ))
    .write_to(&mut png, image::ImageFormat::Png)?;
    let png = png.into_inner();
    source.set_avatar(&png).await?;
    let person = source.identity.certificate.person_id();
    let path = format!("/v1/members/{}/avatar", encode(person));
    let read = |client: &KeyClient<'_>| client.request(Method::GET, &path).send();
    let original: Value = read(source).await?.error_for_status()?.json().await?;
    let bytes = source.decrypt_metadata(
        Metadata::Avatar,
        person,
        original["avatar_ct"].as_str().context("source avatar")?,
    )?;
    assert!(destination.publish_default_avatar(&bytes).await?);
    let received: Value = read(observer).await?.error_for_status()?.json().await?;
    assert_ne!(original["avatar_ct"], received["avatar_ct"]);
    assert_eq!(
        observer.decrypt_metadata(
            Metadata::Avatar,
            person,
            received["avatar_ct"]
                .as_str()
                .context("destination avatar")?
        )?,
        png
    );
    assert!(!destination.publish_default_avatar(&bytes).await?);
    let unchanged: Value = read(observer).await?.error_for_status()?.json().await?;
    assert_eq!(received, unchanged);
    let mut newer = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        8,
        8,
        image::Rgba([230, 60, 90, 255]),
    ))
    .write_to(&mut newer, image::ImageFormat::Png)?;
    destination.set_avatar(newer.get_ref()).await?;
    assert!(!destination.publish_default_avatar(&bytes).await?);
    let override_image: Value = read(observer).await?.error_for_status()?.json().await?;
    assert_eq!(
        observer.decrypt_metadata(
            Metadata::Avatar,
            person,
            override_image["avatar_ct"]
                .as_str()
                .context("newer avatar")?
        )?,
        *newer.get_ref()
    );

    destination
        .request(Method::PUT, "/v1/me/profile")
        .json(&json!({"avatar_ct": null}))
        .send()
        .await?
        .error_for_status()?;
    assert!(!destination.publish_default_avatar(&bytes).await?);
    assert_eq!(read(observer).await?.status(), StatusCode::NOT_FOUND);
    println!(
        "W357 synthetic avatar re-encrypted across isolated servers, independently decrypted, duplicate and cleared publications rejected"
    );
    Ok(())
}

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

    let (peer_words, peer) = DeviceIdentity::create("W15 peer")?;
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
    // Open joining has no invite-wrapped space key. Publication must wait, not
    // send plaintext or mark the destination initialized before a key arrives.
    assert!(!peer_b.has_space_key());
    let mut avatar = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(8, 8).write_to(&mut avatar, image::ImageFormat::Png)?;
    assert!(
        peer_b
            .publish_default_avatar(avatar.get_ref())
            .await
            .is_err()
    );
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
    avatar_across_servers(&peer_a, &peer_b, &owner_b).await?;
    sticker_across_servers(owner_a, &mut owner_b, &mut peer_b, text_b)
        .await
        .context("a sticker of server A sent on server B")?;
    let dms = dm::replicas(
        project,
        &server_b,
        (&mut *owner_a, &mut owner_b),
        (&mut peer_a, &mut peer_b),
    )
    .await
    .context("W70 DM over two servers")?;
    // Server B's API restarted, which closed the event streams there. Reconnect with
    // the DM pins kept, as the app does, so nothing needs re-keying.
    let (owner_pins, peer_pins) = (owner_b.dms().clone(), peer_b.dms().clone());
    owner_b =
        KeyClient::connect_with_dms(&server_b, owner_a.identity, &owner_token, Some(owner_pins))
            .await?;
    peer_b = KeyClient::connect_with_dms(&server_b, &peer, &peer_b_token, Some(peer_pins)).await?;
    assert!(!peer_b.publish_default_avatar(avatar.get_ref()).await?);
    println!("W357 explicit avatar clear survived API restart and client reconnect");

    dm::friends(
        (&mut *owner_a, &mut owner_b),
        (&mut peer_a, &mut peer_b),
        dms,
    )
    .await
    .context("W71 friends over a DM")?;

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

    // Server B keeps B2 enrolled and accepts its posts. Only the owner client's
    // observation on server A can prevent B from obtaining another wrap.
    let sibling = DeviceIdentity::add_device(&peer_words, "B2")?;
    let stale_profile = Path::new(&data).join("b2-profile");
    let stale = Server::connect(
        known.address.clone(),
        known.tls_fingerprint,
        Some(&known),
        &stale_profile,
    )
    .await?;
    let token = session(stale.sign_in(&sibling, None).await?, secrets).await?;
    let mut sibling_client = KeyClient::connect(&stale, &sibling, &token).await?;
    owner_b.sync().await?;
    receive_key(&mut sibling_client, text_b, 1).await?;
    let device = sibling.certificate.signing_key().to_bytes();
    #[cfg(feature = "media")]
    let (mut listening, speaking, voice_scope) = {
        use rusttalk_core::client::voice::RoomEvent;
        let voice = owner_b
            .create_channel(&category_b, "voice", b"Revocation check", b"")
            .await?;
        owner_b.sync().await?;
        receive_key(&mut sibling_client, voice, 1).await?;
        let (mut listening, speaking) = tokio::try_join!(
            owner_b.join_voice(&voice.id),
            sibling_client.join_voice(&voice.id)
        )?;
        speaking
            .publish_audio(rusttalk_bot::voice::source())
            .await?;
        super::wait::within("B2 microphone subscription", async {
            while let Some(event) = listening.next_event().await {
                if matches!(event, RoomEvent::TrackSubscribed { participant, .. } if participant.identity() == speaking.identity()) {
                    return Ok(());
                }
            }
            anyhow::bail!("room closed before B2 subscribed")
        }).await?;
        (listening, speaking, voice)
    };
    let revoked =
        rusttalk_core::identity::revoke_device(&peer_words, peer.certificate.person_id(), device)?;
    peer_a
        .request(Method::POST, "/v1/me/revocations")
        .json(&json!({"record": encode(&revoked.to_bytes())}))
        .send()
        .await?
        .error_for_status()?;
    owner_a.sync().await?;
    let post = sibling_client
        .send_message(
            &text_b.id,
            &rusttalk_core::client::MessageBody {
                text: "stale device".into(),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(
        owner_b
            .open_message(&text_b.id, &post)
            .await
            .err()
            .context("accepted revoked sender")?
            .to_string(),
        "device revoked"
    );
    #[cfg(feature = "media")]
    {
        use rusttalk_core::client::voice::RoomEvent;
        super::wait::within("B2 microphone unsubscription on the stale server", async {
            while let Some(event) = listening.next_event().await {
                if matches!(event, RoomEvent::TrackUnsubscribed { participant, .. } if participant.identity() == speaking.identity()) {
                    return Ok(());
                }
            }
            anyhow::bail!("room closed before B2 unsubscribed")
        }).await?;
        assert!(listening.remote_audio_tracks().is_empty());
        owner_b.sync().await?;
        assert!(listening.sync_key(&owner_b)?);
        assert!(listening.epoch() > speaking.epoch());
        assert!(
            owner_b.current_key(voice_scope)?.1
                != sibling_client.key(voice_scope, speaking.epoch()).unwrap()
        );
        tokio::try_join!(listening.leave(), speaking.leave())?;
        println!(
            "W109 B2 microphone unsubscribed during a call after another server delivered its revocation"
        );
    }
    // B still serves ciphertext to B2 and has never asked for rotation. A reader
    // that learned the proof on A must initiate it here, including the voice scope.
    let old_key = sibling_client.current_key(text_b)?.1.clone();
    owner_b.sync().await?;
    let (epoch, fresh) = owner_b.current_key(text_b)?;
    assert!(epoch > 1 && fresh != &old_key);
    let after = owner_b
        .send_message(
            &text_b.id,
            &rusttalk_core::client::MessageBody {
                text: "after device rotation".into(),
                ..Default::default()
            },
        )
        .await?;
    let copy = sibling_client.message(&text_b.id, &after.id).await?;
    let context = rusttalk_core::messages::MessageContext {
        server_id: known.server_id,
        channel_id: text_b.id,
        epoch: copy.epoch,
        id: copy.id,
        sender_device: copy.sender_device,
        person: copy.person,
    };
    let bytes = rusttalk_core::auth::decode_bytes(&copy.envelope)?;
    assert_eq!(copy.epoch, epoch);
    assert!(matches!(
        context.open(&old_key, &owner_b.identity.certificate, &bytes),
        Err(rusttalk_core::messages::MessageError::Authentication)
    ));
    peer_b.sync().await?;
    assert_eq!(
        peer_b.open_message(&text_b.id, &copy).await?.text,
        "after device rotation"
    );
    sibling_client.sync().await?;
    assert!(sibling_client.key(text_b, epoch).is_none());
    owner_b.sync().await?;
    assert_eq!(
        owner_b.current_key(text_b)?.0,
        epoch,
        "replay must not rotate twice"
    );
    let new_scope = owner_b
        .create_channel(&category_b, "text", b"After revocation", b"")
        .await?;
    owner_b.sync().await?;
    let state: rusttalk_core::keys::KeyState = owner_b
        .request(Method::GET, "/v1/keys")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(
        state
            .pending
            .iter()
            .any(|need| need.scope == new_scope && need.device == encode(&device))
    );
    println!("W109 server B still accepted B2, but the peer refused its message and key wrap");

    // An authenticated sync blob can be made by any device holding the sync key.
    // A restored owner must relay only records signed by the owner's root.
    use rusttalk_core::client::{revocations::Revocations, sync};
    let (foreign_words, foreign) = DeviceIdentity::create("foreign signer")?;
    let forged = rusttalk_core::identity::revoke_device(
        &foreign_words,
        foreign.certificate.person_id(),
        peer.certificate.signing_key().to_bytes(),
    )?;
    let signed = encode(&revoked.to_bytes());
    let mut snapshot = sync::Snapshot::default();
    snapshot.state.revocations = [signed.clone(), encode(&forged.to_bytes())].into();
    let source = [peer_a.sync_peer()];
    assert!(
        sync::push_all(&source, &peer.sync_key, &mut snapshot)
            .await?
            .is_empty()
    );
    let (snapshots, failures) = sync::load_all(&source, &peer.sync_key).await;
    assert!(failures.is_empty());
    let relay = Revocations::in_profile(&pins.join("restored-owner"));
    let (kept, failures) = relay
        .relay(
            peer.certificate.person_id(),
            &snapshots,
            &[peer_b.sync_peer()],
        )
        .await?;
    assert_eq!(kept, [signed.clone()].into());
    assert!(failures.is_empty());
    let after: rusttalk_core::keys::KeyState = peer_b
        .request(Method::GET, "/v1/keys")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(after.revocations.contains(&(peer_id.clone(), signed)));
    assert_eq!(
        sibling_client
            .request(Method::GET, "/v1/me")
            .send()
            .await?
            .status(),
        StatusCode::UNAUTHORIZED
    );
    println!(
        "W109 owner relayed the verified revocation from encrypted sync to B and discarded the forged record"
    );

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
    message_history(&mut owner_b, &category_b)
        .await
        .context("#518 message history on server B")?;
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
        super::wait::hang_guard(),
        hear_tone(&mut listener, Some(&sender.identity()), hz),
    )
    .await??;
    tones.shutdown().await;
    tokio::try_join!(listener.leave(), sender.leave())?;
    Ok(())
}

/// Server B's host sets no cap: its owner keeps everything, then lowers the limit, which
/// deletes older messages at once. Last on B, since it trims every channel there.
async fn message_history(owner: &mut KeyClient<'_>, category: &[u8; 32]) -> Result<()> {
    const PATH: &str = "/v1/settings/message-history";
    let get = |owner: &KeyClient<'_>| owner.request(Method::GET, PATH).send();
    let initial: MessageHistory = get(owner).await?.error_for_status()?.json().await?;
    assert_eq!(
        initial,
        MessageHistory {
            keep: 10_000,
            ceiling: None
        }
    );
    let save = |owner: &KeyClient<'_>, keep: i64| {
        owner
            .request(Method::PUT, PATH)
            .json(&json!({ "keep": keep }))
            .send()
    };
    let all: MessageHistory = save(owner, 0).await?.error_for_status()?.json().await?;
    assert_eq!(all.keep, 0);

    // The peer left B, so the space key rotates before a new channel can be made.
    owner.sync().await?;
    let channel = owner
        .create_channel(category, "text", b"#518 history", b"")
        .await?;
    owner.sync().await?;
    let mut sent = Vec::new();
    for text in ["#518 one", "#518 two", "#518 three"] {
        let body = rusttalk_core::client::MessageBody {
            text: text.into(),
            ..Default::default()
        };
        sent.push(owner.send_message(&channel.id, &body).await?.id);
    }
    let kept: MessageHistory = save(owner, 2).await?.error_for_status()?.json().await?;
    assert_eq!(kept.keep, 2);
    let status = |id: &[u8; 32]| {
        owner
            .request(
                Method::GET,
                &format!(
                    "/v1/channels/{}/messages/{}",
                    encode(&channel.id),
                    encode(id)
                ),
            )
            .send()
    };
    assert_eq!(status(&sent[0]).await?.status(), StatusCode::NOT_FOUND);
    assert_eq!(status(&sent[1]).await?.status(), StatusCode::OK);
    assert_eq!(
        get(owner)
            .await?
            .error_for_status()?
            .json::<MessageHistory>()
            .await?
            .keep,
        2
    );
    println!("#518 owner kept every message, then a lower limit removed the oldest at once");
    Ok(())
}
