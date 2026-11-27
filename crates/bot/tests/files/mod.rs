use std::{io::Cursor, time::Duration};

use anyhow::{Context, Result};
use futures_util::stream;
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use reqwest::{Body, Method, StatusCode};
use rusttalk_core::{
    auth::encode,
    client::{KeyClient, MessageBody, MessageRecord, Server, decode_thumbnail, prepare_file},
    keys::random_key,
};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{compose, compose_bytes, receive_key, rejected};

const FILE: &str = "W13 Private File Malachite.bin";
const PNG: &str = "W13 Private Image Carnelian.png";
const MARKER: &[u8] = b"W13 Amethyst plaintext only on device";
const MIB: usize = 1024 * 1024;

/// Send `request`, waiting as asked while the server says to slow down (429), like the
/// app does: these checks follow many quick uploads.
async fn paced(request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
    loop {
        let again = request
            .try_clone()
            .context("a body that can be sent again")?;
        let reply = again.send().await?;
        if reply.status() != StatusCode::TOO_MANY_REQUESTS {
            return Ok(reply);
        }
        let wait = (reply.headers().get(reqwest::header::RETRY_AFTER))
            .and_then(|value| value.to_str().ok()?.parse().ok())
            .context("a 429 without Retry-After")?;
        tokio::time::sleep(Duration::from_secs(wait)).await;
    }
}

/// Pick a rate-limit preset as the owner.
async fn rate_limits(owner: &KeyClient<'_>, preset: &str) -> Result<()> {
    let reply = (owner.request(Method::PUT, "/v1/settings/rate-limits"))
        .json(&json!({ "preset": preset }))
        .send()
        .await?
        .error_for_status()?;
    let saved: serde_json::Value = reply.json().await?;
    anyhow::ensure!(saved["preset"] == preset, "preset not saved");
    Ok(())
}

fn check_storage(project: &str, live_id: &str) -> Result<()> {
    let raw = compose_bytes(
        project,
        &[
            "exec",
            "-T",
            "api",
            "sh",
            "-ec",
            "test -s /data/files/\"$1\"; cat /data/rusttalk.sqlite3 /data/rusttalk.sqlite3-wal /data/files/*",
            "sh",
            live_id,
        ],
    )?;
    let logs = compose_bytes(project, &["logs", "--no-color"])?;
    for data in [raw, logs] {
        for needle in [FILE.as_bytes(), PNG.as_bytes(), MARKER] {
            assert!(!data.windows(needle.len()).any(|window| window == needle));
        }
    }
    Ok(())
}

pub(super) async fn scenario(
    project: &str,
    server: &Server,
    a: &mut KeyClient<'_>,
    b: &mut KeyClient<'_>,
    c: &KeyClient<'_>,
    category: &[u8; 32],
) -> Result<()> {
    let channel = a
        .create_channel(category, "text", b"Stored files", b"")
        .await?;
    // B has the moderator role from the chat scenario. C remains a regular member.
    for (role, flags) in [("moderator", 3), ("member", 0)] {
        a.request(
            Method::PUT,
            &format!("/v1/channels/{}/permissions/{role}", encode(&channel.id)),
        )
        .json(&json!({"flags": flags}))
        .send()
        .await?
        .error_for_status()?;
    }
    a.sync().await?;
    let epoch = a.current_key(channel)?.0;
    receive_key(b, channel, epoch).await?;

    let mut bytes = vec![0x93; 2 * MIB];
    bytes[51..51 + MARKER.len()].copy_from_slice(MARKER);
    let mut png = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(RgbImage::from_pixel(640, 240, Rgb([19, 83, 211])))
        .write_to(&mut png, ImageFormat::Png)?;
    let png = png.into_inner();
    let file = a
        .upload_file(&channel.id, FILE, "application/octet-stream", &bytes, None)
        .await?;
    let image = a
        .upload_file(&channel.id, PNG, "image/png", &png, None)
        .await?;
    let id = encode(&file.file.id);
    let image_id = image.file.id;
    let path = format!("/v1/files/{id}");
    let post = a
        .send_message(
            &channel.id,
            &MessageBody {
                attachments: vec![file, image],
                ..Default::default()
            },
        )
        .await?;
    let record = b.message(&channel.id, &post.id).await?;
    let body = b.open_message(&channel.id, &record).await?;
    let [file, image] = body.attachments.as_slice() else {
        anyhow::bail!("expected two attachments")
    };
    assert_eq!(
        (&*file.name, &*file.media_type),
        (FILE, "application/octet-stream")
    );
    assert_eq!((&*image.name, &*image.media_type), (PNG, "image/png"));
    for (reference, expected) in [(&file.file, &bytes), (&image.file, &png)] {
        assert_eq!(
            Sha256::digest(b.download_file(reference, None).await?),
            Sha256::digest(expected)
        );
    }
    let thumbnail = b
        .download_file(image.thumbnail.as_ref().context("missing thumbnail")?, None)
        .await?;
    let thumbnail = decode_thumbnail(&thumbnail)?.into_rgb8();
    assert_eq!(thumbnail.dimensions(), (320, 120));
    assert_eq!(*thumbnail.get_pixel(100, 50), Rgb([19, 83, 211]));
    rejected(
        server.request(Method::GET, &path).send().await?,
        StatusCode::NOT_FOUND,
        "file_not_found",
    )
    .await?;

    // Ten pictures with the longest names and text: larger than older servers take and
    // than an inline event, so B reads it by its ID.
    assert_eq!(server.info().await?.max_files(), 10);
    let mut album = Vec::new();
    for index in 0..10 {
        let name = format!("{index:02} {}.png", "p".repeat(248));
        album.push(
            a.upload_file(&channel.id, &name, "image/png", &png, None)
                .await?,
        );
    }
    let post = a
        .send_message(
            &channel.id,
            &MessageBody {
                text: "🦀".repeat(rusttalk_core::messages::MAX_TEXT_CHARS),
                attachments: album.clone(),
                ..Default::default()
            },
        )
        .await?;
    let record = b.message(&channel.id, &post.id).await?;
    assert!(record.size > 10_752);
    let body = b.open_message(&channel.id, &record).await?;
    assert!(body.attachments == album);
    rejected(
        c.request(Method::GET, &path).send().await?,
        StatusCode::FORBIDDEN,
        "channel_read_denied",
    )
    .await?;

    let upload = |client: &KeyClient<'_>, id| {
        client
            .request(Method::POST, "/v1/files")
            .query(&[("channel", encode(&channel.id)), ("id", id)])
            .timeout(Duration::from_secs(30))
    };
    let chunks = stream::iter((0..33).map(|_| Ok::<_, std::io::Error>(vec![0xA3; MIB])));
    rejected(
        upload(a, encode(random_key()?.as_ref()))
            .body(Body::wrap_stream(chunks))
            .send()
            .await?,
        StatusCode::PAYLOAD_TOO_LARGE,
        "file_too_large",
    )
    .await?;

    // Each concurrent upload fits on its own. Only one fits atop the existing 2 MiB.
    let first = *random_key()?;
    let second = *random_key()?;
    let (first_reply, second_reply) = tokio::join!(
        upload(a, encode(&first))
            .body(vec![0xA3; 768 * 1024])
            .send(),
        upload(a, encode(&second))
            .body(vec![0xB4; 768 * 1024])
            .send(),
    );
    let (first_reply, second_reply) = (first_reply?, second_reply?);
    let (winner, refused) = if first_reply.status() == StatusCode::CREATED {
        (first, second_reply)
    } else {
        assert_eq!(second_reply.status(), StatusCode::CREATED);
        (second, first_reply)
    };
    rejected(refused, StatusCode::PAYLOAD_TOO_LARGE, "file_member_quota").await?;
    rejected(
        upload(b, encode(random_key()?.as_ref()))
            .body(vec![0xC5; 5 * MIB / 2])
            .send()
            .await?,
        StatusCode::PAYLOAD_TOO_LARGE,
        "file_server_quota",
    )
    .await?;
    check_storage(project, &id)?;
    a.delete_file(&winner).await?;
    // A deleted file's ID stays reserved.
    rejected(
        paced(upload(a, encode(&winner)).body(vec![0xA3; 1024])).await?,
        StatusCode::CONFLICT,
        "file_exists",
    )
    .await?;
    // B reposts A's image in a message of its own. Deleting that message as moderator names B as
    // the sender, so A's file survives. Deleting A's message names A and removes it.
    let repost = b
        .send_message(
            &channel.id,
            &MessageBody {
                attachments: vec![image.clone()],
                ..Default::default()
            },
        )
        .await?;
    b.delete_message(&channel.id, &repost.id).await?;
    let b_person = *b.identity.certificate.person_id();
    let error = b
        .delete_message_file(&image_id, &b_person)
        .await
        .expect_err("a moderator removed another member's file");
    assert_eq!(
        error
            .downcast_ref::<reqwest::Error>()
            .and_then(reqwest::Error::status),
        Some(StatusCode::NOT_FOUND)
    );
    b.download_file(&image.file, None).await?;
    let a_person = *a.identity.certificate.person_id();
    b.delete_message(&channel.id, &post.id).await?;
    b.delete_message_file(&image_id, &a_person).await?;
    rejected(
        a.request(Method::GET, &format!("/v1/files/{}", encode(&image_id)))
            .send()
            .await?,
        StatusCode::NOT_FOUND,
        "file_not_found",
    )
    .await?;

    // A shared file lists for readers with its sealed name and outlives the expiry sweep.
    let (shared, _) = prepare_file(FILE, "application/octet-stream", MARKER, &[])?;
    a.upload_prepared(&channel.id, &shared, MARKER, None, None)
        .await?;
    a.share_file(&channel.id, &shared).await?;
    let files_path = format!("/v1/channels/{}/files", encode(&channel.id));
    rejected(
        c.request(Method::GET, &files_path).send().await?,
        StatusCode::FORBIDDEN,
        "channel_read_denied",
    )
    .await?;
    // Shared-file records and chat messages are sealed under separate labels: neither
    // route takes the other's envelope, even under a matching ID.
    let listed: Vec<MessageRecord> = a
        .request(Method::GET, &files_path)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let record = listed.first().context("missing shared record")?;
    let as_message =
        json!({"id": encode(&record.id), "epoch": record.epoch, "envelope": record.envelope});
    rejected(
        a.request(
            Method::POST,
            &format!("/v1/channels/{}/messages", encode(&channel.id)),
        )
        .json(&as_message)
        .send()
        .await?,
        StatusCode::BAD_REQUEST,
        "invalid_message",
    )
    .await?;
    let (other, _) = prepare_file(FILE, "application/octet-stream", MARKER, &[])?;
    a.upload_prepared(&channel.id, &other, MARKER, None, None)
        .await?;
    let body = MessageBody {
        attachments: vec![other.clone()],
        ..Default::default()
    };
    let message = a.post_message(&channel.id, other.file.id, &body).await?;
    let as_shared =
        json!({"id": encode(&message.id), "epoch": message.epoch, "envelope": message.envelope});
    rejected(
        a.request(Method::POST, &files_path)
            .json(&as_shared)
            .send()
            .await?,
        StatusCode::BAD_REQUEST,
        "invalid_message",
    )
    .await?;
    a.delete_message(&channel.id, &message.id).await?;
    a.delete_file(&other.file.id).await?;

    tokio::time::timeout(Duration::from_secs(24), async {
        loop {
            let state = compose(
                project,
                &[
                    "exec",
                    "-T",
                    "api",
                    "sh",
                    "-ec",
                    "if [ -e /data/files/\"$1\" ]; then echo present; fi",
                    "sh",
                    &id,
                ],
            )?;
            if state.is_empty() {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .context("waiting for file expiry sweep")??;
    let listed = b.shared_files(&channel.id).await?;
    let [listed] = listed.as_slice() else {
        anyhow::bail!("expected one shared file")
    };
    assert_eq!((&*listed.file.name, listed.person), (FILE, a_person));
    assert_eq!(b.download_file(&listed.file.file, None).await?, MARKER);
    b.delete_message_file(&shared.file.id, &a_person).await?;
    assert!(b.shared_files(&channel.id).await?.is_empty());
    Box::pin(takedown(project, a, b, &channel.id, &png)).await?;
    // A channel keeps at most 500 shared files; the list returns them all. Filling it
    // at once is no normal use, so the owner relaxes the rate limits meanwhile.
    rate_limits(a, "relaxed").await?;
    for _ in 0..500 {
        share(a, &channel.id).await?;
    }
    let full = share(a, &channel.id)
        .await
        .expect_err("a 501st shared file");
    let rejected_code = full
        .downcast_ref::<rusttalk_core::client::UploadRejected>()
        .map(|r| (r.status, r.code.as_str()));
    assert_eq!(rejected_code, Some((StatusCode::CONFLICT, "shared_limit")));
    assert_eq!(shared_records(a, &channel.id).await?.len(), 500);
    rejected(
        a.request(Method::GET, &path).send().await?,
        StatusCode::NOT_FOUND,
        "file_not_found",
    )
    .await?;
    // An expired file's ID stays reserved.
    rejected(
        paced(upload(a, id.clone()).body(vec![0xA3; 1024])).await?,
        StatusCode::CONFLICT,
        "file_exists",
    )
    .await?;
    rate_limits(a, "standard").await?;
    let replacement = a
        .upload_file(&channel.id, FILE, "application/octet-stream", &bytes, None)
        .await?;
    check_storage(project, &encode(&replacement.file.id))?;
    println!(
        "W13 verified encrypted file SHA-256, PNG thumbnail, shared files kept past expiry, access, streaming cap, quota races, deletion, moderator deletes limited to the sender's files, expiry, no ID reuse and zero plaintext in SQLite, WAL, blobs and logs"
    );
    Ok(())
}

/// The stored ciphertext of a file, as the server keeps it.
async fn ciphertext(client: &KeyClient<'_>, id: &[u8; 32]) -> Result<Vec<u8>> {
    let path = format!("/v1/files/{}", encode(id));
    let reply = client.request(Method::GET, &path).send().await?;
    Ok(reply.error_for_status()?.bytes().await?.to_vec())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// W679: only the owner removes a message for a legal reason, with its picture, thumbnail
/// and a shared file; all of it leaves the server and its disk, and the record keeps the
/// ciphertext fingerprints and the owner's reason but no content.
async fn takedown(
    project: &str,
    owner: &KeyClient<'_>,
    poster: &mut KeyClient<'_>,
    channel: &[u8; 32],
    png: &[u8],
) -> Result<()> {
    let image = poster
        .upload_file(channel, PNG, "image/png", png, None)
        .await?;
    let thumbnail = image.thumbnail.clone().context("missing thumbnail")?;
    let post = poster
        .send_message(
            channel,
            &MessageBody {
                attachments: vec![image.clone()],
                ..Default::default()
            },
        )
        .await?;
    let shared = share(poster, channel).await?;
    let record = owner.message(channel, &post.id).await?;
    let envelope = rusttalk_core::auth::decode_bytes(&record.envelope)?;
    let mut expected = vec![(encode(&post.id), hex(&Sha256::digest(&envelope)))];
    for id in [image.file.id, thumbnail.id, shared] {
        let digest = Sha256::digest(ciphertext(owner, &id).await?);
        expected.push((encode(&id), hex(&digest)));
    }
    let reason = "W679 notice about a reported picture";
    let body = json!({
        "ids": expected.iter().map(|(id, _)| id).collect::<Vec<_>>(),
        "reason": reason,
        "notice": "W679-CASE-7",
    });
    rejected(
        paced(poster.request(Method::POST, "/v1/takedowns").json(&body)).await?,
        StatusCode::FORBIDDEN,
        "owner_required",
    )
    .await?;
    let reply = paced(owner.request(Method::POST, "/v1/takedowns").json(&body)).await?;
    assert_eq!(reply.status(), StatusCode::CREATED);
    let taken: serde_json::Value = reply.json().await?;
    let text = |value: &serde_json::Value| value.as_str().unwrap_or_default().to_owned();
    let mut recorded: Vec<(String, String)> = (taken["items"].as_array().context("items")?)
        .iter()
        .map(|item| (text(&item["id"]), text(&item["sha256"])))
        .collect();
    recorded.sort();
    expected.sort();
    assert_eq!(recorded, expected);
    assert!(taken["reason"] == reason && taken.get("banned").is_none());

    let error = match poster.message(channel, &post.id).await {
        Ok(_) => anyhow::bail!("a removed message is still there"),
        Err(error) => error,
    };
    assert_eq!(
        (error.downcast_ref::<reqwest::Error>()).and_then(reqwest::Error::status),
        Some(StatusCode::NOT_FOUND)
    );
    for id in [image.file.id, thumbnail.id, shared] {
        let id = encode(&id);
        rejected(
            (owner.request(Method::GET, &format!("/v1/files/{id}")))
                .send()
                .await?,
            StatusCode::NOT_FOUND,
            "file_not_found",
        )
        .await?;
        let on_disk = "if [ -e /data/files/\"$1\" ]; then echo present; fi";
        let state = compose(
            project,
            &["exec", "-T", "api", "sh", "-ec", on_disk, "sh", &id],
        )?;
        assert!(state.is_empty(), "a removed file is still on disk");
    }
    let listed = poster.shared_files(channel).await?;
    assert!(!listed.iter().any(|file| file.file.file.id == shared));
    let records: Vec<serde_json::Value> = owner
        .request(Method::GET, "/v1/takedowns")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(records.first(), Some(&taken));
    println!(
        "W679 the owner removed a message, its picture, thumbnail and shared file from the server and disk; the record keeps their ciphertext SHA-256"
    );
    Ok(())
}

/// Upload a small file and share it to the channel's file list.
pub(super) async fn share(client: &KeyClient<'_>, channel: &[u8; 32]) -> Result<[u8; 32]> {
    let (file, _) = prepare_file("shared.bin", "application/octet-stream", b"shared", &[])?;
    client
        .upload_prepared(channel, &file, b"shared", None, None)
        .await?;
    client.share_file(channel, &file).await?;
    Ok(file.file.id)
}

/// The channel's shared-file records as the API lists them, before any app-side checks.
pub(super) async fn shared_records(
    client: &KeyClient<'_>,
    channel: &[u8; 32],
) -> Result<Vec<MessageRecord>> {
    Ok(client
        .request(
            Method::GET,
            &format!("/v1/channels/{}/files", encode(channel)),
        )
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}
