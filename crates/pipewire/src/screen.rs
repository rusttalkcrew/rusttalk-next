use std::{
    cell::Cell,
    io::{self, Cursor, Read, Write},
    os::fd::OwnedFd,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering::Relaxed},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use ashpd::desktop::{
    PersistMode, ResponseError,
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
use futures_util::StreamExt;
use pipewire::{
    self as pw,
    main_loop::MainLoopRc,
    properties::properties,
    spa::{
        buffer::{ChunkFlags, DataType},
        param::{
            ParamType,
            format::{FormatProperties as Format, MediaSubtype, MediaType},
            video::{VideoFormat, VideoInfoRaw},
        },
        pod::{ChoiceValue, Object, Pod, Property, Value, serialize::PodSerializer},
        sys::SPA_PARAM_BUFFERS_dataType,
        utils::{Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Id, Rectangle, SpaTypes},
    },
    stream::{StreamBox, StreamFlags, StreamState},
};

use crate::linux::{check_parent, iterate};

pub fn run() -> Result<()> {
    let mut args = std::env::args().skip(2);
    let fps: u32 = args.next().context("missing screen frame rate")?.parse()?;
    ensure!(matches!(fps, 5 | 15 | 30 | 60), "invalid screen frame rate");
    let mode = match args.next().as_deref() {
        Some("cursor") => CursorMode::EMBEDDED,
        Some("hidden") => CursorMode::HIDDEN,
        _ => bail!("invalid cursor mode"),
    };
    let token: Option<String> = serde_json::from_reader(io::stdin().take(64 * 1024))?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            tokio::select! {
                result = select(fps, mode, token) => result,
                result = watch_parent() => result,
            }
        })
}

/// The one selection line the client reads before frames.
fn report(selection: &serde_json::Value) -> Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, selection)?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

async fn watch_parent() -> Result<()> {
    loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        check_parent()?;
    }
}

async fn select(fps: u32, cursor: CursorMode, token: Option<String>) -> Result<()> {
    let portal = Screencast::new()
        .await
        .context("connect to desktop ScreenCast portal")?;
    let session = portal.create_session(Default::default()).await?;
    let mut closed = Box::pin(session.receive_closed().await?);
    let sources =
        portal.available_source_types().await? & (SourceType::MONITOR | SourceType::WINDOW);
    ensure!(
        !sources.is_empty(),
        "the portal offers no screens or windows"
    );
    ensure!(
        portal.available_cursor_modes().await?.contains(cursor),
        "the portal does not support the requested cursor mode"
    );
    let mut options = SelectSourcesOptions::default()
        .set_sources(sources)
        .set_cursor_mode(cursor);
    if portal.version() >= 4 {
        options = options
            .set_persist_mode(PersistMode::ExplicitlyRevoked)
            .set_restore_token(token.as_deref());
    }
    portal.select_sources(&session, options).await?.response()?;

    let response = tokio::select! {
        result = portal.start(&session, None, Default::default()) =>
            result.context("open desktop screen picker")?.response(),
        _ = closed.next() => bail!("portal screen selection closed"),
    };
    let selection = match response {
        // The person closed the picker: not an error to show.
        Err(ashpd::Error::Response(ResponseError::Cancelled)) => {
            return report(&serde_json::json!({ "cancelled": true }));
        }
        response => response?,
    };
    let [stream] = selection.streams() else {
        bail!("expected one selected screen source")
    };
    let name = if stream.source_type() == Some(SourceType::WINDOW) {
        "Shared window"
    } else {
        "Shared screen"
    };
    // Hyprland publishes the selected output's connector as the monitor mapping ID.
    let monitor = (stream.source_type() == Some(SourceType::MONITOR))
        .then(|| stream.mapping_id())
        .flatten();
    // Save a replacement even if opening the PipeWire stream subsequently fails.
    report(&serde_json::json!({
        "name": name, "restore_token": selection.restore_token(), "monitor": monitor,
    }))?;
    let fd = portal
        .open_pipe_wire_remote(&session, Default::default())
        .await?;
    let node = stream.pipe_wire_node_id();
    let serial = stream.pipewire_serial();
    let mut worker = tokio::task::spawn_blocking(move || capture(fd, node, serial, fps));
    tokio::select! {
        result = &mut worker => result?,
        _ = closed.next() => {
            // PipeWire polling observes this within one iteration.
            CLOSED.store(true, Relaxed);
            worker.await?
        }
    }
}

static CLOSED: AtomicBool = AtomicBool::new(false);

fn capture(fd: OwnedFd, node: u32, serial: Option<u64>, fps: u32) -> Result<()> {
    pw::init();
    let loop_ = MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&loop_, None)?;
    let core = context
        .connect_fd_rc(fd, None)
        .context("connect to the portal's PipeWire remote")?;
    let failed = Rc::new(Cell::new(false));
    let _error = core
        .add_listener_local()
        .error({
            let failed = failed.clone();
            move |id, _, _, _| {
                if id == pw::core::PW_ID_CORE {
                    failed.set(true);
                }
            }
        })
        .register();
    let mut properties = properties! {
        *pw::keys::MEDIA_TYPE => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        "node.dont-fallback" => "true",
        "node.dont-move" => "true",
        "node.dont-reconnect" => "true",
    };
    // A numeric target.object is a serial. Older portals return only a node ID.
    if let Some(serial) = serial {
        properties.insert(*pw::keys::TARGET_OBJECT, serial.to_string());
    }
    let stream = StreamBox::new(&core, "RustTalk screen", properties)?;

    let params = format(fps)?;
    let buffers = shared_memory()?;
    let error = Rc::new(Cell::new(None));
    let type_error = error.clone();
    let output_error = error.clone();
    let period = Duration::from_secs(1) / fps;
    let mut next = Instant::now();
    let mut output = io::BufWriter::new(io::stdout().lock());
    let _listener = stream
        .add_local_listener_with_user_data(VideoInfoRaw::new())
        .param_changed(move |stream, info, id, param| {
            if id != ParamType::Format.as_raw() {
                return;
            }
            let Some(param) = param else { return };
            if info.parse(param).is_err()
                || !matches!(info.format(), VideoFormat::BGRA | VideoFormat::BGRx)
                || stream.update_params(&mut [pod(&buffers)]).is_err()
            {
                type_error.set(Some("could not negotiate BGRA/BGRx shared memory"));
            }
        })
        .process(move |stream, info| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let now = Instant::now();
            if now < next || CLOSED.load(Relaxed) {
                return;
            }
            if let Some(data) = buffer.datas_mut().first_mut() {
                match write_frame(&mut output, info.size(), data) {
                    Ok(true) => next = now + period,
                    Ok(false) => (),
                    Err(error) => {
                        eprintln!("screen frame: {error:#}");
                        output_error.set(Some("could not deliver a screen frame"));
                    }
                }
            }
        })
        .register()?;
    stream.connect(
        Direction::Input,
        serial.is_none().then_some(node),
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
        &mut [pod(&params)],
    )?;
    let mut connected = false;
    let deadline = Instant::now() + Duration::from_secs(6);
    while !CLOSED.load(Relaxed) {
        ensure!(!failed.get(), "portal PipeWire connection closed");
        if let Some(error) = error.get() {
            bail!("{error}");
        }
        match stream.state() {
            StreamState::Paused | StreamState::Streaming => connected = true,
            StreamState::Unconnected if connected => bail!("selected PipeWire stream disconnected"),
            StreamState::Error(_) => bail!("selected PipeWire stream failed"),
            _ => (),
        }
        ensure!(
            connected || Instant::now() < deadline,
            "timed out connecting to the portal stream"
        );
        iterate(&loop_)?;
    }
    Ok(())
}

fn write_frame(
    output: &mut impl Write,
    Rectangle { width, height }: Rectangle,
    data: &mut pw::spa::buffer::Data,
) -> Result<bool> {
    let chunk = data.chunk();
    if chunk.size() == 0
        || chunk.flags().contains(ChunkFlags::CORRUPTED)
        || width == 0
        || height == 0
    {
        return Ok(false);
    }
    ensure!(
        (1..=8192).contains(&width)
            && (1..=8192).contains(&height)
            && width * height <= 7680 * 4320,
        "PipeWire frame dimensions out of range"
    );
    let offset = chunk.offset() as usize;
    let size = chunk.size() as usize;
    let stride = usize::try_from(chunk.stride()).context("invalid PipeWire frame stride")?;
    let row = (width * 4) as usize;
    let height_rows = height as usize;
    let end = stride
        .checked_mul(height_rows - 1)
        .and_then(|last| last.checked_add(row))
        .context("PipeWire frame stride overflow")?;
    ensure!(stride >= row && end <= size, "truncated PipeWire frame");
    ensure!(
        matches!(data.type_(), DataType::MemFd | DataType::MemPtr),
        "the compositor supplied DMA-BUF instead of shared memory"
    );
    let bytes = data
        .data()
        .and_then(|data| {
            offset
                .checked_add(end)
                .and_then(|end| data.get(offset..end))
        })
        .context("PipeWire frame is outside its shared memory")?;
    output.write_all(&width.to_le_bytes())?;
    output.write_all(&height.to_le_bytes())?;
    for line in bytes.chunks(stride) {
        output.write_all(&line[..row])?;
    }
    output.flush()?;
    Ok(true)
}

fn pod(bytes: &[u8]) -> &Pod {
    Pod::from_bytes(bytes).expect("serialized SPA pod")
}

fn object(kind: SpaTypes, id: ParamType, properties: Vec<Property>) -> Result<Vec<u8>> {
    let value = Value::Object(Object {
        type_: kind.as_raw(),
        id: id.as_raw(),
        properties,
    });
    Ok(PodSerializer::serialize(Cursor::new(Vec::new()), &value)?
        .0
        .into_inner())
}

fn format(fps: u32) -> Result<Vec<u8>> {
    let rect = |width, height| Rectangle { width, height };
    let rate = |num| Fraction { num, denom: 1 };
    let format = |f: VideoFormat| Id(f.as_raw());
    let flags = ChoiceFlags::empty();
    object(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        vec![
            Property::new(
                Format::MediaType.as_raw(),
                Value::Id(Id(MediaType::Video.as_raw())),
            ),
            Property::new(
                Format::MediaSubtype.as_raw(),
                Value::Id(Id(MediaSubtype::Raw.as_raw())),
            ),
            Property::new(
                Format::VideoFormat.as_raw(),
                Value::Choice(ChoiceValue::Id(Choice(
                    flags,
                    ChoiceEnum::Enum {
                        default: format(VideoFormat::BGRx),
                        alternatives: vec![format(VideoFormat::BGRx), format(VideoFormat::BGRA)],
                    },
                ))),
            ),
            Property::new(
                Format::VideoSize.as_raw(),
                Value::Choice(ChoiceValue::Rectangle(Choice(
                    flags,
                    ChoiceEnum::Range {
                        default: rect(1920, 1080),
                        min: rect(1, 1),
                        max: rect(8192, 8192),
                    },
                ))),
            ),
            Property::new(
                Format::VideoFramerate.as_raw(),
                Value::Choice(ChoiceValue::Fraction(Choice(
                    flags,
                    ChoiceEnum::Range {
                        default: rate(fps),
                        min: rate(0),
                        max: rate(360),
                    },
                ))),
            ),
        ],
    )
}

fn shared_memory() -> Result<Vec<u8>> {
    let mask = (1 << DataType::MemFd.as_raw()) | (1 << DataType::MemPtr.as_raw());
    object(
        SpaTypes::ObjectParamBuffers,
        ParamType::Buffers,
        vec![Property::new(
            SPA_PARAM_BUFFERS_dataType,
            Value::Choice(ChoiceValue::Int(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Flags {
                    default: mask,
                    flags: vec![],
                },
            ))),
        )],
    )
}
