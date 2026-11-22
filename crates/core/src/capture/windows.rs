use std::{
    collections::{BTreeMap, VecDeque},
    ffi::OsString,
    os::windows::{
        ffi::OsStringExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::Relaxed},
        mpsc,
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use wasapi::{
    AudioClient, DeviceEnumerator, Direction, SampleType, SessionState, StreamMode, WaveFormat,
};
use windows::{
    Wdk::System::SystemServices::RtlGetVersion,
    Win32::{
        Foundation::{FILETIME, HANDLE, WAIT_TIMEOUT},
        System::{
            SystemInformation::OSVERSIONINFOW,
            Threading::{
                GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32,
                PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW,
                WaitForSingleObject,
            },
        },
    },
    core::PWSTR,
};

use super::{App, CHANNELS, Capture, FRAME_SAMPLES, SAMPLE_RATE, Unsupported, deliver};

pub(super) fn supported() -> Result<()> {
    let mut version = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // Use the OS build even when an application's compatibility manifest is absent.
    unsafe { RtlGetVersion(&mut version) }.ok()?;
    if version.dwBuildNumber < 20348 {
        return Err(Unsupported).context("Windows build 20348 or later is required");
    }
    Ok(())
}

struct Mta;

impl Mta {
    fn enter() -> Result<Self> {
        wasapi::initialize_mta().ok()?;
        Ok(Self)
    }
}

impl Drop for Mta {
    fn drop(&mut self) {
        wasapi::deinitialize();
    }
}

fn process(pid: u32) -> Result<OwnedHandle> {
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )?
    };
    // OpenProcess returned an owned handle; OwnedHandle closes it once.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
}

fn process_id(pid: u32, process: &OwnedHandle) -> Result<String> {
    let mut created = FILETIME::default();
    unsafe {
        GetProcessTimes(
            HANDLE(process.as_raw_handle()),
            &mut created,
            &mut FILETIME::default(),
            &mut FILETIME::default(),
            &mut FILETIME::default(),
        )?;
    }
    Ok(format!(
        "{pid}:{:08x}{:08x}",
        created.dwHighDateTime, created.dwLowDateTime
    ))
}

fn process_name(process: &OwnedHandle) -> Result<String> {
    let mut path = vec![0; 32_768];
    let mut length = path.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            HANDLE(process.as_raw_handle()),
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut length,
        )?;
    }
    let path = OsString::from_wide(&path[..length as usize]);
    Ok(Path::new(&path)
        .file_stem()
        .context("process has no executable name")?
        .to_string_lossy()
        .into_owned())
}

pub fn list_apps() -> Result<Vec<App>> {
    supported()?;
    // Enumeration requires MTA, including when the caller already has an STA.
    thread::spawn(enumerate)
        .join()
        .map_err(|_| anyhow::anyhow!("WASAPI app enumeration thread panicked"))?
}

fn enumerate() -> Result<Vec<App>> {
    let _mta = Mta::enter()?;
    let devices = DeviceEnumerator::new()?.get_device_collection(Direction::Render)?;
    let mut apps = BTreeMap::new();
    for device in &devices {
        let sessions = device?
            .get_iaudiosessionmanager()?
            .get_audiosessionenumerator()?;
        for i in 0..sessions.get_count()? {
            // A session can disappear between enumeration and inspection.
            let Ok(session) = sessions.get_session(i) else {
                continue;
            };
            if session.get_state().ok() != Some(SessionState::Active) {
                continue;
            }
            let Ok(pid) = session.get_process_id() else {
                continue;
            };
            // Never offer RustTalk's own playback: sharing it would echo the call.
            if pid == 0 || pid == std::process::id() || apps.contains_key(&pid) {
                continue;
            }
            let Ok(process) = process(pid) else { continue };
            let Ok(id) = process_id(pid, &process) else {
                continue;
            };
            let name = process_name(&process).unwrap_or_else(|_| "Audio application".into());
            apps.insert(pid, App { id, name });
        }
    }
    Ok(apps.into_values().collect())
}

pub fn capture(app: &App, sink: impl FnMut(&[i16]) + Send + 'static) -> Result<Capture> {
    supported()?;
    let id = app.id.clone();
    let (ready, startup) = mpsc::sync_channel(1);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_worker = stop.clone();
    let worker = thread::spawn(move || {
        if let Err(error) = record(&id, sink, &ready, &stop_worker) {
            tracing::warn!(%error, "WASAPI app capture ended");
            let _ = ready.try_send(Err(error));
        }
        Ok(())
    });
    let capture = Capture::new(move || stop.store(true, Relaxed), worker);
    startup
        .recv_timeout(Duration::from_secs(5))
        .context("starting WASAPI app capture")??;
    Ok(capture)
}

fn record(
    id: &str,
    mut sink: impl FnMut(&[i16]),
    ready: &mpsc::SyncSender<Result<()>>,
    stop: &AtomicBool,
) -> Result<()> {
    let _mta = Mta::enter()?;
    // All system audio is every process except ours, so the call never echoes.
    let (pid, include, process) = if id == super::SYSTEM {
        (std::process::id(), false, None)
    } else {
        let pid = id
            .split_once(':')
            .context("invalid audio process ID")?
            .0
            .parse()?;
        // Hold the process handle so a recycled PID cannot redirect the selected capture.
        let process = process(pid).context("selected audio process exited")?;
        ensure!(
            process_id(pid, &process)? == id,
            "selected audio process exited"
        );
        (pid, true, Some(process))
    };

    let format = WaveFormat::new(
        16,
        16,
        SampleType::Int,
        SAMPLE_RATE as usize,
        CHANNELS as usize,
        None,
    );
    let mut client = AudioClient::new_application_loopback_client(pid, include)?;
    client.initialize_client(
        &format,
        Direction::Capture,
        StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: 0,
        },
        None,
    )?;
    let event = client.set_get_eventhandle()?;
    let capture = client.get_audiocaptureclient()?;
    let mut pending = VecDeque::new();
    const FRAME_BYTES: usize = FRAME_SAMPLES * 2;
    let mut frame = [0; FRAME_BYTES];
    client.start_stream()?;
    if ready.send(Ok(())).is_ok() {
        let running = |process: &OwnedHandle| {
            let state = unsafe { WaitForSingleObject(HANDLE(process.as_raw_handle()), 0) };
            state == WAIT_TIMEOUT
        };
        while !stop.load(Relaxed) && process.as_ref().is_none_or(running) {
            match event.wait_for_event(50) {
                Ok(()) | Err(wasapi::WasapiError::EventTimeout) => (),
                Err(error) => return Err(error.into()),
            }
            while !stop.load(Relaxed) && capture.get_next_packet_size()?.unwrap_or(0) > 0 {
                capture.read_from_device_to_deque(&mut pending)?;
                while pending.len() >= frame.len() {
                    for (byte, next) in frame.iter_mut().zip(pending.drain(..FRAME_BYTES)) {
                        *byte = next;
                    }
                    deliver(&frame, &mut sink);
                }
            }
        }
    }
    client.stop_stream()?;
    Ok(())
}
