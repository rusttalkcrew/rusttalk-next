use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use livekit::{
    Room, RoomOptions,
    e2ee::{E2eeOptions, EncryptionType, key_provider::KeyProvider},
};
use reqwest::Method;
use rusttalk_core::{
    auth::encode,
    client::{
        Event, KeyClient,
        voice::{LiveFileEvent, RoomEvent, VoiceRoom, media_key},
    },
    identity::random_bytes,
    keys::Scope,
};
use sha2::{Digest, Sha256};
use tokio::time::{sleep, timeout};

const NAME: &str = "rusttalk-private-live-file-7fc1.bin";
const MARKER: &[u8] = b"rusttalk unsealed live file chunk 9f53c7ed";

pub(super) async fn exercise(
    project: &str,
    a: &mut KeyClient<'_>,
    b: &mut KeyClient<'_>,
    c: &KeyClient<'_>,
    room_a: &mut VoiceRoom,
    room_b: &mut VoiceRoom,
    voice: Scope,
) -> Result<()> {
    let channel = encode(&voice.id);
    assert!(room_a.can_publish_data() && room_b.can_publish_data());
    super::set_flags(a, &channel, "admin", 13).await?;
    timeout(super::LIMIT, async {
        while room_b.can_publish_data() {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("B retained data publishing after losing write")?;
    assert!(
        room_b.can_publish(),
        "B lost microphone rights along with data"
    );

    // Watch raw SDK events on the third bot so filtering in VoiceRoom cannot hide a broadcast.
    let request = c.request(Method::POST, &format!("/v1/voice/{channel}/token"));
    let mut url = request
        .try_clone()
        .context("build observer request")?
        .build()?
        .url()
        .clone();
    url.set_path("");
    url.set_scheme("wss")
        .map_err(|()| anyhow::anyhow!("invalid observer origin"))?;
    let response: serde_json::Value = request.send().await?.error_for_status()?.json().await?;
    let token = response["token"]
        .as_str()
        .context("missing observer token")?;
    let key = media_key(
        c.key(voice, 1).context("observer lacks room key")?,
        &c.space().id,
        &voice.id,
        1,
    );
    let mut options = RoomOptions::default();
    options.auto_subscribe = false;
    let provider = KeyProvider::with_shared_key(Default::default(), key.to_vec());
    assert!(provider.set_shared_key(key.to_vec(), 1));
    options.encryption = Some(E2eeOptions {
        key_provider: provider,
        encryption_type: EncryptionType::Gcm,
    });
    let (observer, mut observer_events) = Room::connect(url.as_str(), token, options).await?;
    assert!(
        !observer
            .local_participant()
            .permission()
            .context("missing observer grant")?
            .can_publish_data
    );
    super::roster(a, &voice.id, 3).await?;

    let dir = tempfile::tempdir()?;
    let source = dir.path().join(NAME);
    let target = dir.path().join("download");
    let mut file = vec![0; 20 * 1024 * 1024 + 17];
    random_bytes(&mut file)?;
    for chunk in file.as_chunks_mut::<{ 64 * 1024 }>().0 {
        chunk[..MARKER.len()].copy_from_slice(MARKER);
    }
    std::fs::write(&source, &file)?;
    let record = room_a.offer_live_file(a, &source).await?;
    let offer = timeout(super::LIMIT, async {
        while let Some(event) = b.next_event().await? {
            if let Event::MessageCreated {
                channel: id,
                id: message,
            } = event
                && id == channel
                && message == encode(&record.id)
            {
                return b.message(&voice.id, &record.id).await;
            }
        }
        bail!("event channel closed before live file offer");
    })
    .await??;
    let body = b.open_message(&voice.id, &offer).await?;
    let attachment = body.live.context("missing live attachment")?;
    assert_eq!(attachment.name, NAME);
    assert_eq!(attachment.size, file.len() as u64);
    let mut download = room_b.accept_live_file(b, &offer, &target).await?;
    timeout(super::LIMIT, async {
        while let Some(event) = a.next_event().await? {
            room_a.handle_live_file_event(a, &event)?;
            if matches!(event, Event::LiveFileAccepted { .. }) {
                return Ok(());
            }
        }
        bail!("event channel closed before live file acceptance");
    })
    .await??;

    timeout(Duration::from_secs(30), async {
        loop {
            tokio::select! {
                event = room_b.next_event() => ensure!(event.is_some(), "voice room closed during transfer"),
                event = observer_events.recv() => match event {
                    Some(RoomEvent::ByteStreamOpened { .. }) => bail!("an unaccepting bot received a byte stream"),
                    None => bail!("observer disconnected during transfer"),
                    _ => (),
                },
                update = download.next_event() => match update {
                    Some(LiveFileEvent::Done) => return Ok(()),
                    Some(LiveFileEvent::Progress { bytes, total }) => {
                        assert!(bytes <= total && total == file.len() as u64);
                    }
                    Some(LiveFileEvent::Failed { reason }) => bail!("live file transfer failed: {reason}"),
                    None => bail!("download ended without a terminal event"),
                }
            }
        }
    }).await.context("waiting for the live file transfer")??;
    assert_eq!(Sha256::digest(std::fs::read(target)?), Sha256::digest(file));
    // Room-control events can still be queued after the last file packet was processed.
    while let Ok(event) = observer_events.try_recv() {
        assert!(!matches!(event, RoomEvent::ByteStreamOpened { .. }));
    }
    observer.close().await?;
    super::set_flags(a, &channel, "admin", 15).await?;

    let raw = crate::compose_bytes(
        project,
        &[
            "exec",
            "-T",
            "api",
            "sh",
            "-ec",
            "cat /data/rusttalk.sqlite3 /data/rusttalk.sqlite3-wal",
        ],
    )?;
    let logs = crate::compose_bytes(project, &["logs", "--no-color", "api", "caddy", "livekit"])?;
    for bytes in [raw, logs] {
        for private in [NAME.as_bytes(), MARKER] {
            for needle in [private, encode(private).as_bytes()] {
                assert!(
                    !bytes.windows(needle.len()).any(|window| window == needle),
                    "live file name or content leaked"
                );
            }
        }
    }
    println!(
        "W12 20 MiB live file matched SHA-256, read-only bot accepted, unaccepting bot received no stream, and SQLite/WAL and API/Caddy/LiveKit logs held no filename or plaintext marker"
    );
    Ok(())
}

pub(super) async fn data_only(
    a: &mut KeyClient<'_>,
    b: &mut KeyClient<'_>,
    c: &mut KeyClient<'_>,
    text: Scope,
    voice: Scope,
) -> Result<()> {
    use rusttalk_core::client::voice::DataLiveFiles;

    let dm = a.create_dm(b.identity.certificate.person_id()).await?;
    super::receive_key(b, dm, 1).await?;
    super::receive_key(b, text, 1).await?;
    super::receive_key(c, text, 1).await?;

    let data_token = |scope: Scope| {
        c.request(
            Method::POST,
            &format!(
                "/v1/channels/{}/live-file/token/{}",
                encode(&scope.id),
                encode(&[0; 16])
            ),
        )
    };
    super::rejected(
        data_token(dm).send().await?,
        reqwest::StatusCode::FORBIDDEN,
        "channel_read_denied",
    )
    .await?;

    // A voice channel's chat has its own data room, apart from the call.
    data_token(voice).send().await?.error_for_status()?;

    // A reader without write may join to receive but never to send.
    super::set_flags(a, &encode(&text.id), "member", 1).await?;
    let request = data_token(text);
    let mut url = request
        .try_clone()
        .context("build observer request")?
        .build()?
        .url()
        .clone();
    url.set_path("");
    url.set_scheme("wss")
        .map_err(|()| anyhow::anyhow!("invalid observer origin"))?;
    let pin = a.sync_peer().0.known_server().tls_fingerprint;
    rusttalk_core::pinned_tls::register(&url, pin).map_err(anyhow::Error::from_boxed)?;
    let mut options = RoomOptions::default();
    options.auto_subscribe = false;
    let reply: serde_json::Value = request.send().await?.error_for_status()?.json().await?;
    let (grant_room, _) = Room::connect(
        url.as_str(),
        reply["token"].as_str().context("missing data token")?,
        options.clone(),
    )
    .await?;
    let grant = grant_room
        .local_participant()
        .permission()
        .context("missing data grant")?;
    assert!(!grant.can_publish_data && grant.can_subscribe && !grant.can_publish);
    assert!(a.voice_participants(&voice.id).await?.people.is_empty());
    grant_room.close().await?;
    super::set_flags(a, &encode(&text.id), "member", 15).await?;

    let reply: serde_json::Value = data_token(text)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let (observer, mut observer_events) = Room::connect(
        url.as_str(),
        reply["token"].as_str().context("missing observer token")?,
        options,
    )
    .await?;
    let dir = tempfile::tempdir()?;
    let source = dir.path().join(NAME);
    let mut bytes = vec![0u8; 10 * 1024 * 1024 + 17];
    random_bytes(&mut bytes)?;
    let digest = Sha256::digest(&bytes);
    std::fs::write(&source, bytes)?;

    for scope in [text, dm] {
        let mut sender = DataLiveFiles::new(a, scope.id);
        let receiver = DataLiveFiles::new(b, scope.id);
        let posted = sender.offer_live_file(a, &source).await?;
        let offer = b.message(&scope.id, &posted.id).await?;
        if scope == text {
            assert!(
                observer.remote_participants().is_empty(),
                "offer alone joined the data room"
            );
        }
        let path = dir.path().join(encode(&scope.id));
        let mut download = receiver.accept_live_file(b, &offer, &path).await?;
        timeout(Duration::from_secs(45), async {
            loop {
                tokio::select! {
                    event = a.next_event() => {
                        let event = event?.context("sender event socket closed")?;
                        sender.handle_live_file_event(a, &event)?;
                    },
                    event = observer_events.recv() => match event {
                        Some(RoomEvent::ByteStreamOpened { .. }) => bail!("unaccepting bot received a data stream"),
                        None => bail!("data observer disconnected"),
                        _ => (),
                    },
                    event = download.next_event() => match event {
                        Some(LiveFileEvent::Done) => return Ok(()),
                        Some(LiveFileEvent::Progress { .. }) => (),
                        Some(LiveFileEvent::Failed { reason }) => bail!("data transfer failed: {reason}"),
                        None => bail!("download ended without a terminal event"),
                    },
                }
            }
        }).await.context("waiting for data-only transfer")??;
        assert_eq!(Sha256::digest(std::fs::read(path)?), digest);
        if scope == text {
            timeout(super::LIMIT, async {
                while !observer.remote_participants().is_empty() {
                    if matches!(
                        observer_events.recv().await,
                        Some(RoomEvent::ByteStreamOpened { .. })
                    ) {
                        bail!("unaccepting bot received a data stream");
                    }
                }
                Ok::<_, anyhow::Error>(())
            })
            .await
            .context("accepted devices did not leave the text data room")??;
        }
    }
    while let Ok(event) = observer_events.try_recv() {
        assert!(!matches!(event, RoomEvent::ByteStreamOpened { .. }));
    }
    observer.close().await?;
    println!(
        "B3 10 MiB matched SHA-256 in text and DM without a voice call, with media grants disabled and no stream to the unaccepting bot"
    );
    Ok(())
}
