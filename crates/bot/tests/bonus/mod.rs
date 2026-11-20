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
    println!(
        "B2 bot saw online, away, invisible as offline, offline on the last socket closing, the stored choice on reconnect, and no notice for an invisible reconnect"
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
