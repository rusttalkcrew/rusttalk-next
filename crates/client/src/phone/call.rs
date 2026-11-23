//! One phone call: sign in at the SIP provider, call, carry the sound both ways between
//! the RTP socket and the voice room, and hang up. Runs on the network task's runtime;
//! the window steers it through `Control` and hears back through `Event`.
//!
//! Logs never name the number, the account or the password: errors are logged by their
//! outermost context only, which is ours.

use std::{
    collections::VecDeque,
    fmt,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use livekit::webrtc::{audio_frame::AudioFrame, audio_source::AudioSourceOptions};
use rsipstack::{
    EndpointBuilder,
    dialog::{
        authenticate::Credential,
        dialog::{DialogState, DialogStateReceiver},
        dialog_layer::DialogLayer,
        invitation::InviteOption,
        registration::Registration,
    },
    sip::{Host, Method, StatusCode, Transport, Uri},
    transaction::{TransactionReceiver, transaction::Transaction},
    transport::{SipAddr, TransportLayer, udp::UdpConnection},
};
use rusttalk_core::client::voice::NativeAudioSource;
use tokio::{net::UdpSocket, sync::mpsc::UnboundedReceiver, time};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::{
    Account,
    codec::Coder,
    resample::{Down, Up},
    rtp, sdp,
};
use crate::audio::{FRAME, PhoneLine, RATE};

const LOOKUP: Duration = Duration::from_secs(5);
const SIGN_IN: Duration = Duration::from_secs(10);
const SIGN_OUT: Duration = Duration::from_secs(2);
/// How long a sign-in lasts; a call shorter than this needs no renewal. Providers keep
/// the binding for at least the time they grant, and we sign out after the call.
const REGISTER_SECS: u32 = 3600;
/// Packets after a tone before the next one, so two equal keys stay two.
const TONE_GAP: u8 = 2;

pub struct Dial {
    pub account: Account,
    pub password: Zeroizing<String>,
    /// The SIP address to call (see `phone::target`).
    pub target: String,
    /// What the call shows as under the caller.
    pub label: String,
    pub control: UnboundedReceiver<Control>,
}

pub enum Control {
    /// A keypad key, sent as a tone.
    Key(char),
    HangUp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Calling,
    Ringing,
    Talking,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Status(Status),
    /// The call is over: `None` when someone hung up, else why it failed.
    Ended(Option<String>),
}

/// A failure the person is told about, with its text.
#[derive(Debug)]
struct Failed(&'static str);

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Failed {}

fn because(text: &'static str) -> anyhow::Error {
    anyhow::Error::new(Failed(text))
}

/// rsipstack's errors can carry addresses; keep them out of the outermost context.
fn sip(error: rsipstack::Error, what: &'static str) -> anyhow::Error {
    anyhow::Error::msg(error.to_string()).context(what)
}

/// Why the other side turned the call down, for the person.
fn refused(code: u16) -> &'static str {
    match code {
        401 | 403 | 407 => t!("phone.wrong_login"),
        404 | 484 | 604 => t!("phone.no_such_number"),
        486 | 600 => t!("phone.busy"),
        408 | 480 => t!("phone.no_answer"),
        487 => t!("phone.cancelled"),
        603 => t!("phone.declined"),
        _ => t!("phone.failed"),
    }
}

/// Make the call, then tell the window how it ended. `line` and `source` carry the sound.
pub async fn run(
    mut dial: Dial,
    line: PhoneLine,
    source: NativeAudioSource,
    stop: CancellationToken,
    events: impl Fn(Event),
) {
    let ended = match call(&mut dial, &line, &source, &stop, &events).await {
        Ok(()) => None,
        Err(error) => {
            tracing::warn!(error = %error, "phone call failed");
            Some(match error.downcast_ref::<Failed>() {
                Some(Failed(text)) => (*text).to_owned(),
                None => t!("phone.failed").to_owned(),
            })
        }
    };
    tracing::info!("phone call ended");
    events(Event::Ended(ended));
}

/// This computer's address towards `server`: no packet is sent to find it.
fn local_ip(server: SocketAddr) -> Result<IpAddr> {
    let any: SocketAddr = if server.is_ipv4() {
        "0.0.0.0:0".parse()?
    } else {
        "[::]:0".parse()?
    };
    let socket = std::net::UdpSocket::bind(any)?;
    socket.connect(server)?;
    Ok(socket.local_addr()?.ip())
}

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    if rusttalk_core::identity::random_bytes(&mut bytes).is_err() {
        // Only the RTP counters and session ID use these; they need not be secret.
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        for (byte, from) in bytes.iter_mut().zip(time.as_nanos().to_le_bytes()) {
            *byte = from;
        }
    }
    bytes
}

async fn call(
    dial: &mut Dial,
    line: &PhoneLine,
    source: &NativeAudioSource,
    stop: &CancellationToken,
    events: &impl Fn(Event),
) -> Result<()> {
    events(Event::Status(Status::Calling));
    let account = dial.account.clone();
    let server = time::timeout(
        LOOKUP,
        tokio::net::lookup_host((account.host(), account.port())),
    )
    .await
    .map_err(|_| because(t!("phone.unreachable")))?
    .map_err(|_| because(t!("phone.no_server")))?
    .min_by_key(|address| !address.is_ipv4())
    .ok_or_else(|| because(t!("phone.no_server")))?;
    let local = local_ip(server).context("finding the local address failed")?;
    let token = stop.child_token();
    // Closes the SIP socket however the call ends.
    let _close = token.clone().drop_guard();
    let transport = TransportLayer::new(token.clone());
    let udp = UdpConnection::create_connection(
        SocketAddr::new(local, 0),
        None,
        Some(token.child_token()),
    )
    .await
    .map_err(|e| sip(e, "opening the SIP socket failed"))?;
    transport.add_transport(udp.into());
    let endpoint = EndpointBuilder::new()
        .with_cancel_token(token.clone())
        .with_transport_layer(transport)
        .build();
    let mut incoming = endpoint
        .incoming_transactions()
        .map_err(|e| sip(e, "taking SIP requests failed"))?;
    let local_sip =
        (endpoint.get_addrs().first().cloned()).context("the SIP socket has no address")?;
    let layer = Arc::new(DialogLayer::new(endpoint.inner.clone()));
    let credential = Credential {
        username: account.user.trim().to_owned(),
        password: dial.password.to_string(),
        realm: None,
        auth_username: None,
    };
    let registrar = Uri::try_from(format!("sip:{}", account.host()).as_str())
        .map_err(|_| because(t!("phone.no_server")))?;
    let mut registration = Registration::new(endpoint.inner.clone(), Some(credential.clone()));
    let work = async {
        let signed_in = match time::timeout(
            SIGN_IN,
            registration.register(registrar.clone(), Some(REGISTER_SECS)),
        )
        .await
        {
            Err(_) => return Err(because(t!("phone.unreachable"))),
            Ok(Err(error)) => return Err(sip(error, "signing in failed")),
            Ok(Ok(response)) => match response.status_code.code() {
                200..=299 => true,
                401 | 403 | 407 => return Err(because(t!("phone.wrong_login"))),
                code => {
                    // Some providers take calls without a sign-in; the call will tell.
                    tracing::info!(code, "the SIP server refused the sign-in, calling anyway");
                    false
                }
            },
        };
        let public = registration.discovered_public_address();
        let result = place(
            dial,
            line,
            source,
            stop,
            events,
            Setup {
                layer: &layer,
                incoming: &mut incoming,
                server,
                local,
                local_sip: &local_sip,
                public,
                credential,
                account: &account,
            },
        )
        .await;
        if signed_in {
            let _ =
                time::timeout(SIGN_OUT, registration.register(registrar.clone(), Some(0))).await;
        }
        result
    };
    tokio::select! {
        result = work => result,
        _ = endpoint.serve() => Err(anyhow!("the SIP endpoint stopped")),
    }
}

struct Setup<'a> {
    layer: &'a Arc<DialogLayer>,
    incoming: &'a mut TransactionReceiver,
    server: SocketAddr,
    local: IpAddr,
    local_sip: &'a SipAddr,
    /// Our address as the provider sees it, behind a router.
    public: Option<rsipstack::sip::HostWithPort>,
    credential: Credential,
    account: &'a Account,
}

/// Call, wait for the answer, talk, and hang up.
async fn place(
    dial: &mut Dial,
    line: &PhoneLine,
    source: &NativeAudioSource,
    stop: &CancellationToken,
    events: &impl Fn(Event),
    setup: Setup<'_>,
) -> Result<()> {
    let Setup {
        layer,
        incoming,
        server,
        local,
        local_sip,
        public,
        credential,
        account,
    } = setup;
    let rtp = UdpSocket::bind(SocketAddr::new(local, 0))
        .await
        .context("opening the sound socket failed")?;
    let port = rtp.local_addr()?.port();
    let address = match public.as_ref().map(|p| &p.host) {
        Some(Host::IpAddr(ip)) => *ip,
        _ => local,
    };
    let callee =
        Uri::try_from(dial.target.as_str()).map_err(|_| because(t!("phone.bad_number")))?;
    let caller = Uri::try_from(format!("sip:{}@{}", credential.username, account.host()).as_str())
        .map_err(|_| because(t!("phone.wrong_login")))?;
    let contact =
        Registration::create_nat_aware_contact(&credential.username, public, local_sip).uri;
    let name = account.name.trim();
    let option = InviteOption {
        caller_display_name: (!name.is_empty()).then(|| name.to_owned()),
        caller,
        callee,
        destination: Some(SipAddr::new(Transport::Udp, server.into())),
        content_type: Some("application/sdp".into()),
        offer: Some(sdp::offer(address, port, u64::from_le_bytes(random())).into_bytes()),
        contact,
        credential: Some(credential),
        ..Default::default()
    };
    let (states_sender, mut states) = layer.new_dialog_state_channel();
    let (dialog, mut invite) = layer
        .do_invite_async(option, states_sender)
        .map_err(|e| sip(e, "calling failed"))?;
    let (id, body) = loop {
        tokio::select! {
            _ = stop.cancelled() => {
                let _ = dialog.hangup().await;
                return Ok(());
            }
            control = dial.control.recv() => match control {
                Some(Control::Key(_)) => {}
                Some(Control::HangUp) | None => {
                    let _ = dialog.hangup().await;
                    return Ok(());
                }
            },
            Some(state) = states.recv() => {
                if let DialogState::Early(..) = state {
                    events(Event::Status(Status::Ringing));
                }
            }
            Some(tx) = incoming.recv() => respond(layer, tx).await,
            done = &mut invite => {
                let (id, response) = done
                    .context("the call stopped")?
                    .map_err(|e| sip(e, "calling failed"))?;
                let response = response.context("the call got no answer")?;
                let code = response.status_code.code();
                if !(200..300).contains(&code) {
                    tracing::info!(code, "the call was turned down");
                    return Err(because(refused(code)));
                }
                break (id, response.body);
            }
        }
    };
    let Some(media) = sdp::answer(&String::from_utf8_lossy(&body)) else {
        let _ = dialog.hangup().await;
        layer.remove_dialog(&id);
        return Err(because(t!("phone.no_sound")));
    };
    tracing::info!(codec = ?media.codec, tones = media.tones.is_some(), "phone call answered");
    events(Event::Status(Status::Talking));
    let hung_up = talk(
        dial,
        line,
        source,
        stop,
        &rtp,
        &media,
        &mut states,
        incoming,
        layer,
    )
    .await;
    if hung_up {
        let _ = dialog.hangup().await;
    }
    layer.remove_dialog(&id);
    Ok(())
}

/// Requests from the provider: those of our call go to it, others are turned down.
async fn respond(layer: &DialogLayer, mut tx: Transaction) {
    if let Some(mut dialog) = layer.match_dialog(&tx) {
        if let Err(error) = dialog.handle(&mut tx).await {
            tracing::debug!(error = %error, "handling a SIP request failed");
        }
        return;
    }
    let status = match tx.original.method {
        Method::Options => StatusCode::OK,
        // Calls to us are not taken (yet).
        Method::Invite => StatusCode::BusyHere,
        _ => StatusCode::CallTransactionDoesNotExist,
    };
    let _ = tx.reply(status).await;
}

/// The source the call's sound is published from: mono 48 kHz like the frames
/// `send_frames` hands it (app shares are stereo), without queue or processing; the
/// phone line is cleaned up already, and the room's voices are in it on purpose.
pub fn source() -> NativeAudioSource {
    let off = AudioSourceOptions {
        echo_cancellation: false,
        noise_suppression: false,
        auto_gain_control: false,
    };
    NativeAudioSource::new(off, RATE, 1, 0)
}

/// The source copies 10 ms frames synchronously when it has no queue. A frame it
/// refuses is logged once per call.
async fn send_frames(source: &NativeAudioSource, samples: &[i16], warned: &mut bool) {
    for frame in samples.as_chunks::<FRAME>().0 {
        let frame = AudioFrame {
            data: frame.as_slice().into(),
            sample_rate: RATE,
            num_channels: 1,
            samples_per_channel: FRAME as u32,
        };
        if let Err(error) = source.capture_frame(&frame).await
            && !std::mem::replace(warned, true)
        {
            tracing::warn!(%error, "sending phone audio to the call failed");
        }
    }
}

/// Carry the sound until someone hangs up. True when it was us.
#[allow(clippy::too_many_arguments)]
async fn talk(
    dial: &mut Dial,
    line: &PhoneLine,
    source: &NativeAudioSource,
    stop: &CancellationToken,
    rtp: &UdpSocket,
    media: &sdp::Answer,
    states: &mut DialogStateReceiver,
    incoming: &mut TransactionReceiver,
    layer: &DialogLayer,
) -> bool {
    let [s0, s1, s2, s3, q0, q1, t0, t1, t2, t3] = random::<10>();
    let mut sender = rtp::Sender::new(
        u32::from_le_bytes([s0, s1, s2, s3]),
        u16::from_le_bytes([q0, q1]),
        u32::from_le_bytes([t0, t1, t2, t3]),
    );
    let mut to = media.to;
    let mut coder = Coder::new(media.codec);
    let mut down = Down::new(media.codec.ratio());
    let mut up = Up::new(media.codec.ratio());
    // 20 ms of the room, one packet's worth.
    let mut room = vec![0i16; RATE as usize / 50];
    let mut keys: VecDeque<char> = VecDeque::new();
    let mut tone: Option<(rtp::Tone, u32)> = None;
    let mut gap = 0u8;
    let mut buffer = [0u8; 1500];
    let mut strangers = false;
    let mut warned = false;
    // The newest sound packet's sequence number.
    let mut last: Option<u16> = None;
    let mut tick = time::interval(Duration::from_millis(20));
    loop {
        tokio::select! {
            _ = stop.cancelled() => return true,
            control = dial.control.recv() => match control {
                Some(Control::Key(key)) => keys.push_back(key),
                Some(Control::HangUp) | None => return true,
            },
            Some(state) = states.recv() => {
                if let DialogState::Terminated(..) = state {
                    return false;
                }
            }
            Some(tx) = incoming.recv() => respond(layer, tx).await,
            _ = tick.tick() => {
                line.take(&mut room);
                if media.tones.is_none() {
                    // The other side takes no keypad tones.
                    keys.clear();
                }
                while tone.is_none() && gap == 0 && let Some(key) = keys.pop_front() {
                    tone = rtp::Tone::new(key).map(|next| (next, sender.timestamp));
                }
                let packet = match (&mut tone, media.tones) {
                    (Some((current, start)), Some(pt)) => {
                        let packet = sender.tone(pt, *start, current);
                        current.advance();
                        sender.skip();
                        if current.done() {
                            tone = None;
                            gap = TONE_GAP;
                        }
                        packet
                    }
                    _ => {
                        gap = gap.saturating_sub(1);
                        let sound = coder.encode(&down.run(&room));
                        sender.sound(media.codec.payload_type(), &sound)
                    }
                };
                if let Err(error) = rtp.send_to(&packet, to).await {
                    tracing::debug!(%error, "sending phone audio failed");
                }
            }
            received = rtp.recv_from(&mut buffer) => {
                let Ok((len, from)) = received else { continue };
                // Only the provider's media server may speak into the call.
                if from.ip() != media.to.ip() {
                    if !strangers {
                        strangers = true;
                        tracing::info!("ignoring phone audio from another address");
                    }
                    continue;
                }
                // Behind its own router the server answers from another port: follow it.
                to = from;
                let Some(packet) = rtp::parse(&buffer[..len]) else { continue };
                // A late or repeated packet would only click: its moment has passed.
                if last.is_some_and(|last| packet.sequence.wrapping_sub(last).cast_signed() <= 0) {
                    continue;
                }
                last = Some(packet.sequence);
                let Some(samples) = coder.decode(packet.payload_type, packet.payload) else { continue };
                let wide = up.run(&samples);
                line.play(&wide);
                send_frames(source, &wide, &mut warned).await;
            }
        }
    }
}
