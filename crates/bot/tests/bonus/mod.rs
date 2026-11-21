use std::{io::Cursor, time::Duration};

use anyhow::{Context, Result, ensure};
use image::{DynamicImage, ImageFormat};
use reqwest::{Method, StatusCode};
use rusttalk_core::{
    auth::{Session, encode},
    client::{KeyClient, Server, decode_profile_image},
    identity::DeviceIdentity,
    keys::Metadata,
    presence::{Presence, PresenceEvent, Status, Update},
};
use serde_json::{Value, json};
use tokio::time::timeout;

async fn member(client: &KeyClient<'_>, person: &str) -> Result<Value> {
    let rows: Vec<Value> = client
        .request(Method::GET, "/v1/members")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    rows.into_iter()
        .find(|row| row["person_id"] == person)
        .context("member missing from directory")
}

async fn presence_is(a: &mut KeyClient<'_>, person: &str, expected: Presence) -> Result<()> {
    timeout(Duration::from_secs(5), async {
        loop {
            let update = a.next_update().await?.context("presence socket closed")?;
            if let Update::Presence(PresenceEvent::PresenceChanged { person_id, status }) = update
                && person_id == person
            {
                assert_eq!(status, expected);
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await
    .context("waiting for presence event")??;
    assert_eq!(member(a, person).await?["presence"], json!(expected));
    Ok(())
}

pub async fn presence<'a>(
    a: &mut KeyClient<'_>,
    bot: &'a Server,
    identity: &'a DeviceIdentity,
    token: &Session,
) -> Result<KeyClient<'a>> {
    let person = encode(identity.certificate.person_id());
    assert_eq!(member(a, &person).await?["presence"], "offline");
    let b = KeyClient::connect(bot, identity, token).await?;
    presence_is(a, &person, Presence::Online).await?;
    let sibling_socket = KeyClient::connect(bot, identity, token).await?;
    drop(sibling_socket);
    b.set_status(Status::Away).await?;
    presence_is(a, &person, Presence::Away).await?;
    b.set_status(Status::Invisible).await?;
    presence_is(a, &person, Presence::Offline).await?;
    b.set_status(Status::Online).await?;
    presence_is(a, &person, Presence::Online).await?;
    b.set_status(Status::Away).await?;
    presence_is(a, &person, Presence::Away).await?;
    drop(b);
    presence_is(a, &person, Presence::Offline).await?;
    // The stored choice is announced on reconnect.
    let b = KeyClient::connect(bot, identity, token).await?;
    presence_is(a, &person, Presence::Away).await?;

    // The choice outlives the socket, so an invisible member reconnects unannounced.
    b.set_status(Status::Invisible).await?;
    presence_is(a, &person, Presence::Offline).await?;
    drop(b);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let b = KeyClient::connect(bot, identity, token).await?;
    let leaked = timeout(Duration::from_millis(1500), async {
        loop {
            let update = a.next_update().await?.context("presence socket closed")?;
            if let Update::Presence(PresenceEvent::PresenceChanged { person_id, .. }) = update
                && person_id == person
            {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await;
    assert!(
        leaked.is_err(),
        "invisible member was announced on reconnect"
    );
    assert_eq!(member(a, &person).await?["presence"], "offline");
    let me: Value = b
        .request(Method::GET, "/v1/me")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(me["status"], "invisible");
    b.set_status(Status::Online).await?;
    presence_is(a, &person, Presence::Online).await?;
    assert_eq!(me["hide_offline"], false);
    b.set_hide_offline(true).await?;
    assert_eq!(member(a, &person).await?["hide_offline"], true);
    b.set_hide_offline(false).await?;
    assert_eq!(member(a, &person).await?["hide_offline"], false);
    println!(
        "B2 bot saw online, away, invisible as offline, offline on the last socket closing, the stored choice on reconnect, no notice for an invisible reconnect, and the hide-offline flag"
    );
    Ok(b)
}

pub async fn icons(a: &KeyClient<'_>, b: &KeyClient<'_>) -> Result<()> {
    let mut png = Cursor::new(Vec::new());
    DynamicImage::new_rgba8(3, 2).write_to(&mut png, ImageFormat::Png)?;
    let png = png.into_inner();
    let person_id = b.identity.certificate.person_id();
    let server_id = a.space().id;
    let scope_before: Value = a
        .request(Method::GET, "/v1/space")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let icon_ct = b.encrypt_metadata(Metadata::ServerIcon, &server_id, &png)?;
    super::rejected(
        b.request(Method::PUT, "/v1/space")
            .json(&json!({"icon_ct": icon_ct}))
            .send()
            .await?,
        StatusCode::FORBIDDEN,
        "permission_denied",
    )
    .await?;

    let role: Value = a
        .request(Method::POST, "/v1/roles")
        .json(&json!({"rank": 1, "permissions": ["manage_server"]}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let endpoint = format!("/v1/members/{}/roles", encode(person_id));
    a.request(Method::PUT, &endpoint)
        .json(&json!({"role_ids": ["member", role["id"]]}))
        .send()
        .await?
        .error_for_status()?;

    b.set_server_icon(&png).await?;
    b.set_display_name(b"Bonus profile").await?;
    b.set_avatar(&png).await?;
    let space: Value = a
        .request(Method::GET, "/v1/space")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let profile = member(a, &encode(person_id)).await?;
    // Directory loads carry only short hashes; the pictures are fetched on their own.
    ensure!(space["icon_hash"].is_string() && profile["avatar_hash"].is_string());
    ensure!(space.get("icon_ct").is_none() && profile.get("avatar_ct").is_none());
    let icon: Value = a
        .request(Method::GET, "/v1/space/icon")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let avatar: Value = a
        .request(
            Method::GET,
            &format!("/v1/members/{}/avatar", encode(person_id)),
        )
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    for (kind, id, ct) in [
        (Metadata::ServerIcon, &server_id, &icon["icon_ct"]),
        (Metadata::Avatar, person_id, &avatar["avatar_ct"]),
    ] {
        let opened = a.decrypt_metadata(kind, id, ct.as_str().context("missing image")?)?;
        assert_eq!(opened, png);
        assert_eq!(
            decode_profile_image(&opened)?[0].buffer().dimensions(),
            (3, 2)
        );
    }
    assert_eq!(space["name_ct"], scope_before["name_ct"]);
    assert_eq!(
        a.decrypt_metadata(
            Metadata::DisplayName,
            person_id,
            profile["display_name_ct"]
                .as_str()
                .context("missing display name")?
        )?,
        b"Bonus profile"
    );
    a.request(Method::PUT, &endpoint)
        .json(&json!({"role_ids": ["member"]}))
        .send()
        .await?
        .error_for_status()?;
    println!(
        "B4 custom manage_server grant allowed an icon update, and a peer decrypted the icon and avatar"
    );
    Ok(())
}

/// Server emoji: the Manage emoji permission, encrypted names and pictures, limits,
/// rename, delete, audit entries and the change event.
/// B sets the name and bio their client publishes to every server (#201); A, on the
/// same server, decrypts both from the member directory, which holds only ciphertext.
pub async fn profile(a: &KeyClient<'_>, b: &KeyClient<'_>) -> Result<()> {
    let person_id = b.identity.certificate.person_id();
    let person = encode(person_id);
    let bio = "Hi :wave: 👋\nhttps://example.org";
    b.set_display_name(b"Bea").await?;
    b.set_bio(bio).await?;
    let row = member(a, &person).await?;
    let open = |kind, field: &str| -> Result<Vec<u8>> {
        let ct = row[field].as_str().context("missing profile field")?;
        ensure!(
            !ct.contains("Bea") && !ct.contains("wave"),
            "profile field is not encrypted"
        );
        a.decrypt_metadata(kind, person_id, ct)
    };
    assert_eq!(open(Metadata::DisplayName, "display_name_ct")?, b"Bea");
    assert_eq!(open(Metadata::Bio, "bio_ct")?, bio.as_bytes());
    b.set_bio("").await?;
    assert_eq!(member(a, &person).await?["bio_ct"], Value::Null);
    println!("B4b a peer decrypted another member's name and bio, and saw the bio cleared");
    Ok(())
}

pub async fn emoji(a: &mut KeyClient<'_>, b: &KeyClient<'_>) -> Result<()> {
    let mut png = Cursor::new(Vec::new());
    DynamicImage::new_rgba8(4, 4).write_to(&mut png, ImageFormat::Png)?;
    let png = png.into_inner();
    let body = |client: &KeyClient<'_>, id: &[u8; 32], kind: &str, name: &[u8]| -> Result<Value> {
        Ok(json!({
            "id": encode(id),
            "kind": kind,
            "name_ct": client.encrypt_metadata(Metadata::EmojiName, id, name)?,
            "image_ct": client.encrypt_metadata(Metadata::EmojiImage, id, &png)?,
        }))
    };
    let add = |client: &KeyClient<'_>, body: Value| {
        client.request(Method::POST, "/v1/emoji").json(&body).send()
    };
    let party = [41; 32];
    super::rejected(
        add(b, body(b, &party, "emoji", b"party")?).await?,
        StatusCode::FORBIDDEN,
        "permission_denied",
    )
    .await?;

    let person_id = b.identity.certificate.person_id();
    let role: Value = a
        .request(Method::POST, "/v1/roles")
        .json(&json!({"rank": 1, "permissions": ["manage_emoji"]}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let roles = format!("/v1/members/{}/roles", encode(person_id));
    a.request(Method::PUT, &roles)
        .json(&json!({"role_ids": ["member", role["id"]]}))
        .send()
        .await?
        .error_for_status()?;

    add(b, body(b, &party, "emoji", b"party")?)
        .await?
        .error_for_status()?;
    // Everyone hears that the server changed and reloads its emoji with the directory.
    timeout(Duration::from_secs(5), async {
        loop {
            let update = a.next_update().await?.context("event socket closed")?;
            if matches!(
                update,
                Update::Event(rusttalk_core::keys::Event::ChannelChanged)
            ) {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await
    .context("waiting for the emoji change event")??;
    super::rejected(
        add(b, body(b, &party, "emoji", b"again")?).await?,
        StatusCode::CONFLICT,
        "emoji_exists",
    )
    .await?;
    add(b, body(b, &[42; 32], "sticker", b"hug")?)
        .await?
        .error_for_status()?;
    // The e2e stack allows one sticker.
    super::rejected(
        add(b, body(b, &[43; 32], "sticker", b"hug2")?).await?,
        StatusCode::CONFLICT,
        "sticker_limit",
    )
    .await?;
    let mut oversized = body(b, &[44; 32], "emoji", b"big")?;
    oversized["image_ct"] = json!(encode(&vec![0; 256 * 1024 + 1]));
    let reply = add(b, oversized).await?;
    ensure!(
        reply.status() == StatusCode::PAYLOAD_TOO_LARGE,
        "oversized emoji got {}",
        reply.status()
    );

    let list: Value = a
        .request(Method::GET, "/v1/emoji")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(list["max_stickers"], 1);
    let items = list["items"].as_array().context("no emoji list")?;
    assert_eq!(items.len(), 2);
    let row = &items[0];
    assert_eq!(row["id"], encode(&party));
    assert_eq!(row["kind"], "emoji");
    assert_eq!(row["owner"], encode(person_id));
    ensure!(row["image_hash"].is_string() && row.get("image_ct").is_none());
    let name = row["name_ct"].as_str().context("missing name")?;
    assert_eq!(
        a.decrypt_metadata(Metadata::EmojiName, &party, name)?,
        b"party"
    );
    let image: Value = a
        .request(Method::GET, &format!("/v1/emoji/{}/image", encode(&party)))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let image = image["image_ct"].as_str().context("missing image")?;
    assert_eq!(
        a.decrypt_metadata(Metadata::EmojiImage, &party, image)?,
        png
    );

    let name_ct = b.encrypt_metadata(Metadata::EmojiName, &party, b"party_time")?;
    b.request(Method::PUT, &format!("/v1/emoji/{}/name", encode(&party)))
        .json(&json!({"name_ct": name_ct}))
        .send()
        .await?
        .error_for_status()?;
    for id in [party, [42; 32]] {
        b.request(Method::DELETE, &format!("/v1/emoji/{}", encode(&id)))
            .send()
            .await?
            .error_for_status()?;
    }
    super::rejected(
        a.request(Method::GET, &format!("/v1/emoji/{}/image", encode(&party)))
            .send()
            .await?,
        StatusCode::NOT_FOUND,
        "emoji_not_found",
    )
    .await?;
    for action in ["emoji_added", "emoji_renamed", "emoji_deleted"] {
        let audit: Vec<Value> = a
            .request(
                Method::GET,
                &format!("/v1/audit?after=0&limit=100&action={action}"),
            )
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            audit
                .iter()
                .any(|e| e["action"] == action && e["target_id"] == encode(&party)),
            "missing audit entry {action}"
        );
    }
    a.request(Method::PUT, &roles)
        .json(&json!({"role_ids": ["member"]}))
        .send()
        .await?
        .error_for_status()?;
    println!(
        "B5 manage_emoji grant allowed emoji and stickers within limits, a peer decrypted names and pictures, and rename and delete were audited"
    );
    Ok(())
}
