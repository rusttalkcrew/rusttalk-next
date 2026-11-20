use std::{io::Cursor, time::Duration};

use anyhow::{Context, Result};
use futures_util::stream;
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use reqwest::{Body, Method, StatusCode};
use rusttalk_core::{
    auth::encode,
    client::{KeyClient, MessageBody, Server, decode_thumbnail},
    keys::random_key,
};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{compose, compose_bytes, receive_key, rejected};

const FILE: &str = "W13 Private File Malachite.bin";
const PNG: &str = "W13 Private Image Carnelian.png";
const MARKER: &[u8] = b"W13 Amethyst plaintext only on device";
const MIB: usize = 1024 * 1024;

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
    rejected(
        a.request(Method::GET, &path).send().await?,
        StatusCode::NOT_FOUND,
        "file_not_found",
    )
    .await?;
    let replacement = a
        .upload_file(&channel.id, FILE, "application/octet-stream", &bytes, None)
        .await?;
    check_storage(project, &encode(&replacement.file.id))?;
    println!(
        "W13 verified encrypted file SHA-256, PNG thumbnail, access, streaming cap, quota races, deletion, moderator deletes limited to the sender's files, expiry and zero plaintext in SQLite, WAL, blobs and logs"
    );
    Ok(())
}
