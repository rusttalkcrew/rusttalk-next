//! macOS app audio through Core Audio process taps (macOS 14.2 and later).
//!
//! A tap mixes the chosen process, or every process except ours, down to stereo. A
//! private aggregate device wraps the tap so an IO block can read it. The block
//! converts the device-rate float samples into the shared 48 kHz s16 10 ms frames and
//! hands them to the capture worker, which calls the sink. The first start makes macOS
//! ask for System Audio Recording access (`NSAudioCaptureUsageDescription`); refused,
//! macOS delivers silence, which the worker notices and reports as `NoAccess`.

use std::{
    ffi::CStr,
    mem::MaybeUninit,
    ptr::{self, NonNull},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering::Relaxed},
        mpsc::{self, RecvTimeoutError, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::AllocAnyThread;
use objc2_app_kit::NSRunningApplication;
use objc2_core_audio::{
    AudioDeviceCreateIOProcIDWithBlock, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID,
    AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
    AudioHardwareCreateProcessTap, AudioHardwareDestroyAggregateDevice,
    AudioHardwareDestroyProcessTap, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectID, AudioObjectPropertyAddress, AudioObjectPropertySelector, CATapDescription,
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceNameKey,
    kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceTapListKey,
    kAudioAggregateDeviceUIDKey, kAudioDevicePropertyNominalSampleRate,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyProcessObjectList,
    kAudioHardwarePropertyTranslatePIDToProcessObject, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kAudioObjectUnknown,
    kAudioProcessPropertyBundleID, kAudioProcessPropertyIsRunningOutput, kAudioProcessPropertyPID,
    kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey, kAudioTapPropertyFormat,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp,
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatLinearPCM,
};
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_foundation::{NSArray, NSNumber, NSString, NSUUID};

use super::{
    App, Capture, FRAME_SAMPLES, NoAccess, SYSTEM,
    tap::{self, Converter, Process, SilenceCheck},
};

const SYSTEM_OBJECT: AudioObjectID = kAudioObjectSystemObject as AudioObjectID;

fn check(status: i32, what: &str) -> Result<()> {
    ensure!(status == 0, "{what} failed (Core Audio status {status})");
    Ok(())
}

fn address(selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Property value types that are valid for any bytes Core Audio writes.
trait Plain: Copy {}
impl Plain for u32 {}
impl Plain for i32 {}
impl Plain for f64 {}
impl Plain for AudioStreamBasicDescription {}
impl Plain for *const CFString {}

/// Read a fixed-size property, optionally qualified by a PID.
fn get<T: Plain>(object: AudioObjectID, selector: u32, pid: Option<&i32>) -> Result<T> {
    let mut value = MaybeUninit::<T>::uninit();
    let mut size = size_of::<T>() as u32;
    let qualifier = pid.map_or(ptr::null(), |pid| ptr::from_ref(pid).cast());
    // SAFETY: every pointer is live for the call, and `size` caps what Core Audio writes.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&address(selector)),
            if pid.is_some() {
                size_of::<i32>() as u32
            } else {
                0
            },
            qualifier,
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        )
    };
    check(status, "reading an audio property")?;
    ensure!(
        size as usize == size_of::<T>(),
        "unexpected audio property size"
    );
    // SAFETY: Core Audio wrote all of it, and `Plain` types accept any bytes.
    Ok(unsafe { value.assume_init() })
}

fn get_string(object: AudioObjectID, selector: u32) -> Result<CFRetained<CFString>> {
    let string: *const CFString = get(object, selector, None)?;
    let string = NonNull::new(string.cast_mut()).context("empty audio property")?;
    // SAFETY: Core Audio returns CF properties retained; this takes over that reference.
    Ok(unsafe { CFRetained::from_raw(string) })
}

/// The Core Audio object of a process, or `kAudioObjectUnknown` once it has gone.
fn process_object(pid: i32) -> Result<AudioObjectID> {
    get(
        SYSTEM_OBJECT,
        kAudioHardwarePropertyTranslatePIDToProcessObject,
        Some(&pid),
    )
}

fn process_objects() -> Result<Vec<AudioObjectID>> {
    let address = address(kAudioHardwarePropertyProcessObjectList);
    let mut size = 0;
    // SAFETY: the pointers are live for the call.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            SYSTEM_OBJECT,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
        )
    };
    check(status, "counting audio processes")?;
    let mut objects = vec![kAudioObjectUnknown; size as usize / size_of::<AudioObjectID>()];
    let mut size = size_of_val(objects.as_slice()) as u32;
    // SAFETY: `objects` holds `size` bytes; Core Audio writes at most that and says how much.
    let status = unsafe {
        AudioObjectGetPropertyData(
            SYSTEM_OBJECT,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(objects.as_mut_slice()).cast(),
        )
    };
    check(status, "listing audio processes")?;
    objects.truncate(size as usize / size_of::<AudioObjectID>());
    Ok(objects)
}

/// What people call a process: its app, found by bundle ID so a browser's helper
/// process shows the browser's name.
fn name(pid: i32, object: AudioObjectID) -> String {
    let bundle = get_string(object, kAudioProcessPropertyBundleID)
        .ok()
        .map(|id| id.to_string());
    let app = bundle.as_deref().and_then(|id| {
        tap::bundle_prefixes(id).find_map(|prefix| {
            let prefix = NSString::from_str(prefix);
            NSRunningApplication::runningApplicationsWithBundleIdentifier(&prefix)
                .firstObject()?
                .localizedName()
        })
    });
    app.or_else(|| {
        NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?.localizedName()
    })
    .map(|name| name.to_string())
    .or(bundle)
    .filter(|name| !name.is_empty())
    .unwrap_or_else(|| "Audio application".into())
}

fn own_pid() -> i32 {
    std::process::id() as i32
}

fn default_output() -> Result<AudioObjectID> {
    get(
        SYSTEM_OBJECT,
        kAudioHardwarePropertyDefaultOutputDevice,
        None,
    )
}

fn running_output(object: AudioObjectID) -> bool {
    get::<u32>(object, kAudioProcessPropertyIsRunningOutput, None).is_ok_and(|on| on != 0)
}

/// Whether something the tap listens to is playing now.
fn tapped_playing(target: Option<(i32, AudioObjectID)>, own: AudioObjectID) -> bool {
    match target {
        Some((_, object)) => running_output(object),
        None => process_objects()
            .is_ok_and(|objects| objects.into_iter().any(|o| o != own && running_output(o))),
    }
}

pub fn list_apps() -> Result<Vec<App>> {
    let processes = process_objects()?.into_iter().filter_map(|object| {
        // A process can quit between listing and reading it; skip it then.
        let pid: i32 = get(object, kAudioProcessPropertyPID, None).ok()?;
        let playing = running_output(object);
        let name = if playing {
            name(pid, object)
        } else {
            String::new()
        };
        Some(Process {
            pid,
            object,
            playing,
            name,
        })
    });
    Ok(tap::playing(processes, own_pid()))
}

pub fn capture(app: &App, sink: impl FnMut(&[i16]) + Send + 'static) -> Result<Capture> {
    let target = if app.id == SYSTEM {
        None
    } else {
        Some(tap::parse_id(&app.id)?)
    };
    let (ready, startup) = mpsc::sync_channel(1);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_worker = stop.clone();
    let worker = thread::spawn(move || {
        if let Err(error) = record(target, sink, &ready, &stop_worker) {
            tracing::warn!(error = %format_args!("{error:#}"), "Core Audio app capture ended");
            // Before audio flows, `capture` reports it; afterwards `Capture::error` does.
            if let Err(TrySendError::Full(Err(error)) | TrySendError::Disconnected(Err(error))) =
                ready.try_send(Err(error))
            {
                return Err(error);
            }
        }
        Ok(())
    });
    let capture = Capture::new(move || stop.store(true, Relaxed), worker);
    startup
        // Generous: the very first start can wait on the permission prompt.
        .recv_timeout(Duration::from_secs(30))
        .context("starting Core Audio app capture")??;
    Ok(capture)
}

/// Destroys the tap when capture ends.
struct ProcessTap(AudioObjectID);

impl Drop for ProcessTap {
    fn drop(&mut self) {
        // SAFETY: we created this tap and destroy it once.
        let _ = unsafe { AudioHardwareDestroyProcessTap(self.0) };
    }
}

/// Destroys the aggregate device when capture ends.
struct Aggregate(AudioObjectID);

impl Drop for Aggregate {
    fn drop(&mut self) {
        // SAFETY: we created this device and destroy it once.
        let _ = unsafe { AudioHardwareDestroyAggregateDevice(self.0) };
    }
}

type IoBlock = RcBlock<
    dyn Fn(
        NonNull<AudioTimeStamp>,
        NonNull<AudioBufferList>,
        NonNull<AudioTimeStamp>,
        NonNull<AudioBufferList>,
        NonNull<AudioTimeStamp>,
    ),
>;

/// Stops and removes the IO block when capture ends.
struct IoProc {
    device: AudioObjectID,
    id: AudioDeviceIOProcID,
    _block: IoBlock,
}

impl Drop for IoProc {
    fn drop(&mut self) {
        // SAFETY: the ID came from AudioDeviceCreateIOProcIDWithBlock on this device. Stop
        // waits for a running callback, so the block is unused once both return.
        unsafe {
            AudioDeviceStop(self.device, self.id);
            AudioDeviceDestroyIOProcID(self.device, self.id);
        }
    }
}

/// The buffers of a list Core Audio passed to an IO block.
///
/// # Safety
/// `list` must be valid for `'a`.
unsafe fn buffers<'a>(list: NonNull<AudioBufferList>) -> &'a [AudioBuffer] {
    let list = list.as_ptr();
    // SAFETY: the list holds `mNumberBuffers` buffers inline, starting at `mBuffers`.
    unsafe {
        std::slice::from_raw_parts(
            ptr::addr_of!((*list).mBuffers).cast::<AudioBuffer>(),
            (*list).mNumberBuffers as usize,
        )
    }
}

/// The float samples in a tap buffer.
///
/// # Safety
/// `buffer` must point at float data valid for `'a`.
unsafe fn samples<'a>(buffer: &AudioBuffer) -> &'a [f32] {
    if buffer.mData.is_null() {
        return &[];
    }
    // SAFETY: the tap format is 32-bit float, so the bytes are aligned f32 samples.
    unsafe {
        std::slice::from_raw_parts(
            buffer.mData.cast::<f32>(),
            buffer.mDataByteSize as usize / size_of::<f32>(),
        )
    }
}

fn key(key: &CStr) -> CFRetained<CFString> {
    CFString::from_str(key.to_str().expect("Core Audio keys are ASCII"))
}

fn dictionary(entries: &[(&CStr, &CFType)]) -> CFRetained<CFDictionary<CFString, CFType>> {
    let keys: Vec<_> = entries.iter().map(|(name, _)| key(name)).collect();
    let keys: Vec<&CFString> = keys.iter().map(|key| &**key).collect();
    let values: Vec<&CFType> = entries.iter().map(|(_, value)| *value).collect();
    CFDictionary::from_slices(&keys, &values)
}

fn uuid() -> CFRetained<CFString> {
    CFString::from_str(&NSUUID::new().UUIDString().to_string())
}

/// A private aggregate device holding only the tap. With the output device added as a
/// sub-device too (as some samples do), its input, such as a headset microphone, would
/// share the input buffer list and could come first; with the tap alone, every input
/// buffer is the tap's.
fn aggregate(tap: &CFString) -> Result<Aggregate> {
    let one = CFNumber::new_i32(1);
    let sub_tap = dictionary(&[
        (kAudioSubTapUIDKey, tap.as_ref()),
        (kAudioSubTapDriftCompensationKey, one.as_ref()),
    ]);
    let taps = CFArray::from_objects(&[&*sub_tap]);
    let (name, uid) = (CFString::from_str("RustTalk app audio"), uuid());
    let description = dictionary(&[
        (kAudioAggregateDeviceNameKey, name.as_ref()),
        (kAudioAggregateDeviceUIDKey, uid.as_ref()),
        // Private: the device is hidden from other apps and goes away with ours.
        (kAudioAggregateDeviceIsPrivateKey, one.as_ref()),
        (kAudioAggregateDeviceTapAutoStartKey, one.as_ref()),
        (kAudioAggregateDeviceTapListKey, taps.as_ref()),
    ]);
    let mut device = kAudioObjectUnknown;
    // SAFETY: the description and output pointer are live for the call.
    let status = unsafe {
        AudioHardwareCreateAggregateDevice(description.as_ref(), NonNull::from(&mut device))
    };
    check(status, "creating the capture device")?;
    Ok(Aggregate(device))
}

fn record(
    target: Option<(i32, AudioObjectID)>,
    mut sink: impl FnMut(&[i16]),
    ready: &mpsc::SyncSender<Result<()>>,
    stop: &AtomicBool,
) -> Result<()> {
    let processes = |objects: &[AudioObjectID]| {
        let numbers: Vec<_> = objects
            .iter()
            .map(|&object| NSNumber::new_u32(object))
            .collect();
        NSArray::from_retained_slice(&numbers)
    };
    let own = process_object(own_pid())?;
    let description = match target {
        Some((pid, object)) => {
            ensure!(process_object(pid)? == object, "the selected app quit");
            // SAFETY: a documented initializer on a fresh allocation.
            unsafe {
                CATapDescription::initStereoMixdownOfProcesses(
                    CATapDescription::alloc(),
                    &processes(&[object]),
                )
            }
        }
        None => {
            // All system audio is every process except ours, so the call never echoes.
            // RustTalk plays sound throughout a call; if Core Audio doesn't know it yet,
            // refuse rather than echo once it starts.
            ensure!(
                own != kAudioObjectUnknown,
                "Core Audio doesn't know RustTalk's own sound yet, so it can't be left out"
            );
            // SAFETY: a documented initializer on a fresh allocation.
            unsafe {
                CATapDescription::initStereoGlobalTapButExcludeProcesses(
                    CATapDescription::alloc(),
                    &processes(&[own]),
                )
            }
        }
    };
    // SAFETY: plain property setter. Private keeps the tap to this process; the tapped
    // apps stay audible because unmuted is the default.
    unsafe { description.setPrivate(true) };
    let mut tap = kAudioObjectUnknown;
    // SAFETY: the description and output pointer are live for the call.
    let status = unsafe { AudioHardwareCreateProcessTap(Some(&description), &mut tap) };
    check(status, "creating the audio tap")?;
    // Guards drop in reverse order on every exit: the IO proc, the device, then the tap.
    let tap = ProcessTap(tap);

    let format: AudioStreamBasicDescription = get(tap.0, kAudioTapPropertyFormat, None)?;
    ensure!(
        format.mFormatID == kAudioFormatLinearPCM
            && format.mFormatFlags & kAudioFormatFlagIsFloat != 0
            && format.mBitsPerChannel == 32
            && format.mChannelsPerFrame > 0,
        "unsupported audio tap format"
    );
    let interleaved = format.mFormatFlags & kAudioFormatFlagIsNonInterleaved == 0;
    let channels = format.mChannelsPerFrame as usize;
    // SAFETY: plain property getter.
    let tap_uid = CFString::from_str(&unsafe { description.UUID() }.UUIDString().to_string());
    let device = aggregate(&tap_uid)?;
    // Data arrives at the device's rate, which can differ from the tap's.
    let rate: f64 = get(device.0, kAudioDevicePropertyNominalSampleRate, None)?;
    ensure!(rate > 0.0, "the capture device has no sample rate");
    let output = default_output()?;

    // Half a second of frames; the IO block drops audio rather than wait on the sink.
    let (frames, received) = mpsc::sync_channel::<[i16; FRAME_SAMPLES]>(50);
    let converter = Mutex::new(Converter::new(rate));
    // Runs on `queue`, one call at a time. It owns only a Mutex and a SyncSender, which
    // are safe to use from that thread.
    let block: IoBlock = RcBlock::new(
        move |_: NonNull<AudioTimeStamp>,
              input: NonNull<AudioBufferList>,
              _: NonNull<AudioTimeStamp>,
              _: NonNull<AudioBufferList>,
              _: NonNull<AudioTimeStamp>| {
            let mut converter = converter.lock().unwrap_or_else(PoisonError::into_inner);
            let emit = |frame: &[i16; FRAME_SAMPLES]| {
                let _ = frames.try_send(*frame);
            };
            // SAFETY: Core Audio passes a valid list of float buffers for this call.
            let buffers = unsafe { buffers(input) };
            // SAFETY: as above, each buffer holds the tap's float samples.
            let samples = |buffer| unsafe { samples(buffer) };
            if interleaved {
                if let Some(buffer) = buffers.first() {
                    converter.push(tap::stereo(samples(buffer), channels), emit);
                }
            } else if let [left, rest @ ..] = buffers {
                let right = rest.first().map_or(&[][..], samples);
                let planar: &[&[f32]] = if right.is_empty() {
                    &[samples(left)]
                } else {
                    &[samples(left), right]
                };
                converter.push(tap::planar(planar), emit);
            }
        },
    );
    let queue = DispatchQueue::new("rusttalk.app-audio", None);
    let mut id: AudioDeviceIOProcID = None;
    // SAFETY: the block lives in `IoProc` until the IO proc is destroyed.
    let status = unsafe {
        AudioDeviceCreateIOProcIDWithBlock(
            NonNull::from(&mut id),
            device.0,
            Some(&queue),
            RcBlock::as_ptr(&block),
        )
    };
    check(status, "reading the capture device")?;
    let io = IoProc {
        device: device.0,
        id,
        _block: block,
    };
    // SAFETY: the IO proc belongs to this device.
    let status = unsafe { AudioDeviceStart(io.device, io.id) };
    check(status, "starting the capture device")?;

    if ready.send(Ok(())).is_err() {
        return Ok(());
    }
    let mut silence = SilenceCheck::default();
    let (mut heard, mut checked) = (Instant::now(), Instant::now());
    while !stop.load(Relaxed) {
        match received.recv_timeout(Duration::from_millis(100)) {
            Ok(frame) => {
                heard = Instant::now();
                if silence.feed(&frame) == Some(true) && tapped_playing(target, own) {
                    return Err(NoAccess.into());
                }
                sink(&frame);
            }
            // A lost output device (headphones unplugged) stops the callbacks.
            Err(RecvTimeoutError::Timeout) => ensure!(
                heard.elapsed() < Duration::from_secs(2),
                "the capture device stopped delivering audio"
            ),
            Err(RecvTimeoutError::Disconnected) => break,
        }
        // Each change below would leave the share silent or echoing; ending it lets the
        // call clean up.
        if checked.elapsed() >= Duration::from_secs(1) {
            checked = Instant::now();
            ensure!(default_output()? == output, "the sound output changed");
            ensure!(
                get::<f64>(device.0, kAudioDevicePropertyNominalSampleRate, None)? == rate,
                "the capture sample rate changed"
            );
            ensure!(
                process_object(own_pid())? == own,
                "RustTalk's own sound changed"
            );
            if let Some((pid, object)) = target {
                ensure!(process_object(pid)? == object, "the shared app quit");
            }
        }
    }
    Ok(())
}
