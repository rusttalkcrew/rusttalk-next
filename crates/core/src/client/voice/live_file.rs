mod crypto;

use std::{
    collections::{HashMap, hash_map::Entry},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use livekit::{
    Room, RoomDataStreamOptions, RoomEvent, RoomOptions,
    data_stream::api::{ByteStreamReader, StreamByteOptions, StreamReader, StreamWriter},
};
use reqwest::{Method, RequestBuilder};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::timeout,
};
use zeroize::Zeroizing;

use crate::{
    auth::{KnownServer, decode, encode, live_file_identity, voice_identity},
    identity::random_bytes,
    keys::{Event, random_key},
    messages::{
        LIVE_CHUNK_SIZE, LiveAttachment, LiveFileAcceptance, MAX_LIVE_FILE_SIZE, MessageBody,
        MessageRecord, fit_file_name,
    },
};

use super::{ConnectionState, KeyClient, ParticipantIdentity, Token, VoiceRoom, signaling_url};
use crypto::{FileCipher, TAG_SIZE, wire_size};

const TOPIC: &str = "rusttalk.file";
const IDLE: Duration = Duration::from_secs(30);
const OFFER_LIFETIME: Duration = Duration::from_secs(10 * 60);
const TRANSFER_ATTRIBUTE: &str = "transfer";
const NONCE_ATTRIBUTE: &str = "acceptance";

pub(super) const RECEIVE_CAP: usize = wire_size(MAX_LIVE_FILE_SIZE) as usize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveFileEvent {
    Progress { bytes: u64, total: u64 },
    Done,
    Failed { reason: String },
}

/// An accepted download. Dropping the handle cancels it.
pub struct LiveDownload {
    events: watch::Receiver<LiveFileEvent>,
    /// Dropping this sender stops the download task.
    _cancel: oneshot::Sender<()>,
}

impl LiveDownload {
    /// Wait for the latest progress or terminal event. Intermediate updates may coalesce.
    pub async fn next_event(&mut self) -> Option<LiveFileEvent> {
        self.events.changed().await.ok()?;
        Some(self.events.borrow_and_update().clone())
    }
}

struct Upload {
    file: LiveAttachment,
    path: PathBuf,
    created: Instant,
    recipients: HashMap<[u8; 32], ([u8; 16], JoinHandle<()>)>,
}

impl Drop for Upload {
    fn drop(&mut self) {
        for (_, task) in self.recipients.values() {
            task.abort();
        }
    }
}

struct Accepted {
    sender: ParticipantIdentity,
    wire_size: u64,
    reader: oneshot::Sender<ByteStreamReader>,
}

#[derive(Default)]
pub(super) struct LiveFiles {
    offers: HashMap<[u8; 32], Upload>,
    accepted: HashMap<([u8; 32], [u8; 16]), Accepted>,
    disconnect: watch::Sender<()>,
}

impl LiveFiles {
    pub(super) fn disconnect(&mut self) {
        self.disconnect.send_replace(());
        self.offers.clear();
        self.accepted.clear();
    }

    pub(super) fn receive(&mut self, sender: &ParticipantIdentity, reader: ByteStreamReader) {
        let result = self.match_stream(sender, reader);
        if let Err(reason) = result {
            tracing::warn!(reason, "dropped live file stream");
        }
    }

    fn match_stream(
        &mut self,
        sender: &ParticipantIdentity,
        reader: ByteStreamReader,
    ) -> Result<(), &'static str> {
        let info = reader.info();
        if info.topic != TOPIC {
            return Err("unknown byte stream topic");
        }
        let attributes = info.attributes();
        let transfer = attributes
            .get(TRANSFER_ATTRIBUTE)
            .and_then(|id| decode(id).ok());
        let nonce = attributes
            .get(NONCE_ATTRIBUTE)
            .and_then(|id| decode(id).ok());
        let (Some(transfer), Some(nonce)) = (transfer, nonce) else {
            return Err("invalid file stream attributes");
        };
        self.accepted
            .retain(|_, accepted| !accepted.reader.is_closed());
        let Entry::Occupied(slot) = self.accepted.entry((transfer, nonce)) else {
            return Err("file stream has no matching acceptance");
        };
        let accepted = slot.get();
        if &accepted.sender != sender {
            return Err("file stream sender differs from verified offer");
        }
        if info.total_length != Some(accepted.wire_size) {
            return Err("file stream length differs from verified offer");
        }
        slot.remove()
            .reader
            .send(reader)
            .map_err(|_| "download closed")
    }
}

impl VoiceRoom {
    /// Whether the active LiveKit grant allows sending live files.
    pub fn can_publish_data(&self) -> bool {
        can_publish_data(&self.room)
    }

    /// Post a signed and encrypted offer for a local file. Pump room and API events while it is open.
    pub async fn offer_live_file(
        &mut self,
        client: &mut KeyClient<'_>,
        path: &Path,
    ) -> Result<MessageRecord> {
        self.check_file_client(client)?;
        ensure!(self.can_publish_data(), "live file publishing is denied");
        self.files.offer(client, &self.channel, path).await
    }

    /// Verify the offer's current sender, accept it, and download to a user-chosen path.
    pub async fn accept_live_file(
        &mut self,
        client: &mut KeyClient<'_>,
        record: &MessageRecord,
        save_path: &Path,
    ) -> Result<LiveDownload> {
        self.check_file_client(client)?;
        let (acceptance, _) =
            prepare_accept(client, &self.channel, record, save_path, false).await?;
        let (reader_tx, reader_rx) = oneshot::channel();
        self.files.arm(&acceptance, reader_tx);
        Ok(start_download(
            acceptance,
            save_path,
            async move { reader_rx.await.context("file stream no longer accepted") },
            Some(self.files.disconnect.subscribe()),
            None,
        ))
    }

    /// Stop every upload for this offer and refuse further acceptances.
    pub fn cancel_live_offer(&mut self, transfer: &[u8; 32]) {
        self.files.offers.remove(transfer);
    }

    /// Dispatch a KeyClient API event. Verified acceptances start one targeted stream per device.
    pub fn handle_live_file_event(&mut self, client: &KeyClient<'_>, event: &Event) -> Result<()> {
        if matches!(event, Event::LiveFileAccepted { channel, data: false, .. } if *channel == encode(&self.channel))
        {
            self.check_file_client(client)?;
            self.files
                .handle(client, &self.channel, event, Some(&self.room))?;
        }
        Ok(())
    }

    fn check_file_client(&self, client: &KeyClient<'_>) -> Result<()> {
        ensure!(
            &self.server == client.server.known_server()
                && self.identity().as_str()
                    == voice_identity(
                        &self.server.server_id,
                        client.identity.certificate.signing_key().as_bytes(),
                    ),
            "key client belongs to a different voice session"
        );
        ensure!(
            self.connection_state() == ConnectionState::Connected,
            "voice room is disconnected"
        );
        Ok(())
    }
}

/// Local offers for one channel. Dispatch API events to start uploads.
pub struct DataLiveFiles {
    server: [u8; 32],
    device: [u8; 32],
    channel: [u8; 32],
    files: LiveFiles,
}

impl DataLiveFiles {
    pub fn new(client: &KeyClient<'_>, channel: [u8; 32]) -> Self {
        Self {
            server: client.space().id,
            device: client.identity.certificate.signing_key().to_bytes(),
            channel,
            files: LiveFiles::default(),
        }
    }

    fn check_client(&self, client: &KeyClient<'_>) -> Result<()> {
        ensure!(
            self.server == client.space().id
                && &self.device == client.identity.certificate.signing_key().as_bytes(),
            "key client belongs to a different live file session"
        );
        Ok(())
    }

    /// Post an encrypted offer. The sender only connects after a verified acceptance.
    pub async fn offer_live_file(
        &mut self,
        client: &mut KeyClient<'_>,
        path: &Path,
    ) -> Result<MessageRecord> {
        crate::route::allow_realtime_media("Live file transfers")?;
        self.check_client(client)?;
        self.files.offer(client, &self.channel, path).await
    }

    /// Join the data room, send the signed acceptance and download to a chosen path.
    /// The download task leaves the room on completion, failure or cancellation.
    pub async fn accept_live_file(
        &self,
        client: &mut KeyClient<'_>,
        record: &MessageRecord,
        save_path: &Path,
    ) -> Result<LiveDownload> {
        self.check_client(client)?;
        let (acceptance, room) =
            prepare_accept(client, &self.channel, record, save_path, true).await?;
        let (room, mut room_events) = room.context("missing data connection")?;
        let mut files = LiveFiles::default();
        let (reader_tx, mut reader_rx) = oneshot::channel();
        files.arm(&acceptance, reader_tx);
        Ok(start_download(
            acceptance,
            save_path,
            async move {
                loop {
                    tokio::select! {
                        result = &mut reader_rx => return result.context("file stream no longer accepted"),
                        event = room_events.recv() => match event {
                            Some(RoomEvent::ByteStreamOpened { reader, participant_identity, .. }) => {
                                if let Some(reader) = reader.take() {
                                    files.receive(&participant_identity, reader);
                                }
                            }
                            Some(RoomEvent::Disconnected { .. }) | None => anyhow::bail!("data room disconnected"),
                            _ => (),
                        }
                    }
                }
            },
            None,
            Some(room),
        ))
    }

    pub fn cancel_live_offer(&mut self, transfer: &[u8; 32]) {
        self.files.offers.remove(transfer);
    }

    pub fn handle_live_file_event(&mut self, client: &KeyClient<'_>, event: &Event) -> Result<()> {
        self.check_client(client)?;
        self.files.handle(client, &self.channel, event, None)
    }
}

impl LiveFiles {
    async fn offer(
        &mut self,
        client: &mut KeyClient<'_>,
        channel: &[u8; 32],
        path: &Path,
    ) -> Result<MessageRecord> {
        let metadata = tokio::fs::metadata(path)
            .await
            .context("inspect offered file")?;
        ensure!(metadata.is_file(), "offer requires a regular file");
        ensure!(
            metadata.len() <= MAX_LIVE_FILE_SIZE,
            "live file exceeds 2 GiB"
        );
        let file = LiveAttachment {
            id: *random_key()?,
            name: fit_file_name(
                &path
                    .file_name()
                    .context("live file requires a name")?
                    .to_string_lossy(),
            ),
            mime_type: mime_guess::from_path(path)
                .first_or_octet_stream()
                .essence_str()
                .to_owned(),
            size: metadata.len(),
            chunk_size: LIVE_CHUNK_SIZE as u32,
            key: random_key()?,
        };
        let record = client
            .send_message(
                channel,
                &MessageBody {
                    live: Some(file.clone()),
                    ..Default::default()
                },
            )
            .await
            .context("post live file offer")?;
        self.offers.retain(|_, offer| {
            offer.created.elapsed() < OFFER_LIFETIME
                || offer
                    .recipients
                    .values()
                    .any(|(_, task)| !task.is_finished())
        });
        self.offers.insert(
            file.id,
            Upload {
                file,
                path: path.to_owned(),
                created: Instant::now(),
                recipients: HashMap::new(),
            },
        );
        Ok(record)
    }

    fn arm(&mut self, acceptance: &DownloadSetup, reader: oneshot::Sender<ByteStreamReader>) {
        self.accepted
            .retain(|_, accepted| !accepted.reader.is_closed());
        self.accepted.insert(
            (acceptance.file.id, acceptance.nonce),
            Accepted {
                sender: acceptance.sender.clone(),
                wire_size: wire_size(acceptance.file.size),
                reader,
            },
        );
    }

    fn handle(
        &mut self,
        client: &KeyClient<'_>,
        channel_id: &[u8; 32],
        event: &Event,
        room: Option<&Arc<Room>>,
    ) -> Result<()> {
        let Event::LiveFileAccepted {
            channel,
            device,
            acceptance,
            data,
        } = event
        else {
            return Ok(());
        };
        if *channel != encode(channel_id) || *data != room.is_none() {
            return Ok(());
        }
        let local = client.identity.certificate.signing_key();
        ensure!(
            acceptance.recipient == encode(local.as_bytes()),
            "file acceptance has wrong recipient"
        );
        let certificate = client
            .device(device)
            .context("file acceptance has unknown device")?;
        let peer = certificate.signing_key();
        acceptance
            .verify(&client.space().id, channel_id, peer)
            .context("invalid signed file acceptance")?;
        let transfer = decode(&acceptance.transfer)?;
        let nonce = decode(&acceptance.nonce)?;
        // A cancelled offer or another session of this device may have received the notice.
        let Some(offer) = self.offers.get_mut(&transfer) else {
            return Ok(());
        };
        // A verified cancellation of a transfer already running for that device and
        // nonce always goes through, also while DMs here are paused.
        if acceptance.cancel {
            if let Some((accepted_nonce, task)) = offer.recipients.get(peer.as_bytes())
                && *accepted_nonce == nonce
            {
                task.abort();
            }
            return Ok(());
        }
        // A new transfer in a DM: only the pinned other person, and not while paused.
        ensure!(
            client.dm_peer_is(channel_id, certificate.person_id()),
            "file acceptance from someone other than the DM's other person"
        );
        ensure!(
            offer.created.elapsed() < OFFER_LIFETIME,
            "live file offer expired"
        );
        if let Entry::Vacant(entry) = offer.recipients.entry(peer.to_bytes()) {
            let server = client.server.known_server().clone();
            let destination = ParticipantIdentity::from(if *data {
                live_file_identity(&server.server_id, peer.as_bytes(), &nonce)
            } else {
                voice_identity(&server.server_id, peer.as_bytes())
            });
            let request = data_request(client, channel_id, &nonce);
            let room = room.cloned();
            let file = offer.file.clone();
            let path = offer.path.clone();
            let prefix = crypto::nonce_prefix(peer.as_bytes(), &nonce);
            if let Some(room) = &room {
                ensure!(can_publish_data(room), "live file publishing is denied");
            }
            entry.insert((
                nonce,
                tokio::spawn(async move {
                    let result = async {
                        if let Some(room) = room {
                            return upload(room, destination, file, prefix, nonce, &path).await;
                        }
                        let (room, mut events) = connect_data(&server, request).await?;
                        upload(
                            room.0.clone(),
                            destination.clone(),
                            file,
                            prefix,
                            nonce,
                            &path,
                        )
                        .await?;
                        // Closing as soon as the SDK queues the trailer can drop data still in flight.
                        timeout(IDLE, async {
                            while let Some(event) = events.recv().await {
                                match event {
                                    RoomEvent::ParticipantDisconnected(peer)
                                        if peer.identity() == destination =>
                                    {
                                        break;
                                    }
                                    RoomEvent::Disconnected { .. } => break,
                                    _ => (),
                                }
                            }
                        })
                        .await
                        .context("receiver did not leave the data room")?;
                        Ok(())
                    }
                    .await;
                    if let Err(error) = result {
                        tracing::warn!(reason = %error, "live file upload stopped");
                    }
                }),
            ));
        }
        Ok(())
    }
}

struct DataConnection(Arc<Room>);

impl Drop for DataConnection {
    fn drop(&mut self) {
        let room = self.0.clone();
        tokio::spawn(async move {
            if let Err(error) = room.close().await {
                tracing::warn!(reason = %error, "could not close live file data room");
            }
        });
    }
}

fn data_request(client: &KeyClient<'_>, channel: &[u8; 32], nonce: &[u8; 16]) -> RequestBuilder {
    client.request(
        Method::POST,
        &format!(
            "/v1/channels/{}/live-file/token/{}",
            encode(channel),
            encode(nonce)
        ),
    )
}

async fn connect_data(server: &KnownServer, request: RequestBuilder) -> Result<ConnectedData> {
    crate::route::allow_realtime_media("Live file transfers")?;
    let token: Token = request
        .timeout(IDLE)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let mut options = RoomOptions::default();
    options.auto_subscribe = false;
    options.join_retries = 0;
    options.data_stream =
        RoomDataStreamOptions::default().with_max_payload_byte_length(RECEIVE_CAP);
    let url = signaling_url(server)?;
    let (room, events) = Room::connect(url.as_str(), &token.token, options)
        .await
        .context("connect to pinned LiveKit data room")?;
    Ok((DataConnection(Arc::new(room)), events))
}

fn can_publish_data(room: &Room) -> bool {
    room.local_participant()
        .permission()
        .is_some_and(|p| p.can_publish_data)
}

struct DownloadSetup {
    sender: ParticipantIdentity,
    file: LiveAttachment,
    nonce: [u8; 16],
    prefix: [u8; 16],
    cancel_post: RequestBuilder,
}

type ConnectedData = (DataConnection, mpsc::UnboundedReceiver<RoomEvent>);

async fn prepare_accept(
    client: &mut KeyClient<'_>,
    channel: &[u8; 32],
    record: &MessageRecord,
    save_path: &Path,
    data: bool,
) -> Result<(DownloadSetup, Option<ConnectedData>)> {
    crate::route::allow_realtime_media("Live file transfers")?;
    let body = client
        .open_message(channel, record)
        .await
        .context("verify live file offer")?;
    let file = body.live.context("message has no live file offer")?;
    ensure!(
        body.edit_of.is_none(),
        "live file offer must be an original message"
    );
    // In a DM only from one of the two, and not while this server's DMs are paused.
    ensure!(
        client.may_accept_offer(channel, &record.person),
        "live file offer from outside the DM, or DMs here are paused"
    );
    let sender = client
        .device(&encode(&record.sender_device))
        .filter(|cert| cert.person_id() == &record.person)
        .context("live file sender has no current verified device")?;
    let mut nonce = [0; 16];
    random_bytes(&mut nonce)?;
    let sender = ParticipantIdentity::from(if data {
        live_file_identity(&client.space().id, sender.signing_key().as_bytes(), &nonce)
    } else {
        voice_identity(&client.space().id, sender.signing_key().as_bytes())
    });
    ensure!(
        !tokio::fs::try_exists(save_path)
            .await
            .context("check download destination")?,
        "download destination already exists"
    );
    let prefix = crypto::nonce_prefix(client.identity.certificate.signing_key().as_bytes(), &nonce);
    let mut request = LiveFileAcceptance::signed(
        client.identity,
        &client.space().id,
        channel,
        &record.sender_device,
        &file.id,
        &nonce,
    );
    let room = if data {
        Some(
            connect_data(
                client.server.known_server(),
                data_request(client, channel, &nonce),
            )
            .await?,
        )
    } else {
        None
    };
    let route = if data { "channels" } else { "voice" };
    let endpoint = format!("/v1/{route}/{}/live-file", encode(channel));
    let post = client.request(Method::POST, &endpoint);
    submit(
        post.try_clone()
            .context("build file acceptance")?
            .json(&request),
    )
    .await?;
    request.cancel = true;
    request.sign(client.identity, &client.space().id, channel);
    let cancel_post = post.json(&request);
    Ok((
        DownloadSetup {
            sender,
            file,
            nonce,
            prefix,
            cancel_post,
        },
        room,
    ))
}

fn start_download(
    setup: DownloadSetup,
    save_path: &Path,
    reader: impl Future<Output = Result<ByteStreamReader>> + Send + 'static,
    disconnect: Option<watch::Receiver<()>>,
    room: Option<DataConnection>,
) -> LiveDownload {
    let DownloadSetup {
        file,
        prefix,
        cancel_post,
        ..
    } = setup;
    let (events, mut updates) = watch::channel(LiveFileEvent::Progress {
        bytes: 0,
        total: file.size,
    });
    updates.mark_changed();
    let (cancel, mut cancelled) = oneshot::channel();
    let path = save_path.to_owned();
    tokio::spawn(async move {
        let result = tokio::select! {
            biased;
            _ = &mut cancelled => Err(anyhow::anyhow!("download cancelled")),
            _ = async move {
                match disconnect {
                    Some(mut disconnect) => { let _ = disconnect.changed().await; }
                    None => std::future::pending().await,
                }
            } => Err(anyhow::anyhow!("voice room disconnected")),
            result = async {
                let reader = timeout(IDLE, reader).await.context("file stream did not arrive")??;
                download(file, prefix, reader, &path, &events).await
            } => result,
        };
        drop(room);
        match result {
            Ok(()) => {
                events.send_replace(LiveFileEvent::Done);
            }
            Err(error) => {
                tracing::warn!(reason = %error, "live file download stopped");
                events.send_replace(LiveFileEvent::Failed {
                    reason: error.to_string(),
                });
                drop(events);
                if let Err(error) = submit(cancel_post).await {
                    tracing::warn!(reason = %error, "could not send live file cancellation");
                }
            }
        }
    });
    LiveDownload {
        events: updates,
        _cancel: cancel,
    }
}

async fn submit(request: RequestBuilder) -> Result<()> {
    request
        .timeout(IDLE)
        .send()
        .await
        .context("relay live file response")?
        .error_for_status()
        .context("live file response rejected")?;
    Ok(())
}

async fn upload(
    room: Arc<livekit::Room>,
    destination: ParticipantIdentity,
    file: LiveAttachment,
    prefix: [u8; 16],
    nonce: [u8; 16],
    path: &Path,
) -> Result<()> {
    let mut source = File::open(path).await.context("open offered file")?;
    let metadata = source.metadata().await.context("inspect offered file")?;
    ensure!(
        metadata.is_file() && metadata.len() == file.size,
        "offered file changed size or is no longer regular"
    );
    let options = StreamByteOptions::new_with_topic(TOPIC)
        .with_destination_identity(destination)
        .with_total_length(wire_size(file.size))
        .with_attributes(HashMap::from([
            (TRANSFER_ATTRIBUTE.to_owned(), encode(&file.id)),
            (NONCE_ATTRIBUTE.to_owned(), encode(&nonce)),
        ]));
    let participant = room.local_participant();
    let writer = timeout(IDLE, participant.stream_bytes(options))
        .await
        .context("opening file stream timed out")?
        .context("open file stream")?;
    let result: Result<()> = async {
        let mut cipher = FileCipher::new(file, prefix)?;
        let mut data = Zeroizing::new(vec![0; LIVE_CHUNK_SIZE]);
        while let Some((len, final_chunk)) = cipher.next_chunk() {
            source
                .read_exact(&mut data[..len])
                .await
                .context("read offered file")?;
            if final_chunk {
                ensure!(
                    source
                        .read(&mut [0])
                        .await
                        .context("check offered file length")?
                        == 0,
                    "offered file grew during transfer"
                );
            }
            let chunk = cipher.seal(&data[..len])?;
            timeout(IDLE, writer.write(&chunk))
                .await
                .context("writing file stream timed out")?
                .context("write file stream")?;
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        let _ = timeout(IDLE, writer.close_with_reason("upload failed")).await;
        return Err(error);
    }
    timeout(IDLE, writer.close())
        .await
        .context("closing file stream timed out")?
        .context("close file stream")
}

async fn download(
    file: LiveAttachment,
    prefix: [u8; 16],
    mut reader: ByteStreamReader,
    path: &Path,
    events: &watch::Sender<LiveFileEvent>,
) -> Result<()> {
    let total = file.size;
    let mut cipher = FileCipher::new(file, prefix)?;
    let parent = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let (temp, temporary_path) = tempfile::NamedTempFile::new_in(parent)
        .context("create partial file")?
        .into_parts();
    let mut target = File::from_std(temp);
    let mut bytes = 0;
    let mut chunk = Vec::with_capacity(LIVE_CHUNK_SIZE + TAG_SIZE);
    while let Some(packet) = timeout(IDLE, reader.next())
        .await
        .context("file stream stalled")?
    {
        let packet = packet.context("read file stream")?;
        let mut rest = packet.as_ref();
        while !rest.is_empty() {
            let (len, _) = cipher.next_chunk().context("excess file stream bytes")?;
            let frame_len = len + TAG_SIZE;
            let (part, tail) = rest.split_at((frame_len - chunk.len()).min(rest.len()));
            chunk.extend_from_slice(part);
            rest = tail;
            if chunk.len() == frame_len {
                let opened = cipher.open(&chunk)?;
                target
                    .write_all(&opened)
                    .await
                    .context("write partial file")?;
                bytes += opened.len() as u64;
                events.send_replace(LiveFileEvent::Progress { bytes, total });
                chunk.clear();
            }
        }
    }
    cipher.finish()?;
    target.flush().await.context("flush partial file")?;
    drop(target);
    temporary_path
        .persist_noclobber(path)
        .map_err(|failure| failure.error)
        .context("save verified file")?;
    Ok(())
}
