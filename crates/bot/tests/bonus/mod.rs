use std::{io::Cursor, time::Duration};

use anyhow::{Context, Result, ensure};
use image::{DynamicImage, ImageFormat};
use reqwest::{Method, StatusCode};
use rusttalk_core::{
    auth::{Session, encode},
    client::{KeyClient, Server, decode_profile_image},
    identity::DeviceIdentity,
    keys::{Event, Metadata},
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
    // The directory reports what members were last told. A notice only triggers a re-check:
    // an older one for the same status can still be queued from a wait that ended early.
    super::wait::until(
        a,
        "presence change",
        async |a| Ok(member(a, person).await?["presence"] == json!(expected)),
        |_| false,
    )
    .await
}

/// Read every update already sent, so a later window only sees new ones.
async fn drain(a: &mut KeyClient<'_>) -> Result<()> {
    while let Ok(update) = timeout(Duration::from_millis(200), a.next_update()).await {
        update?.context("event socket closed")?;
    }
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
    // The wait above can end on the directory before the offline notice is read.
    drain(a).await?;
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

/// B sets the name and bio their client publishes to every server (#201); A, on the
/// same server, decrypts both from the member directory, which holds only ciphertext.
pub async fn profile(a: &mut KeyClient<'_>, b: &KeyClient<'_>) -> Result<()> {
    let person_id = b.identity.certificate.person_id();
    let person = encode(person_id);
    let bio = "Hi :wave: 👋\nhttps://example.org";
    drain(a).await?;
    b.set_display_name(b"Bea").await?;
    // The notice carries the new profile, so peers update B without reloading anything.
    let mut inline = None;
    super::wait::notice(a, "the profile notice", |update| {
        if let Update::Event(Event::MemberChanged {
            person: changed,
            profile: Some(profile),
            ..
        }) = update
            && *changed == person
        {
            inline = profile.display_name_ct.clone();
        }
        inline.is_some()
    })
    .await?;
    let inline = inline.context("missing inline name")?;
    assert_eq!(
        a.decrypt_metadata(Metadata::DisplayName, person_id, &inline)?,
        b"Bea"
    );
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
    println!(
        "B4b a peer decrypted another member's name from the inline notice and the directory, and saw the bio cleared"
    );
    Ok(())
}

/// Emoji packs: the Default pack, encrypted names, moving, reordering, limits and deleting.
async fn packs(a: &KeyClient<'_>, b: &KeyClient<'_>, png: &[u8]) -> Result<()> {
    let default = encode(&[0; 32]);
    let list = || async {
        Ok::<Value, anyhow::Error>(
            a.request(Method::GET, "/v1/emoji")
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?,
        )
    };
    let before = list().await?;
    assert_eq!(before["packs"].as_array().map(Vec::len), Some(1));
    assert_eq!(before["packs"][0]["id"], default);
    let cats = [51; 32];
    let new_pack = |client: &KeyClient<'_>, id: &[u8; 32], name: &[u8]| -> Result<Value> {
        Ok(json!({
            "id": encode(id),
            "name_ct": client.encrypt_metadata(Metadata::EmojiPackName, id, name)?,
        }))
    };
    b.request(Method::POST, "/v1/emoji/packs")
        .json(&new_pack(b, &cats, b"Cats")?)
        .send()
        .await?
        .error_for_status()?;
    super::rejected(
        a.request(Method::POST, "/v1/emoji/packs")
            .json(&new_pack(a, &cats, b"Again")?)
            .send()
            .await?,
        StatusCode::CONFLICT,
        "pack_exists",
    )
    .await?;
    // An emoji goes into a named pack and moves between packs.
    let id = [52; 32];
    let item = json!({
        "id": encode(&id),
        "kind": "emoji",
        "name_ct": b.encrypt_metadata(Metadata::EmojiName, &id, b"meow")?,
        "image_ct": b.encrypt_metadata(Metadata::EmojiImage, &id, png)?,
        "pack_id": encode(&cats),
    });
    b.request(Method::POST, "/v1/emoji")
        .json(&item)
        .send()
        .await?
        .error_for_status()?;
    let listed = list().await?;
    let row = &listed["items"][0];
    assert_eq!(row["pack_id"], encode(&cats));
    let name = listed["packs"][1]["name_ct"]
        .as_str()
        .context("pack name")?;
    assert_eq!(
        a.decrypt_metadata(Metadata::EmojiPackName, &cats, name)?,
        b"Cats"
    );
    b.request(Method::PUT, &format!("/v1/emoji/{}/pack", encode(&id)))
        .json(&json!({"pack_id": default}))
        .send()
        .await?
        .error_for_status()?;
    b.request(Method::PUT, "/v1/emoji/packs/order")
        .json(&json!({"ids": [encode(&cats), default]}))
        .send()
        .await?
        .error_for_status()?;
    let listed = list().await?;
    assert_eq!(listed["packs"][0]["id"], encode(&cats));
    assert_eq!(listed["items"][0]["pack_id"], default);
    let renamed = b.encrypt_metadata(Metadata::EmojiPackName, &cats, b"Kittens")?;
    b.request(
        Method::PUT,
        &format!("/v1/emoji/packs/{}/name", encode(&cats)),
    )
    .json(&json!({"name_ct": renamed}))
    .send()
    .await?
    .error_for_status()?;
    // Deleting a pack deletes what is in it; the last pack stays.
    b.request(Method::PUT, &format!("/v1/emoji/{}/pack", encode(&id)))
        .json(&json!({"pack_id": encode(&cats)}))
        .send()
        .await?
        .error_for_status()?;
    b.request(
        Method::DELETE,
        &format!("/v1/emoji/packs/{}", encode(&cats)),
    )
    .send()
    .await?
    .error_for_status()?;
    let after = list().await?;
    assert_eq!(after["items"].as_array().map(Vec::len), Some(0));
    super::rejected(
        b.request(Method::DELETE, &format!("/v1/emoji/packs/{default}"))
            .send()
            .await?,
        StatusCode::CONFLICT,
        "last_pack",
    )
    .await?;
    println!("B5 packs: named, moved, reordered and deleted with their emoji; the last pack stays");
    Ok(())
}

/// Server emoji: the Manage emoji permission, encrypted names and pictures, limits,
/// rename, delete, audit entries and the change event.
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
    super::wait::notice(a, "the emoji change event", |update| {
        matches!(
            update,
            Update::Event(rusttalk_core::keys::Event::ChannelChanged {
                part: Some(rusttalk_core::keys::Part::Emoji)
            })
        )
    })
    .await?;
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
    packs(a, b, &png).await?;
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
