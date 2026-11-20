use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    io::{self, Cursor, Write},
    os::unix::process::parent_id,
    rc::Rc,
    sync::OnceLock,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use pipewire::{
    self as pw,
    core::{Core, CoreRc},
    loop_::Timeout,
    main_loop::{MainLoop, MainLoopRc},
    node::{Node, NodeListener, NodeState},
    properties::properties,
    spa::{
        param::{
            ParamType,
            audio::{AudioFormat, AudioInfoRaw},
        },
        pod::{Object, Pod, Value, serialize::PodSerializer},
        utils::{Direction, SpaTypes},
    },
    stream::{StreamBox, StreamFlags, StreamState},
    types::ObjectType,
};
use serde::Serialize;

#[derive(Serialize)]
struct App {
    id: String,
    name: String,
}

const STARTUP: Duration = Duration::from_secs(3);
static PARENT: OnceLock<u32> = OnceLock::new();

pub fn run() -> Result<()> {
    PARENT.get_or_init(parent_id);
    pw::init();
    let loop_ = MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&loop_, None)?;
    let core = context
        .connect_rc(None)
        .context("connect to the local PipeWire daemon")?;
    let arg = std::env::args()
        .nth(1)
        .context("expected list or a stream ID")?;
    if arg == "list" {
        serde_json::to_writer(io::stdout().lock(), &list_apps(&loop_, &core)?)?;
        return Ok(());
    }
    let (_, serial) = arg.split_once(':').context("invalid PipeWire stream ID")?;
    let serial: u64 = serial.parse().context("invalid PipeWire object serial")?;
    ensure!(
        list_apps(&loop_, &core)?.iter().any(|app| app.id == arg),
        "the selected playback stream is no longer running"
    );
    capture(&loop_, &core, serial)
}

fn list_apps(loop_: &MainLoop, core: &CoreRc) -> Result<Vec<App>> {
    type Nodes = HashMap<u32, (NodeListener, Node, App, Rc<Cell<bool>>)>;
    let nodes = Rc::new(RefCell::new(Nodes::new()));
    let registry = core.get_registry_rc()?;
    let weak = registry.downgrade();
    let _listener = registry
        .add_listener_local()
        .global({
            let nodes = nodes.clone();
            move |global| {
                let Some(props) = global.props.filter(|props| {
                    global.type_ == ObjectType::Node
                        && props.get(*pw::keys::MEDIA_CLASS) == Some("Stream/Output/Audio")
                }) else {
                    return;
                };
                let Some((id, registry)) = props.get(*pw::keys::OBJECT_SERIAL).zip(weak.upgrade())
                else {
                    return;
                };
                let name = props
                    .get(*pw::keys::APP_NAME)
                    .filter(|name| !name.is_empty())
                    .or_else(|| props.get(*pw::keys::APP_PROCESS_BINARY))
                    .unwrap_or("Audio application");
                let app = App {
                    id: id.into(),
                    name: name.into(),
                };
                let Ok(node) = registry.bind::<Node, _>(global) else {
                    return;
                };
                let running = Rc::new(Cell::new(false));
                let state = running.clone();
                let listener = node
                    .add_listener_local()
                    .info(move |info| state.set(matches!(info.state(), NodeState::Running)))
                    .register();
                nodes
                    .borrow_mut()
                    .insert(global.id, (listener, node, app, running));
            }
        })
        .global_remove({
            let nodes = nodes.clone();
            move |id| {
                nodes.borrow_mut().remove(&id);
            }
        })
        .register();
    // The first round trip announces nodes. The second returns the bound nodes' state.
    let cookie = Rc::new(Cell::new(None));
    let done = Rc::new(Cell::new(None));
    let failed = Rc::new(Cell::new(false));
    let _sync = core
        .add_listener_local()
        .info({
            let cookie = cookie.clone();
            move |info| cookie.set(Some(info.cookie()))
        })
        .done({
            let done = done.clone();
            move |id, seq| {
                if id == pw::core::PW_ID_CORE {
                    done.set(Some(seq));
                }
            }
        })
        .error({
            let failed = failed.clone();
            move |id, _, _, _| {
                if id == pw::core::PW_ID_CORE {
                    failed.set(true);
                }
            }
        })
        .register();
    let deadline = Instant::now() + STARTUP;
    for _ in 0..2 {
        let pending = core.sync(0)?;
        while done.get() != Some(pending) {
            ensure!(
                !failed.get(),
                "PipeWire connection failed during enumeration"
            );
            ensure!(
                Instant::now() < deadline,
                "timed out listing PipeWire nodes"
            );
            iterate(loop_)?;
        }
    }
    let cookie = cookie
        .get()
        .context("PipeWire did not report its daemon cookie")?;
    let mut apps: Vec<_> = nodes
        .borrow()
        .values()
        .filter(|(_, _, _, running)| running.get())
        .map(|(_, _, app, _)| App {
            id: format!("{cookie}:{}", app.id),
            name: app.name.clone(),
        })
        .collect();
    apps.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
    Ok(apps)
}

fn iterate(loop_: &MainLoop) -> Result<()> {
    // A paused stream writes nothing, so a broken pipe alone would not end an orphaned capture.
    ensure!(
        PARENT.get() == Some(&parent_id()),
        "RustTalk exited, stopping app audio capture"
    );
    let result = loop_
        .loop_()
        .iterate(Timeout::Finite(Duration::from_millis(100)));
    ensure!(
        result >= 0 || result == -4,
        "PipeWire event loop failed ({result})"
    );
    Ok(())
}

fn capture(loop_: &MainLoop, core: &Core, serial: u64) -> Result<()> {
    let stream = StreamBox::new(
        core,
        "RustTalk app audio",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::TARGET_OBJECT => serial.to_string(),
            "node.dont-fallback" => "true",
            "node.dont-move" => "true",
            "node.dont-reconnect" => "true",
        },
    )?;

    let output_error = Rc::new(Cell::new(false));
    let io_error = output_error.clone();
    let _listener = stream
        .add_local_listener_with_user_data(io::stdout().lock())
        .process(move |stream, output| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let offset = data.chunk().offset() as usize;
            let size = data.chunk().size() as usize;
            let Some(bytes) = data
                .data()
                .and_then(|bytes| bytes.get(offset..offset + size))
            else {
                io_error.set(true);
                return;
            };
            if !size.is_multiple_of(4)
                || output
                    .write_all(bytes)
                    .and_then(|()| output.flush())
                    .is_err()
            {
                io_error.set(true);
            }
        })
        .register()?;
    let failed = Rc::new(Cell::new(false));
    let _core_error = core
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

    let mut format = AudioInfoRaw::new();
    format.set_format(AudioFormat::S16LE);
    format.set_rate(48_000);
    format.set_channels(2);
    let (param, _) = PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &Value::Object(Object {
            type_: SpaTypes::ObjectParamFormat.as_raw(),
            id: ParamType::EnumFormat.as_raw(),
            properties: format.into(),
        }),
    )?;
    let mut params = [Pod::from_bytes(param.get_ref()).expect("serialized PCM format")];
    stream.connect(
        Direction::Input,
        None,
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;
    let deadline = Instant::now() + STARTUP;
    let mut started = false;
    while !failed.get() && !output_error.get() {
        match stream.state() {
            StreamState::Streaming => started = true,
            StreamState::Unconnected if started => return Ok(()),
            StreamState::Error(_) => {
                bail!("PipeWire could not capture the selected playback stream")
            }
            _ => (),
        }
        if !started && Instant::now() >= deadline {
            bail!("timed out connecting to the selected playback stream");
        }
        iterate(loop_)?;
    }
    bail!("PipeWire capture stream or output disconnected")
}
