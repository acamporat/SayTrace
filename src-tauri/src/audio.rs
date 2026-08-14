use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(windows)]
use std::collections::VecDeque;

#[cfg(target_os = "macos")]
use std::{fmt, mem::ManuallyDrop, sync::OnceLock};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::{
    error::{CoreError, CoreResult},
    models::{AudioDevice, AudioDeviceList},
};

pub const CAPTURE_SAMPLE_RATE: u32 = 48_000;
pub const SEGMENT_SECONDS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureKind {
    Microphone,
    Loopback,
}

impl CaptureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Microphone => "microphone",
            Self::Loopback => "loopback",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CaptureSpec {
    pub session_id: String,
    pub kind: CaptureKind,
    pub device_id: Option<String>,
    pub channels: u16,
    pub directory: PathBuf,
    pub live_captions: bool,
    pub session_qpc_start: Option<u64>,
    pub qpc_frequency: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub enum CaptureCommand {
    Pause,
    Resume,
    Stop,
}

#[derive(Debug, Clone)]
pub struct CaptionChunk {
    pub session_id: String,
    pub stream_id: String,
    pub sequence: u64,
    pub start_ms: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub pcm_s16le: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClockAnchor {
    pub qpc: u64,
    pub frame_index: u64,
    pub discontinuity: bool,
    pub pause_boundary: bool,
    pub inserted_gap_frames: u64,
}

#[derive(Debug, Default)]
pub struct CaptureShared {
    level_bits: AtomicU32,
    pub dropped_packets: AtomicU64,
    pub discontinuities: AtomicU64,
    pub dropped_caption_chunks: AtomicU64,
    pub qpc_first: AtomicU64,
    pub qpc_last: AtomicU64,
    pub samples_written: AtomicU64,
    clock_anchors: Mutex<Vec<ClockAnchor>>,
    liveness: AtomicU32,
    error: Mutex<Option<String>>,
}

impl CaptureShared {
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level_bits.load(Ordering::Relaxed))
    }

    fn set_level(&self, value: f32) {
        self.level_bits
            .store(value.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn clock_anchors(&self) -> Vec<ClockAnchor> {
        self.clock_anchors.lock().clone()
    }

    fn push_clock_anchor(&self, anchor: ClockAnchor) {
        let mut anchors = self.clock_anchors.lock();
        if anchors
            .last()
            .is_some_and(|last| last.qpc == anchor.qpc && last.frame_index == anchor.frame_index)
        {
            return;
        }
        anchors.push(anchor);
    }

    pub fn mark_live(&self) -> bool {
        let error = self.error.lock();
        if error.is_some() {
            return false;
        }
        self.liveness
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn mark_stopped(&self) {
        self.liveness.store(2, Ordering::Release);
    }

    pub fn mark_failed(&self, error: impl Into<String>) {
        let mut stored = self.error.lock();
        if stored.is_none() {
            *stored = Some(error.into());
        }
        self.liveness.store(3, Ordering::Release);
        self.set_level(0.0);
    }

    pub fn is_live(&self) -> bool {
        self.liveness.load(Ordering::Acquire) == 1
    }

    pub fn failure(&self) -> Option<String> {
        self.error.lock().clone()
    }
}

#[derive(Debug)]
pub struct CaptureSummary {
    pub kind: CaptureKind,
    pub device_id: String,
    pub channels: u16,
    pub segment_paths: Vec<PathBuf>,
    pub samples_written: u64,
    pub dropped_packets: u64,
    pub discontinuities: u64,
    pub qpc_first: Option<u64>,
    pub qpc_last: Option<u64>,
    pub clock_anchors: Vec<ClockAnchor>,
}

pub fn enumerate_audio_devices() -> CoreResult<AudioDeviceList> {
    #[cfg(windows)]
    {
        let handle = thread::Builder::new()
            .name("wasapi-device-enumeration".into())
            .spawn(enumerate_windows)
            .map_err(CoreError::Io)?;
        handle
            .join()
            .map_err(|_| CoreError::Audio("device enumeration thread panicked".into()))?
    }
    #[cfg(target_os = "macos")]
    {
        enumerate_macos()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Err(CoreError::Audio(
            "audio device enumeration is unavailable on this platform".into(),
        ))
    }
}

#[cfg(not(target_os = "macos"))]
pub fn spawn_capture(
    spec: CaptureSpec,
    commands: Receiver<CaptureCommand>,
    caption_sender: Option<SyncSender<CaptionChunk>>,
    shared: Arc<CaptureShared>,
) -> CoreResult<(
    thread::JoinHandle<CoreResult<CaptureSummary>>,
    Receiver<Result<(), String>>,
)> {
    fs::create_dir_all(&spec.directory)?;
    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    let handle = thread::Builder::new()
        .name(format!("wasapi-{}", spec.kind.as_str()))
        .spawn(move || capture_thread(spec, commands, caption_sender, shared, ready_sender))
        .map_err(CoreError::Io)?;
    Ok((handle, ready_receiver))
}

#[cfg(target_os = "macos")]
pub struct MacosCaptureRequest {
    pub spec: CaptureSpec,
    pub commands: Receiver<CaptureCommand>,
    pub shared: Arc<CaptureShared>,
}

#[cfg(target_os = "macos")]
pub type CaptureLaunch = (
    thread::JoinHandle<CoreResult<CaptureSummary>>,
    Receiver<Result<(), String>>,
);

/// Starts every requested macOS audio track behind one ScreenCaptureKit stream.
///
/// Apple exposes system audio and microphone audio as separate output types on
/// the same `SCStream`. Keeping one native owner avoids overlapping start/drop
/// transitions, which can crash ScreenCaptureKit when one of two independent
/// streams fails during concurrent startup.
#[cfg(target_os = "macos")]
pub fn spawn_macos_captures(
    requests: Vec<MacosCaptureRequest>,
    caption_sender: Option<SyncSender<CaptionChunk>>,
) -> CoreResult<Vec<CaptureLaunch>> {
    validate_macos_capture_kinds(requests.iter().map(|request| request.spec.kind))?;
    for request in &requests {
        fs::create_dir_all(&request.spec.directory)?;
    }

    let mut coordinator_requests = Vec::with_capacity(requests.len());
    let mut launches: Vec<CaptureLaunch> = Vec::with_capacity(requests.len());
    for request in requests {
        let kind = request.spec.kind;
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let (result_sender, result_receiver) =
            mpsc::sync_channel::<Result<CaptureSummary, String>>(1);
        let proxy = match thread::Builder::new()
            .name(format!("screencapturekit-result-{}", kind.as_str()))
            .spawn(move || {
                result_receiver
                    .recv()
                    .map_err(|_| {
                        CoreError::Audio(
                            "ScreenCaptureKit coordinator ended before returning a track result"
                                .into(),
                        )
                    })?
                    .map_err(CoreError::Audio)
            }) {
            Ok(proxy) => proxy,
            Err(error) => {
                drop(coordinator_requests);
                for (handle, _) in launches {
                    let _ = handle.join();
                }
                return Err(CoreError::Io(error));
            }
        };
        coordinator_requests.push(MacosCoordinatorRequest {
            input: MacosCaptureInput {
                spec: request.spec,
                commands: request.commands,
                shared: request.shared,
                ready_sender,
            },
            result_sender,
        });
        launches.push((proxy, ready_receiver));
    }

    if let Err(error) = thread::Builder::new()
        .name("screencapturekit-session".into())
        .spawn(move || run_macos_capture_coordinator(coordinator_requests, caption_sender))
    {
        for (handle, _) in launches {
            let _ = handle.join();
        }
        return Err(CoreError::Io(error));
    }
    Ok(launches)
}

#[cfg(target_os = "macos")]
fn enumerate_macos() -> CoreResult<AudioDeviceList> {
    use screencapturekit::audio_devices::AudioInputDevice;

    let mut microphones = AudioInputDevice::list()
        .into_iter()
        .map(|device| AudioDevice {
            id: device.id,
            name: device.name,
            kind: "input".into(),
            is_default: device.is_default,
            is_active: true,
        })
        .collect::<Vec<_>>();
    microphones.sort_by(|left, right| {
        right
            .is_default
            .cmp(&left.is_default)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    Ok(AudioDeviceList {
        microphones,
        // ScreenCaptureKit captures the system mix instead of binding to one
        // render endpoint. Keep a stable semantic device for the existing UI.
        outputs: vec![AudioDevice {
            id: "system-audio".into(),
            name: "System Audio".into(),
            kind: "output".into(),
            is_default: true,
            is_active: true,
        }],
    })
}

#[cfg(target_os = "macos")]
fn validate_macos_capture_kinds(kinds: impl IntoIterator<Item = CaptureKind>) -> CoreResult<()> {
    let kinds = kinds.into_iter().collect::<Vec<_>>();
    if kinds.is_empty() || kinds.len() > 2 {
        return Err(CoreError::InvalidInput(
            "macOS capture requires one or two audio sources".into(),
        ));
    }
    if kinds.len() == 2 && kinds[0] == kinds[1] {
        return Err(CoreError::InvalidInput(format!(
            "macOS capture received duplicate {} sources",
            kinds[0].as_str()
        )));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
struct MacosCaptureInput {
    spec: CaptureSpec,
    commands: Receiver<CaptureCommand>,
    shared: Arc<CaptureShared>,
    ready_sender: SyncSender<Result<(), String>>,
}

#[cfg(target_os = "macos")]
struct MacosCoordinatorRequest {
    input: MacosCaptureInput,
    result_sender: SyncSender<Result<CaptureSummary, String>>,
}

#[cfg(target_os = "macos")]
fn run_macos_capture_coordinator(
    requests: Vec<MacosCoordinatorRequest>,
    caption_sender: Option<SyncSender<CaptionChunk>>,
) {
    let mut inputs = Vec::with_capacity(requests.len());
    let mut result_senders = Vec::with_capacity(requests.len());
    for request in requests {
        inputs.push(request.input);
        result_senders.push(request.result_sender);
    }
    let results = capture_macos_group(inputs, caption_sender);
    for (sender, result) in result_senders.into_iter().zip(results) {
        let _ = sender.send(result.map_err(|error| capture_error_message(&error)));
    }
}

#[cfg(target_os = "macos")]
fn capture_macos_group(
    inputs: Vec<MacosCaptureInput>,
    caption_sender: Option<SyncSender<CaptionChunk>>,
) -> Vec<CoreResult<CaptureSummary>> {
    let sinks = inputs
        .iter()
        .map(|input| (input.shared.clone(), input.ready_sender.clone()))
        .collect::<Vec<_>>();
    let count = sinks.len();
    let outcome = capture_macos_group_inner(inputs, caption_sender);
    let results = match outcome {
        Ok(summaries) => summaries.into_iter().map(Ok).collect::<Vec<_>>(),
        Err(error) => {
            let message = capture_error_message(&error);
            (0..count)
                .map(|_| Err(CoreError::Audio(message.clone())))
                .collect::<Vec<_>>()
        }
    };
    for ((shared, ready_sender), result) in sinks.into_iter().zip(&results) {
        match result {
            Ok(_) => shared.mark_stopped(),
            Err(error) => {
                let message = capture_error_message(error);
                shared.mark_failed(message.clone());
                let _ = ready_sender.try_send(Err(message));
            }
        }
    }
    results
}

#[cfg(target_os = "macos")]
fn capture_error_message(error: &CoreError) -> String {
    match error {
        CoreError::Audio(message) => message.clone(),
        _ => error.to_string(),
    }
}

#[cfg(target_os = "macos")]
fn capture_macos_group_inner(
    inputs: Vec<MacosCaptureInput>,
    caption_sender: Option<SyncSender<CaptionChunk>>,
) -> CoreResult<Vec<CaptureSummary>> {
    use screencapturekit::prelude::*;
    use screencapturekit::stream::delegate_trait::ErrorHandler;

    validate_macos_capture_kinds(inputs.iter().map(|input| input.spec.kind))?;
    ensure_macos_capture_lifecycle_healthy()?;
    let lifecycle = macos_stream_lifecycle().try_lock().ok_or_else(|| {
        CoreError::Audio(
            "a previous ScreenCaptureKit lifecycle transition is still unresolved; restart SayTrace before retrying capture"
                .into(),
        )
    })?;
    // The first check gives fast feedback. This second check closes the race
    // where a prior owner quarantines its stream immediately before releasing
    // the lifecycle lock that this coordinator then acquires.
    ensure_macos_capture_lifecycle_healthy()?;
    let content = SCShareableContent::get().map_err(|error| {
        CoreError::Audio(format!(
            "ScreenCaptureKit could not access shareable content; allow Screen & System Audio Recording in System Settings: {error}"
        ))
    })?;
    let display = content
        .displays()
        .into_iter()
        .next()
        .ok_or_else(|| CoreError::Audio("ScreenCaptureKit found no display".into()))?;
    let filter = SCContentFilter::create()
        .with_display(&display)
        .with_excluding_windows(&[])
        .build();
    let captures_microphone = inputs
        .iter()
        .any(|input| input.spec.kind == CaptureKind::Microphone);
    let captures_audio = inputs
        .iter()
        .any(|input| input.spec.kind == CaptureKind::Loopback);
    let configured_channels = if captures_audio {
        2
    } else {
        inputs[0].spec.channels as i32
    };
    let mut configuration = SCStreamConfiguration::new()
        .with_width(2)
        .with_height(2)
        .with_queue_depth(8)
        .with_sample_rate(CAPTURE_SAMPLE_RATE as i32)
        .with_channel_count(configured_channels)
        .with_captures_audio(captures_audio)
        .with_captures_microphone(captures_microphone)
        .with_excludes_current_process_audio(true);
    if let Some(device_id) = inputs
        .iter()
        .find(|input| input.spec.kind == CaptureKind::Microphone)
        .and_then(|input| input.spec.device_id.as_deref())
        .filter(|value| !value.is_empty())
    {
        configuration.set_microphone_capture_device_id(device_id);
    }

    let mut tracks = inputs
        .into_iter()
        .map(|input| MacosTrackRuntime::new(input, caption_sender.clone()))
        .collect::<CoreResult<Vec<_>>>()?;
    let delegate_states = tracks
        .iter()
        .map(|track| track.state.clone())
        .collect::<Vec<_>>();

    let mut owner = ManagedMacStream::new(SCStream::new_with_delegate(
        &filter,
        &configuration,
        ErrorHandler::new(move |error| {
            let message = format!("ScreenCaptureKit stopped capture: {error}");
            for state in &delegate_states {
                state.fail(message.clone());
            }
        }),
    ));
    let mut registration_error = None;
    for track in &tracks {
        let handler_state = track.state.clone();
        let output_type = track.output_type;
        match owner.stream_mut().add_output_handler(
            move |sample: CMSampleBuffer, _: SCStreamOutputType| {
                handler_state.handle_sample(sample);
            },
            output_type,
        ) {
            Some(handler_id) => owner.handlers.push((handler_id, output_type)),
            None => {
                registration_error = Some(format!(
                    "ScreenCaptureKit rejected the {} audio output",
                    track.state.spec.kind.as_str()
                ));
                break;
            }
        }
    }
    if let Some(error) = registration_error {
        owner.release_unstarted_locked();
        drop(lifecycle);
        for track in &tracks {
            track.state.begin_closing();
        }
        return Err(CoreError::Audio(error));
    }
    if let Err(error) = owner.start_locked() {
        drop(lifecycle);
        for track in &tracks {
            track.state.begin_closing();
        }
        return Err(CoreError::Audio(format!(
            "ScreenCaptureKit could not start audio capture; check microphone and Screen & System Audio Recording permissions: {error}"
        )));
    }
    let timestamp_converter = match owner.timestamp_converter() {
        Ok(converter) => Arc::new(converter),
        Err(error) => {
            for track in &tracks {
                track.state.begin_closing();
            }
            let cleanup = owner.stop_and_release_locked().err();
            drop(lifecycle);
            let suffix = cleanup
                .map(|cleanup| format!("; {cleanup}"))
                .unwrap_or_default();
            return Err(CoreError::Audio(format!("{error}{suffix}")));
        }
    };
    for track in &tracks {
        if track
            .state
            .install_timestamp_converter(timestamp_converter.clone())
            .is_err()
        {
            for track in &tracks {
                track.state.begin_closing();
            }
            let cleanup = owner.stop_and_release_locked().err();
            drop(lifecycle);
            let suffix = cleanup
                .map(|cleanup| format!("; {cleanup}"))
                .unwrap_or_default();
            return Err(CoreError::Audio(format!(
                "ScreenCaptureKit timestamp synchronization was initialized more than once{suffix}"
            )));
        }
    }
    let mut startup_delegate_error = None;
    for track in &tracks {
        if let Err(error) = track.state.mark_ready() {
            startup_delegate_error = Some(error);
            break;
        }
    }
    if let Some(error) = startup_delegate_error {
        for track in &tracks {
            track.state.begin_closing();
        }
        let cleanup = owner.stop_and_release_locked().err();
        drop(lifecycle);
        let suffix = cleanup
            .map(|error| format!("; {error}"))
            .unwrap_or_default();
        return Err(CoreError::Audio(format!("{error}{suffix}")));
    }
    drop(lifecycle);

    for track in &tracks {
        let _ = track.ready_sender.try_send(Ok(()));
    }
    let capture_error = macos_capture_command_loop(&mut tracks);
    for track in &tracks {
        track.state.begin_closing();
    }
    let stop_error = owner.stop_and_release().err().map(CoreError::Audio);
    let callbacks_drained = tracks
        .iter()
        .all(|track| track.state.wait_for_callbacks(Duration::from_secs(2)));

    let mut summaries = Vec::with_capacity(tracks.len());
    let mut finalization_errors = Vec::new();
    for track in tracks {
        match track.finalize() {
            Ok(summary) => summaries.push(summary),
            Err(error) => finalization_errors.push(error.to_string()),
        }
    }
    if !callbacks_drained {
        finalization_errors
            .push("ScreenCaptureKit callbacks did not drain within two seconds after stop".into());
    }
    if let Some(error) = capture_error.or(stop_error) {
        finalization_errors.insert(0, error.to_string());
    }
    if !finalization_errors.is_empty() {
        return Err(CoreError::Audio(finalization_errors.join("; ")));
    }
    Ok(summaries)
}

#[cfg(target_os = "macos")]
fn macos_stream_lifecycle() -> &'static Mutex<()> {
    static LIFECYCLE: OnceLock<Mutex<()>> = OnceLock::new();
    LIFECYCLE.get_or_init(|| Mutex::new(()))
}

#[cfg(target_os = "macos")]
static MACOS_CAPTURE_POISONED: AtomicU32 = AtomicU32::new(0);

/// Prevents another ScreenCaptureKit session after an unresolved native
/// lifecycle operation. Process restart is the only safe way to release a
/// quarantined stream or cancel a hot operation owned by the bridge.
#[cfg(target_os = "macos")]
pub fn poison_macos_capture_lifecycle() {
    MACOS_CAPTURE_POISONED.store(1, Ordering::Release);
}

#[cfg(target_os = "macos")]
fn ensure_macos_capture_lifecycle_healthy() -> CoreResult<()> {
    if MACOS_CAPTURE_POISONED.load(Ordering::Acquire) != 0 {
        Err(CoreError::Audio(
            "ScreenCaptureKit has an unresolved native lifecycle operation; restart SayTrace before retrying capture"
                .into(),
        ))
    } else {
        Ok(())
    }
}

#[cfg(target_os = "macos")]
#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMSyncConvertTime(
        time: screencapturekit::cm::CMTime,
        from_clock_or_timebase: *const std::ffi::c_void,
        to_clock_or_timebase: *const std::ffi::c_void,
    ) -> screencapturekit::cm::CMTime;
    fn CMClockConvertHostTimeToSystemUnits(host_time: screencapturekit::cm::CMTime) -> u64;
}

#[cfg(target_os = "macos")]
struct MacosTimestampConverter {
    source_clock: screencapturekit::cm::CMClock,
    host_clock: screencapturekit::cm::CMClock,
}

#[cfg(target_os = "macos")]
impl MacosTimestampConverter {
    fn new(source_clock: screencapturekit::cm::CMClock) -> Self {
        Self {
            source_clock,
            host_clock: screencapturekit::cm::CMClock::host_time_clock(),
        }
    }

    fn to_host_nanoseconds(&self, time: screencapturekit::cm::CMTime) -> Option<u64> {
        if !is_numeric_cmtime(time) {
            return None;
        }
        let host_time = unsafe {
            CMSyncConvertTime(time, self.source_clock.as_ptr(), self.host_clock.as_ptr())
        };
        if !is_numeric_cmtime(host_time) {
            return None;
        }
        let ticks = unsafe { CMClockConvertHostTimeToSystemUnits(host_time) };
        Some(mach_ticks_to_nanoseconds(ticks))
    }
}

#[cfg(target_os = "macos")]
fn is_numeric_cmtime(time: screencapturekit::cm::CMTime) -> bool {
    const VALID: u32 = 1 << 0;
    const POSITIVE_INFINITY: u32 = 1 << 2;
    const NEGATIVE_INFINITY: u32 = 1 << 3;
    const INDEFINITE: u32 = 1 << 4;

    time.flags & VALID != 0
        && time.flags & (POSITIVE_INFINITY | NEGATIVE_INFINITY | INDEFINITE) == 0
        && time.value >= 0
        && time.timescale > 0
        && time.epoch == 0
}

#[cfg(target_os = "macos")]
struct ManagedMacStream {
    stream: Option<screencapturekit::stream::SCStream>,
    handlers: Vec<(
        usize,
        screencapturekit::stream::output_type::SCStreamOutputType,
    )>,
    start_attempted: bool,
}

#[cfg(target_os = "macos")]
impl ManagedMacStream {
    fn new(stream: screencapturekit::stream::SCStream) -> Self {
        Self {
            stream: Some(stream),
            handlers: Vec::new(),
            start_attempted: false,
        }
    }

    fn stream_mut(&mut self) -> &mut screencapturekit::stream::SCStream {
        self.stream.as_mut().expect("managed stream is present")
    }

    fn start_locked(&mut self) -> Result<(), String> {
        self.start_attempted = true;
        if let Err(error) = self.stream_mut().start_capture() {
            let cleanup = self.stop_and_release_locked().err();
            let suffix = cleanup
                .map(|cleanup| format!("; {cleanup}"))
                .unwrap_or_default();
            return Err(format!("{error}{suffix}"));
        }
        Ok(())
    }

    fn timestamp_converter(&self) -> Result<MacosTimestampConverter, String> {
        let Some(clock) = self
            .stream
            .as_ref()
            .and_then(screencapturekit::stream::SCStream::synchronization_clock)
        else {
            return Err(
                "ScreenCaptureKit did not provide the clock used by its audio sample timestamps"
                    .into(),
            );
        };
        // screencapturekit 8.0.1's Swift bridge returns this clock at +0,
        // while apple-cf 0.9.3's CMClock::from_raw adopts it without retaining.
        // Suppress that borrowed wrapper's Drop, then Clone the underlying
        // CMClock to take the +1 reference owned by the converter.
        let clock = ManuallyDrop::new(clock);
        let retained_clock = screencapturekit::cm::CMClock::clone(&clock);
        Ok(MacosTimestampConverter::new(retained_clock))
    }

    fn release_unstarted_locked(&mut self) {
        if let Some(mut stream) = self.stream.take() {
            for (handler_id, output_type) in self.handlers.drain(..).rev() {
                let _ = stream.remove_output_handler(handler_id, output_type);
            }
            drop(stream);
        }
    }

    fn stop_and_release(&mut self) -> Result<(), String> {
        let _lifecycle = macos_stream_lifecycle().lock();
        self.stop_and_release_locked()
    }

    fn stop_and_release_locked(&mut self) -> Result<(), String> {
        let Some(mut stream) = self.stream.take() else {
            return Ok(());
        };
        if self.start_attempted {
            if let Err(error) = stream.stop_capture() {
                // v8.0.1 drops SCStream by directly releasing the native object.
                // A failed/partial lifecycle transition can make that destructor
                // crash inside -[SCStream dealloc]. Retaining the object for the
                // remainder of this process is the safe failure mode.
                poison_macos_capture_lifecycle();
                std::mem::forget(stream);
                self.handlers.clear();
                return Err(format!(
                    "ScreenCaptureKit stop failed: {error}; retained the unsafe native stream until process exit"
                ));
            }
        }
        for (handler_id, output_type) in self.handlers.drain(..).rev() {
            let _ = stream.remove_output_handler(handler_id, output_type);
        }
        drop(stream);
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl Drop for ManagedMacStream {
    fn drop(&mut self) {
        if let Some(stream) = self.stream.take() {
            log::error!(
                "quarantining a ScreenCaptureKit stream because controlled teardown was bypassed"
            );
            poison_macos_capture_lifecycle();
            std::mem::forget(stream);
        }
    }
}

#[cfg(target_os = "macos")]
struct MacosTrackRuntime {
    state: Arc<MacosCaptureState>,
    commands: Receiver<CaptureCommand>,
    ready_sender: SyncSender<Result<(), String>>,
    output_type: screencapturekit::stream::output_type::SCStreamOutputType,
    writer_thread: Option<thread::JoinHandle<CoreResult<WriterSummary>>>,
    stopped: bool,
}

#[cfg(target_os = "macos")]
impl MacosTrackRuntime {
    fn new(
        input: MacosCaptureInput,
        caption_sender: Option<SyncSender<CaptionChunk>>,
    ) -> CoreResult<Self> {
        let (writer_sender, writer_receiver) = mpsc::sync_channel(512);
        let writer_directory = input.spec.directory.clone();
        let writer_channels = input.spec.channels;
        let writer_thread = thread::Builder::new()
            .name(format!("recording-writer-{}", input.spec.kind.as_str()))
            .spawn(move || writer_loop(writer_directory, writer_channels, writer_receiver))
            .map_err(CoreError::Io)?;
        let output_type = match input.spec.kind {
            CaptureKind::Microphone => {
                screencapturekit::stream::output_type::SCStreamOutputType::Microphone
            }
            CaptureKind::Loopback => {
                screencapturekit::stream::output_type::SCStreamOutputType::Audio
            }
        };
        Ok(Self {
            state: Arc::new(MacosCaptureState::new(
                input.spec,
                writer_sender,
                caption_sender,
                input.shared,
            )),
            commands: input.commands,
            ready_sender: input.ready_sender,
            output_type,
            writer_thread: Some(writer_thread),
            stopped: false,
        })
    }

    fn finalize(mut self) -> CoreResult<CaptureSummary> {
        self.state.stop_writer()?;
        let writer_summary = self
            .writer_thread
            .take()
            .expect("writer thread is present")
            .join()
            .map_err(|_| CoreError::Audio("recording writer thread panicked".into()))??;
        let samples_written = writer_summary.samples_written;
        let spec = &self.state.spec;
        let shared = &self.state.shared;
        shared
            .samples_written
            .store(samples_written, Ordering::Relaxed);
        if let Some(last) = nonzero(shared.qpc_last.load(Ordering::Relaxed)) {
            shared.push_clock_anchor(ClockAnchor {
                qpc: last,
                frame_index: samples_written / u64::from(spec.channels),
                discontinuity: false,
                pause_boundary: false,
                inserted_gap_frames: 0,
            });
        }
        shared.set_level(0.0);
        Ok(CaptureSummary {
            kind: spec.kind,
            device_id: spec.device_id.clone().unwrap_or_else(|| match spec.kind {
                CaptureKind::Microphone => "default-microphone".into(),
                CaptureKind::Loopback => "system-audio".into(),
            }),
            channels: spec.channels,
            segment_paths: writer_summary.segment_paths,
            samples_written,
            dropped_packets: shared.dropped_packets.load(Ordering::Relaxed),
            discontinuities: shared.discontinuities.load(Ordering::Relaxed),
            qpc_first: nonzero(shared.qpc_first.load(Ordering::Relaxed)),
            qpc_last: nonzero(shared.qpc_last.load(Ordering::Relaxed)),
            clock_anchors: shared.clock_anchors(),
        })
    }
}

#[cfg(target_os = "macos")]
impl Drop for MacosTrackRuntime {
    fn drop(&mut self) {
        self.state.begin_closing();
        let _ = self.state.stop_writer();
        if let Some(writer_thread) = self.writer_thread.take() {
            let _ = writer_thread.join();
        }
    }
}

#[cfg(target_os = "macos")]
fn macos_capture_command_loop(tracks: &mut [MacosTrackRuntime]) -> Option<CoreError> {
    loop {
        for track in tracks.iter_mut() {
            loop {
                match track.commands.try_recv() {
                    Ok(CaptureCommand::Pause) if !track.stopped => track.state.set_paused(true),
                    Ok(CaptureCommand::Resume) if !track.stopped => track.state.set_paused(false),
                    Ok(CaptureCommand::Stop) | Err(TryRecvError::Disconnected) => {
                        track.stopped = true;
                        track.state.begin_closing();
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Ok(_) => {}
                }
            }
            if let Some(error) = track.state.error() {
                return Some(CoreError::Audio(error));
            }
        }
        if tracks.iter().all(|track| track.stopped) {
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(target_os = "macos")]
struct MacosCaptureState {
    spec: CaptureSpec,
    writer: Mutex<Option<SyncSender<WriterMessage>>>,
    captions: Option<SyncSender<CaptionChunk>>,
    shared: Arc<CaptureShared>,
    timestamp_converter: OnceLock<Arc<MacosTimestampConverter>>,
    audio_format: OnceLock<MacosPcmFormat>,
    paused: AtomicU32,
    sequence: AtomicU64,
    frame_index: AtomicU64,
    first_timestamp: AtomicU64,
    pause_boundary: AtomicU32,
    closing: AtomicU32,
    active_callbacks: AtomicU32,
    observed_samples: AtomicU64,
    nonzero_samples: AtomicU64,
    raw_nonzero_seen: AtomicU32,
    silence_reported: AtomicU32,
    error: Mutex<Option<String>>,
}

#[cfg(target_os = "macos")]
impl MacosCaptureState {
    fn new(
        spec: CaptureSpec,
        writer: SyncSender<WriterMessage>,
        captions: Option<SyncSender<CaptionChunk>>,
        shared: Arc<CaptureShared>,
    ) -> Self {
        Self {
            spec,
            writer: Mutex::new(Some(writer)),
            captions,
            shared,
            timestamp_converter: OnceLock::new(),
            audio_format: OnceLock::new(),
            // `start_capture` may begin dispatching samples before it returns.
            // Keep handlers gated until the stream clock converter is installed.
            paused: AtomicU32::new(1),
            sequence: AtomicU64::new(0),
            frame_index: AtomicU64::new(0),
            first_timestamp: AtomicU64::new(0),
            pause_boundary: AtomicU32::new(0),
            closing: AtomicU32::new(0),
            active_callbacks: AtomicU32::new(0),
            observed_samples: AtomicU64::new(0),
            nonzero_samples: AtomicU64::new(0),
            raw_nonzero_seen: AtomicU32::new(0),
            silence_reported: AtomicU32::new(0),
            error: Mutex::new(None),
        }
    }

    fn set_paused(&self, value: bool) {
        if !value && self.closing.load(Ordering::Acquire) != 0 {
            return;
        }
        self.paused.store(value as u32, Ordering::Release);
        if !value {
            self.pause_boundary.store(1, Ordering::Release);
        }
        self.shared.set_level(0.0);
    }

    fn begin_closing(&self) {
        self.closing.store(1, Ordering::Release);
        self.paused.store(1, Ordering::Release);
        self.shared.set_level(0.0);
    }

    fn wait_for_callbacks(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while self.active_callbacks.load(Ordering::Acquire) != 0 {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(2));
        }
        true
    }

    fn install_timestamp_converter(
        &self,
        converter: Arc<MacosTimestampConverter>,
    ) -> Result<(), Arc<MacosTimestampConverter>> {
        self.timestamp_converter.set(converter)
    }

    fn mark_ready(&self) -> Result<(), String> {
        if self.timestamp_converter.get().is_none() {
            return Err("ScreenCaptureKit timestamp synchronization is unavailable".into());
        }
        let error = self.error.lock();
        if let Some(message) = error.as_ref() {
            return Err(message.clone());
        }
        if self.shared.mark_live() {
            self.paused.store(0, Ordering::Release);
            Ok(())
        } else {
            Err(self
                .shared
                .failure()
                .unwrap_or_else(|| "ScreenCaptureKit ended during startup".into()))
        }
    }

    fn fail(&self, message: String) {
        let mut error = self.error.lock();
        if error.is_none() {
            *error = Some(message.clone());
        }
        self.shared.mark_failed(message);
    }

    fn error(&self) -> Option<String> {
        self.error.lock().clone()
    }

    fn stop_writer(&self) -> CoreResult<()> {
        if let Some(writer) = self.writer.lock().take() {
            writer.send(WriterMessage::Stop).map_err(|_| {
                CoreError::Audio("recording writer stopped before finalization".into())
            })?;
        }
        Ok(())
    }

    fn handle_sample(&self, sample: screencapturekit::prelude::CMSampleBuffer) {
        use screencapturekit::prelude::CMSampleBufferExt;

        if self.closing.load(Ordering::Acquire) != 0 {
            return;
        }
        self.active_callbacks.fetch_add(1, Ordering::AcqRel);
        let _callback = MacosCallbackGuard(&self.active_callbacks);
        if self.closing.load(Ordering::Acquire) != 0
            || self.paused.load(Ordering::Acquire) != 0
            || self.error().is_some()
        {
            return;
        }
        let Some(buffers) = sample.audio_buffer_list() else {
            self.shared.dropped_packets.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let mut raw_nonzero = false;
        let raw_buffers = (&buffers)
            .into_iter()
            .map(|buffer| {
                let bytes = buffer.data();
                raw_nonzero |= bytes.iter().any(|byte| *byte != 0);
                (buffer.number_channels, bytes)
            })
            .collect::<Vec<_>>();
        if raw_nonzero {
            self.raw_nonzero_seen.store(1, Ordering::Relaxed);
        }
        let observed_format = match MacosPcmFormat::from_sample(&sample) {
            Ok(format) => format,
            Err(error) => {
                self.fail(error.to_string());
                return;
            }
        };
        let format = if let Some(format) = self.audio_format.get() {
            if *format != observed_format {
                self.fail(format!(
                    "ScreenCaptureKit changed the {} audio format during capture from {format} to {observed_format}",
                    self.spec.kind.as_str()
                ));
                return;
            }
            *format
        } else {
            if self.audio_format.set(observed_format).is_err() {
                self.fail(format!(
                    "ScreenCaptureKit could not initialize the {} audio format",
                    self.spec.kind.as_str()
                ));
                return;
            }
            log::info!(
                "ScreenCaptureKit {} audio format: {}; buffer channels: {:?}",
                self.spec.kind.as_str(),
                observed_format,
                raw_buffers
                    .iter()
                    .map(|(channels, _)| *channels)
                    .collect::<Vec<_>>()
            );
            observed_format
        };
        let converted = match pcm_audio_buffers_to_s16(&raw_buffers, format, self.spec.channels) {
            Ok(converted) => converted,
            Err(error) => {
                self.fail(error.to_string());
                return;
            }
        };
        let pcm = converted.bytes;
        let sample_count = (pcm.len() / 2) as u64;
        let frame_count = sample_count / self.spec.channels as u64;
        if frame_count == 0 {
            return;
        }
        let timestamp = sample.output_presentation_timestamp();
        let Some(timestamp_ns) = self
            .timestamp_converter
            .get()
            .and_then(|converter| converter.to_host_nanoseconds(timestamp))
        else {
            self.fail(
                "ScreenCaptureKit returned an audio timestamp that could not be converted to macOS host time"
                    .into(),
            );
            return;
        };
        let first = self.first_timestamp.load(Ordering::Relaxed);
        let base = if first == 0 {
            self.first_timestamp.store(timestamp_ns, Ordering::Relaxed);
            self.shared.qpc_first.store(timestamp_ns, Ordering::Relaxed);
            timestamp_ns
        } else {
            first
        };
        let end_ns = timestamp_ns.saturating_add(
            (frame_count as u128 * 1_000_000_000_u128 / CAPTURE_SAMPLE_RATE as u128) as u64,
        );
        self.shared.qpc_last.store(end_ns, Ordering::Relaxed);
        let frame_index = self.frame_index.fetch_add(frame_count, Ordering::Relaxed);
        if frame_index == 0 || self.pause_boundary.swap(0, Ordering::AcqRel) != 0 {
            self.shared.push_clock_anchor(ClockAnchor {
                qpc: timestamp_ns,
                frame_index,
                discontinuity: false,
                pause_boundary: frame_index != 0,
                inserted_gap_frames: 0,
            });
        }
        self.shared
            .samples_written
            .fetch_add(sample_count, Ordering::Relaxed);
        let observed_samples = self
            .observed_samples
            .fetch_add(sample_count, Ordering::Relaxed)
            .saturating_add(sample_count);
        if converted.nonzero_samples > 0 {
            self.nonzero_samples
                .fetch_add(converted.nonzero_samples, Ordering::Relaxed);
        }
        let silence_limit = u64::from(CAPTURE_SAMPLE_RATE)
            .saturating_mul(u64::from(self.spec.channels))
            .saturating_mul(3);
        if self.spec.kind == CaptureKind::Microphone
            && observed_samples >= silence_limit
            && self.nonzero_samples.load(Ordering::Relaxed) == 0
            && self
                .silence_reported
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            let raw_detail = if self.raw_nonzero_seen.load(Ordering::Relaxed) == 0 {
                "the native buffers were zero-filled"
            } else {
                "the native buffers contained data below signed-16-bit resolution"
            };
            log::warn!(
                "ScreenCaptureKit microphone delivered three seconds of exact digital silence ({raw_detail}; {format}; device {}). This can be expected from the built-in microphone while a MacBook is closed; otherwise check macOS Microphone permission and the selected input device.",
                self.spec.device_id.as_deref().unwrap_or("system default")
            );
        }
        let rms = (converted.sum_squares / sample_count as f64).sqrt() as f32;
        self.shared.set_level((rms * 3.0).clamp(0.0, 1.0));
        let caption_pcm = self.spec.live_captions.then(|| pcm.clone());
        let writer_result = self
            .writer
            .lock()
            .as_ref()
            .ok_or_else(|| CoreError::Audio("recording writer is unavailable".into()))
            .and_then(|writer| send_writer_packet(writer, pcm));
        if let Err(error) = writer_result {
            self.fail(error.to_string());
            return;
        }
        if let (Some(sender), Some(pcm_s16le)) = (&self.captions, caption_pcm) {
            let caption_base = self
                .spec
                .session_qpc_start
                .filter(|start| timestamp_ns >= *start)
                .unwrap_or(base);
            let frequency = self
                .spec
                .qpc_frequency
                .filter(|value| *value > 0)
                .unwrap_or(1_000_000_000);
            let start_ms = ((timestamp_ns.saturating_sub(caption_base) as u128 * 1_000_u128)
                / frequency as u128) as u64;
            let sequence = self.sequence.load(Ordering::Relaxed);
            let chunk = CaptionChunk {
                session_id: self.spec.session_id.clone(),
                stream_id: self.spec.kind.as_str().into(),
                sequence,
                start_ms,
                sample_rate: CAPTURE_SAMPLE_RATE,
                channels: self.spec.channels,
                pcm_s16le,
            };
            match sender.try_send(chunk) {
                Ok(()) => {
                    self.sequence.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Full(_)) => {
                    self.shared
                        .dropped_caption_chunks
                        .fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => {}
            }
        }
    }
}

#[cfg(target_os = "macos")]
struct MacosCallbackGuard<'a>(&'a AtomicU32);

#[cfg(target_os = "macos")]
impl Drop for MacosCallbackGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(target_os = "macos")]
const MACOS_PCM_FLAG_IS_FLOAT: u32 = 1 << 0;
#[cfg(target_os = "macos")]
const MACOS_PCM_FLAG_IS_BIG_ENDIAN: u32 = 1 << 1;
#[cfg(target_os = "macos")]
const MACOS_PCM_FLAG_IS_SIGNED_INTEGER: u32 = 1 << 2;
#[cfg(target_os = "macos")]
const MACOS_PCM_FLAG_IS_ALIGNED_HIGH: u32 = 1 << 4;
#[cfg(target_os = "macos")]
const MACOS_PCM_FLAG_IS_NON_INTERLEAVED: u32 = 1 << 5;
#[cfg(target_os = "macos")]
const MACOS_PCM_FLAG_SAMPLE_FRACTION_MASK: u32 = 0x3f << 7;

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MacosPcmEncoding {
    Float32,
    Float64,
    SignedInteger,
}

#[cfg(target_os = "macos")]
impl fmt::Display for MacosPcmEncoding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Float32 => "Float32",
            Self::Float64 => "Float64",
            Self::SignedInteger => "signed integer",
        })
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MacosPcmFormat {
    sample_rate: u32,
    channels: u32,
    bits_per_channel: u32,
    bytes_per_frame: u32,
    flags: u32,
    encoding: MacosPcmEncoding,
}

#[cfg(target_os = "macos")]
impl MacosPcmFormat {
    fn from_sample(sample: &screencapturekit::prelude::CMSampleBuffer) -> CoreResult<Self> {
        let description = sample.format_description().ok_or_else(|| {
            CoreError::Audio(
                "ScreenCaptureKit returned audio without a Core Media format description".into(),
            )
        })?;
        if !description.is_pcm() {
            return Err(CoreError::Audio(format!(
                "ScreenCaptureKit returned unsupported {} audio instead of linear PCM",
                description.media_subtype_string()
            )));
        }

        let sample_rate = description.audio_sample_rate().ok_or_else(|| {
            CoreError::Audio("ScreenCaptureKit omitted the PCM sample rate".into())
        })?;
        let rounded_sample_rate = sample_rate.round();
        if !sample_rate.is_finite()
            || (sample_rate - rounded_sample_rate).abs() > f64::EPSILON
            || !(1.0..=u32::MAX as f64).contains(&rounded_sample_rate)
        {
            return Err(CoreError::Audio(format!(
                "ScreenCaptureKit returned an invalid PCM sample rate ({sample_rate})"
            )));
        }

        let bits_per_channel = description
            .audio_bits_per_channel()
            .ok_or_else(|| CoreError::Audio("ScreenCaptureKit omitted the PCM bit depth".into()))?;
        let flags = description.audio_format_flags().ok_or_else(|| {
            CoreError::Audio("ScreenCaptureKit omitted the PCM format flags".into())
        })?;
        let is_float = flags & MACOS_PCM_FLAG_IS_FLOAT != 0;
        let is_signed_integer = flags & MACOS_PCM_FLAG_IS_SIGNED_INTEGER != 0;
        let encoding = match (is_float, is_signed_integer, bits_per_channel) {
            (true, false, 32) => MacosPcmEncoding::Float32,
            (true, false, 64) => MacosPcmEncoding::Float64,
            (false, true, 8 | 16 | 24 | 32) => MacosPcmEncoding::SignedInteger,
            (true, true, _) => {
                return Err(CoreError::Audio(format!(
                    "ScreenCaptureKit returned contradictory PCM flags 0x{flags:x}"
                )));
            }
            _ => {
                return Err(CoreError::Audio(format!(
                    "ScreenCaptureKit returned unsupported PCM encoding (flags 0x{flags:x}, {bits_per_channel} bits)"
                )));
            }
        };
        let format = Self {
            sample_rate: rounded_sample_rate as u32,
            channels: description.audio_channel_count().ok_or_else(|| {
                CoreError::Audio("ScreenCaptureKit omitted the PCM channel count".into())
            })?,
            bits_per_channel,
            bytes_per_frame: description.audio_bytes_per_frame().ok_or_else(|| {
                CoreError::Audio("ScreenCaptureKit omitted the PCM bytes-per-frame value".into())
            })?,
            flags,
            encoding,
        };
        format.validate()?;
        Ok(format)
    }

    fn validate(self) -> CoreResult<()> {
        if self.sample_rate != CAPTURE_SAMPLE_RATE {
            return Err(CoreError::Audio(format!(
                "ScreenCaptureKit microphone uses a native {} Hz format, but this recording requires {} Hz; select a 48 kHz input device",
                self.sample_rate, CAPTURE_SAMPLE_RATE
            )));
        }
        if !matches!(self.channels, 1 | 2) {
            return Err(CoreError::Audio(format!(
                "ScreenCaptureKit returned {} PCM channels; only mono and stereo are supported",
                self.channels
            )));
        }
        if self.flags & MACOS_PCM_FLAG_SAMPLE_FRACTION_MASK != 0 {
            return Err(CoreError::Audio(format!(
                "ScreenCaptureKit returned unsupported fixed-point PCM ({self})"
            )));
        }
        let storage_bytes = self.storage_bytes_per_sample()?;
        match self.encoding {
            MacosPcmEncoding::Float32 if storage_bytes != 4 => Err(CoreError::Audio(format!(
                "ScreenCaptureKit returned malformed Float32 PCM ({self})"
            ))),
            MacosPcmEncoding::Float64 if storage_bytes != 8 => Err(CoreError::Audio(format!(
                "ScreenCaptureKit returned malformed Float64 PCM ({self})"
            ))),
            MacosPcmEncoding::SignedInteger
                if storage_bytes > 4 || self.bits_per_channel > storage_bytes.saturating_mul(8) =>
            {
                Err(CoreError::Audio(format!(
                    "ScreenCaptureKit returned unsupported integer PCM sample storage ({self})"
                )))
            }
            _ => Ok(()),
        }
    }

    fn storage_bytes_per_sample(self) -> CoreResult<u32> {
        let bytes = if self.flags & MACOS_PCM_FLAG_IS_NON_INTERLEAVED != 0 {
            self.bytes_per_frame
        } else if self.channels != 0 && self.bytes_per_frame.is_multiple_of(self.channels) {
            self.bytes_per_frame / self.channels
        } else {
            return Err(CoreError::Audio(format!(
                "ScreenCaptureKit returned an invalid PCM frame stride ({self})"
            )));
        };
        if bytes == 0 || bytes > 8 {
            return Err(CoreError::Audio(format!(
                "ScreenCaptureKit returned an unsupported PCM sample stride ({self})"
            )));
        }
        Ok(bytes)
    }
}

#[cfg(target_os = "macos")]
impl fmt::Display for MacosPcmFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} Hz, {} ch, {}, {} bits, {} bytes/frame, flags 0x{:x}",
            self.sample_rate,
            self.channels,
            self.encoding,
            self.bits_per_channel,
            self.bytes_per_frame,
            self.flags
        )
    }
}

#[cfg(target_os = "macos")]
struct ConvertedPcm {
    bytes: Vec<u8>,
    sum_squares: f64,
    nonzero_samples: u64,
}

#[cfg(target_os = "macos")]
fn pcm_audio_buffers_to_s16(
    buffers: &[(u32, &[u8])],
    format: MacosPcmFormat,
    expected_channels: u16,
) -> CoreResult<ConvertedPcm> {
    if buffers.is_empty() || expected_channels == 0 {
        return Err(CoreError::Audio(
            "ScreenCaptureKit returned an empty audio buffer".into(),
        ));
    }
    format.validate()?;
    let storage_bytes = usize::try_from(format.storage_bytes_per_sample()?)
        .map_err(|_| CoreError::Audio("PCM sample stride exceeds this platform".into()))?;
    let mut frame_count = None;
    let mut input_channels = 0_u32;
    for (channels, bytes) in buffers {
        let buffer_channels = usize::try_from(*channels)
            .map_err(|_| CoreError::Audio("PCM channel count exceeds this platform".into()))?;
        let frame_stride = storage_bytes.checked_mul(buffer_channels).ok_or_else(|| {
            CoreError::Audio("ScreenCaptureKit returned an overflowing PCM frame stride".into())
        })?;
        if *channels == 0 || frame_stride == 0 || bytes.len() % frame_stride != 0 {
            return Err(CoreError::Audio(
                "ScreenCaptureKit returned a malformed PCM audio buffer".into(),
            ));
        }
        let frames = bytes.len() / frame_stride;
        if frame_count
            .replace(frames)
            .is_some_and(|known| known != frames)
        {
            return Err(CoreError::Audio(
                "ScreenCaptureKit returned uneven planar audio buffers".into(),
            ));
        }
        input_channels = input_channels.saturating_add(*channels);
    }

    if input_channels != format.channels {
        return Err(CoreError::Audio(format!(
            "ScreenCaptureKit PCM buffers contain {input_channels} channels, but the format describes {}",
            format.channels
        )));
    }

    let expected_channels = u32::from(expected_channels);
    if !matches!(
        (input_channels, expected_channels),
        (1, 1) | (1, 2) | (2, 1) | (2, 2)
    ) {
        return Err(CoreError::Audio(format!(
            "ScreenCaptureKit returned {input_channels} channels; expected {expected_channels}"
        )));
    }

    let frame_count = frame_count.unwrap_or_default();
    let mut pcm = Vec::with_capacity(
        frame_count
            .saturating_mul(expected_channels as usize)
            .saturating_mul(std::mem::size_of::<i16>()),
    );
    let mut sum_squares = 0.0_f64;
    let mut nonzero_samples = 0_u64;
    let mut append = |value: f64| {
        let finite = if value.is_finite() { value } else { 0.0 };
        let sample = (finite.clamp(-1.0, 1.0) * 32_768.0)
            .round()
            .clamp(i16::MIN as f64, i16::MAX as f64) as i16;
        let normalized = sample as f64 / i16::MAX as f64;
        sum_squares += normalized * normalized;
        nonzero_samples += u64::from(sample != 0);
        pcm.extend_from_slice(&sample.to_le_bytes());
    };

    for frame in 0..frame_count {
        let mut input = [0.0_f64; 2];
        let mut input_index = 0;
        for (channels, bytes) in buffers {
            for channel in 0..*channels as usize {
                let offset = (frame * *channels as usize + channel) * storage_bytes;
                input[input_index] =
                    decode_macos_pcm_sample(&bytes[offset..offset + storage_bytes], format);
                input_index += 1;
            }
        }
        match (input_channels, expected_channels) {
            (1, 1) => append(input[0]),
            (1, 2) => {
                append(input[0]);
                append(input[0]);
            }
            (2, 1) => append((input[0] + input[1]) * 0.5),
            (2, 2) => {
                append(input[0]);
                append(input[1]);
            }
            _ => unreachable!("channel layout was validated above"),
        }
    }
    Ok(ConvertedPcm {
        bytes: pcm,
        sum_squares,
        nonzero_samples,
    })
}

#[cfg(target_os = "macos")]
fn decode_macos_pcm_sample(bytes: &[u8], format: MacosPcmFormat) -> f64 {
    let big_endian = format.flags & MACOS_PCM_FLAG_IS_BIG_ENDIAN != 0;
    match format.encoding {
        MacosPcmEncoding::Float32 => {
            let bytes: [u8; 4] = bytes.try_into().expect("validated Float32 stride");
            if big_endian {
                f32::from_be_bytes(bytes) as f64
            } else {
                f32::from_le_bytes(bytes) as f64
            }
        }
        MacosPcmEncoding::Float64 => {
            let bytes: [u8; 8] = bytes.try_into().expect("validated Float64 stride");
            if big_endian {
                f64::from_be_bytes(bytes)
            } else {
                f64::from_le_bytes(bytes)
            }
        }
        MacosPcmEncoding::SignedInteger => {
            let mut encoded = 0_u64;
            if big_endian {
                for byte in bytes {
                    encoded = (encoded << 8) | u64::from(*byte);
                }
            } else {
                for (index, byte) in bytes.iter().enumerate() {
                    encoded |= u64::from(*byte) << (index * 8);
                }
            }

            let storage_bits = (bytes.len() * 8) as u32;
            let sample_bits = format.bits_per_channel;
            if sample_bits < storage_bits {
                if format.flags & MACOS_PCM_FLAG_IS_ALIGNED_HIGH != 0 {
                    encoded >>= storage_bits - sample_bits;
                } else {
                    encoded &= (1_u64 << sample_bits) - 1;
                }
            }
            let sign_bit = 1_u64 << (sample_bits - 1);
            let signed = if encoded & sign_bit == 0 {
                encoded as i64
            } else {
                encoded as i64 - (1_i64 << sample_bits)
            };
            signed as f64 / sign_bit as f64
        }
    }
}

#[cfg(windows)]
fn enumerate_windows() -> CoreResult<AudioDeviceList> {
    use wasapi::{DeviceEnumerator, DeviceState, Direction};

    let com_result = wasapi::initialize_mta();
    if com_result.is_err() {
        return Err(CoreError::Audio(format!(
            "COM initialization failed: {com_result}"
        )));
    }
    let enumerator = DeviceEnumerator::new()
        .map_err(|error| CoreError::Audio(format!("WASAPI enumerator failed: {error}")))?;
    let default_input = enumerator
        .get_default_device(&Direction::Capture)
        .ok()
        .and_then(|device| device.get_id().ok());
    let default_output = enumerator
        .get_default_device(&Direction::Render)
        .ok()
        .and_then(|device| device.get_id().ok());

    let collect = |direction: Direction,
                   kind: &'static str,
                   default_id: &Option<String>|
     -> CoreResult<Vec<AudioDevice>> {
        let collection = enumerator
            .get_device_collection(&direction)
            .map_err(|error| CoreError::Audio(format!("device query failed: {error}")))?;
        let mut devices = Vec::new();
        for device in &collection {
            let device = device
                .map_err(|error| CoreError::Audio(format!("device query failed: {error}")))?;
            let id = device
                .get_id()
                .map_err(|error| CoreError::Audio(format!("device id failed: {error}")))?;
            let name = device
                .get_friendlyname()
                .unwrap_or_else(|_| "Unnamed audio device".into());
            let active = matches!(device.get_state(), Ok(DeviceState::Active));
            devices.push(AudioDevice {
                is_default: default_id.as_deref() == Some(id.as_str()),
                id,
                name,
                kind: kind.into(),
                is_active: active,
            });
        }
        devices.sort_by(|left, right| {
            right
                .is_default
                .cmp(&left.is_default)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        });
        Ok(devices)
    };

    Ok(AudioDeviceList {
        microphones: collect(Direction::Capture, "input", &default_input)?,
        outputs: collect(Direction::Render, "output", &default_output)?,
    })
}

#[cfg(windows)]
fn capture_thread(
    spec: CaptureSpec,
    commands: Receiver<CaptureCommand>,
    caption_sender: Option<SyncSender<CaptionChunk>>,
    shared: Arc<CaptureShared>,
    ready_sender: SyncSender<Result<(), String>>,
) -> CoreResult<CaptureSummary> {
    let result = capture_thread_inner(
        &spec,
        &commands,
        caption_sender.as_ref(),
        &shared,
        &ready_sender,
    );
    match result {
        Ok(summary) => {
            shared.mark_stopped();
            Ok(summary)
        }
        Err(error) => {
            shared.mark_failed(error.to_string());
            let _ = ready_sender.try_send(Err(error.to_string()));
            Err(error)
        }
    }
}

#[cfg(windows)]
fn capture_thread_inner(
    spec: &CaptureSpec,
    commands: &Receiver<CaptureCommand>,
    caption_sender: Option<&SyncSender<CaptionChunk>>,
    shared: &Arc<CaptureShared>,
    ready_sender: &SyncSender<Result<(), String>>,
) -> CoreResult<CaptureSummary> {
    use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

    let com_result = wasapi::initialize_mta();
    if com_result.is_err() {
        return Err(CoreError::Audio(format!(
            "COM initialization failed: {com_result}"
        )));
    }
    let _mmcss = MmcssAudioGuard::register()?;
    let enumerator = DeviceEnumerator::new()
        .map_err(|error| CoreError::Audio(format!("WASAPI enumerator failed: {error}")))?;
    let endpoint_direction = match spec.kind {
        CaptureKind::Microphone => Direction::Capture,
        CaptureKind::Loopback => Direction::Render,
    };
    let device = if let Some(device_id) = &spec.device_id {
        enumerator
            .get_device(device_id)
            .map_err(|error| CoreError::Audio(format!("selected device is unavailable: {error}")))?
    } else {
        enumerator
            .get_default_device(&endpoint_direction)
            .map_err(|error| CoreError::Audio(format!("default device is unavailable: {error}")))?
    };
    let device_id = device
        .get_id()
        .map_err(|error| CoreError::Audio(format!("device id failed: {error}")))?;
    let mut audio_client = device
        .get_iaudioclient()
        .map_err(|error| CoreError::Audio(format!("audio client creation failed: {error}")))?;
    let format = WaveFormat::new(
        16,
        16,
        &SampleType::Int,
        CAPTURE_SAMPLE_RATE as usize,
        spec.channels as usize,
        None,
    );
    let (_, minimum_period) = audio_client
        .get_device_period()
        .map_err(|error| CoreError::Audio(format!("audio period query failed: {error}")))?;
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: minimum_period,
    };
    audio_client
        .initialize_client(&format, &Direction::Capture, &mode)
        .map_err(|error| {
            CoreError::Audio(format!("audio stream initialization failed: {error}"))
        })?;
    let event = audio_client
        .set_get_eventhandle()
        .map_err(|error| CoreError::Audio(format!("audio event creation failed: {error}")))?;
    let capture_client = audio_client
        .get_audiocaptureclient()
        .map_err(|error| CoreError::Audio(format!("capture client creation failed: {error}")))?;
    audio_client
        .start_stream()
        .map_err(|error| CoreError::Audio(format!("audio stream could not start: {error}")))?;
    let (writer_sender, writer_receiver) = std::sync::mpsc::sync_channel(512);
    let writer_directory = spec.directory.clone();
    let writer_channels = spec.channels;
    let writer_thread = thread::Builder::new()
        .name(format!("recording-writer-{}", spec.kind.as_str()))
        .spawn(move || writer_loop(writer_directory, writer_channels, writer_receiver))
        .map_err(CoreError::Io)?;
    shared.mark_live();
    let _ = ready_sender.try_send(Ok(()));

    let mut queue = VecDeque::<u8>::with_capacity(192_000);
    let mut paused = false;
    let mut stop = false;
    let mut caption_sequence = 0_u64;
    let qpc_frequency = spec
        .qpc_frequency
        .or_else(query_performance_frequency)
        .unwrap_or(10_000_000);
    let mut written_frames = 0_u64;
    let mut last_packet_end_qpc = None;
    let mut last_anchor_qpc = None;
    let mut pause_boundary_pending = false;

    while !stop {
        loop {
            match commands.try_recv() {
                Ok(CaptureCommand::Pause) => {
                    paused = true;
                    last_packet_end_qpc = None;
                }
                Ok(CaptureCommand::Resume) => {
                    paused = false;
                    pause_boundary_pending = true;
                    last_packet_end_qpc = None;
                }
                Ok(CaptureCommand::Stop) => {
                    stop = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    stop = true;
                    break;
                }
            }
        }
        if stop {
            break;
        }
        match event.wait_for_event(1000) {
            Ok(()) => {}
            Err(_) => {
                shared.dropped_packets.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        }
        let before = queue.len();
        let info = capture_client
            .read_from_device_to_deque(&mut queue)
            .map_err(|error| CoreError::Audio(format!("audio packet read failed: {error}")))?;
        let new_bytes = queue.len().saturating_sub(before);
        if new_bytes == 0 {
            continue;
        }
        let mut packet = Vec::with_capacity(new_bytes);
        for _ in 0..new_bytes {
            if let Some(byte) = queue.pop_front() {
                packet.push(byte);
            }
        }
        if paused {
            shared.set_level(0.0);
            continue;
        }
        let packet_frames = packet.len() as u64 / (2 * spec.channels as u64);
        if packet_frames == 0 {
            continue;
        }
        if info.flags.data_discontinuity {
            shared.discontinuities.fetch_add(1, Ordering::Relaxed);
        }
        // WASAPI reports the first frame position in 100 ns units. Convert that
        // value to the process-wide QPC tick domain so independent endpoints
        // share one timeline. Fall back only when the endpoint flags its
        // timestamp as invalid.
        let packet_qpc = if !info.flags.timestamp_error && info.timestamp > 0 {
            ((info.timestamp as u128 * qpc_frequency as u128) / 10_000_000_u128) as u64
        } else {
            query_performance_counter().unwrap_or_default()
        };
        let packet_ticks =
            ((packet_frames as u128 * qpc_frequency as u128) / CAPTURE_SAMPLE_RATE as u128) as u64;
        let mut inserted_gap_frames = 0_u64;
        if info.flags.data_discontinuity && !pause_boundary_pending {
            if let Some(previous_end) = last_packet_end_qpc {
                if packet_qpc > previous_end {
                    inserted_gap_frames = (((packet_qpc - previous_end) as u128
                        * CAPTURE_SAMPLE_RATE as u128)
                        / qpc_frequency as u128) as u64;
                    // Ignore sub-packet timestamp jitter. A real WASAPI
                    // discontinuity is preserved as silence in bounded chunks
                    // so the recoverable PCM itself retains the clock gap.
                    if inserted_gap_frames >= CAPTURE_SAMPLE_RATE as u64 / 200 {
                        let samples_per_chunk =
                            CAPTURE_SAMPLE_RATE as usize * spec.channels as usize;
                        let mut remaining_samples =
                            inserted_gap_frames.saturating_mul(spec.channels as u64);
                        while remaining_samples > 0 {
                            let samples = remaining_samples.min(samples_per_chunk as u64) as usize;
                            send_writer_packet(&writer_sender, vec![0_u8; samples * 2])?;
                            remaining_samples -= samples as u64;
                        }
                        written_frames = written_frames.saturating_add(inserted_gap_frames);
                        shared.samples_written.fetch_add(
                            inserted_gap_frames.saturating_mul(spec.channels as u64),
                            Ordering::Relaxed,
                        );
                    } else {
                        inserted_gap_frames = 0;
                    }
                }
            }
        }
        if shared.qpc_first.load(Ordering::Relaxed) == 0 {
            shared.qpc_first.store(packet_qpc, Ordering::Relaxed);
        }
        let packet_end_qpc = packet_qpc.saturating_add(packet_ticks);
        shared.qpc_last.store(packet_end_qpc, Ordering::Relaxed);
        let should_anchor = last_anchor_qpc.is_none()
            || info.flags.data_discontinuity
            || pause_boundary_pending
            || packet_qpc.saturating_sub(last_anchor_qpc.unwrap_or(packet_qpc))
                >= qpc_frequency.saturating_mul(5);
        if should_anchor {
            shared.push_clock_anchor(ClockAnchor {
                qpc: packet_qpc,
                frame_index: written_frames,
                discontinuity: info.flags.data_discontinuity,
                pause_boundary: pause_boundary_pending,
                inserted_gap_frames,
            });
            last_anchor_qpc = Some(packet_qpc);
        }
        pause_boundary_pending = false;

        let mut sum_squares = 0.0_f64;
        let mut count = 0_u64;
        for bytes in packet.chunks_exact(2) {
            let sample = i16::from_le_bytes([bytes[0], bytes[1]]);
            let normalized = sample as f64 / i16::MAX as f64;
            sum_squares += normalized * normalized;
            count += 1;
        }
        let rms = if count == 0 {
            0.0
        } else {
            (sum_squares / count as f64).sqrt() as f32
        };
        shared.set_level((rms * 3.0).clamp(0.0, 1.0));

        let caption_packet = spec.live_captions.then(|| packet.clone());
        send_writer_packet(&writer_sender, packet)?;
        written_frames = written_frames.saturating_add(packet_frames);
        shared.samples_written.fetch_add(
            packet_frames.saturating_mul(spec.channels as u64),
            Ordering::Relaxed,
        );
        last_packet_end_qpc = Some(packet_end_qpc);

        if let (Some(sender), Some(pcm_s16le)) = (caption_sender, caption_packet) {
            let chunk = CaptionChunk {
                session_id: spec.session_id.clone(),
                stream_id: spec.kind.as_str().into(),
                sequence: caption_sequence,
                start_ms: spec
                    .session_qpc_start
                    .filter(|start| packet_qpc >= *start)
                    .map(|start| {
                        (((packet_qpc - start) as u128 * 1000_u128) / qpc_frequency as u128) as u64
                    })
                    .unwrap_or_default(),
                sample_rate: CAPTURE_SAMPLE_RATE,
                channels: spec.channels,
                pcm_s16le,
            };
            match sender.try_send(chunk) {
                Ok(()) => caption_sequence += 1,
                Err(TrySendError::Full(_)) => {
                    shared
                        .dropped_caption_chunks
                        .fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => {}
            }
        }
    }
    let _ = audio_client.stop_stream();
    writer_sender
        .send(WriterMessage::Stop)
        .map_err(|_| CoreError::Audio("recording writer stopped before finalization".into()))?;
    let writer_summary = writer_thread
        .join()
        .map_err(|_| CoreError::Audio("recording writer thread panicked".into()))??;
    shared
        .samples_written
        .store(writer_summary.samples_written, Ordering::Relaxed);
    if let Some(qpc_last) = nonzero(shared.qpc_last.load(Ordering::Relaxed)) {
        shared.push_clock_anchor(ClockAnchor {
            qpc: qpc_last,
            frame_index: writer_summary.samples_written / spec.channels as u64,
            discontinuity: false,
            pause_boundary: false,
            inserted_gap_frames: 0,
        });
    }
    shared.set_level(0.0);
    let qpc_first = nonzero(shared.qpc_first.load(Ordering::Relaxed));
    let qpc_last = nonzero(shared.qpc_last.load(Ordering::Relaxed));
    Ok(CaptureSummary {
        kind: spec.kind,
        device_id,
        channels: spec.channels,
        segment_paths: writer_summary.segment_paths,
        samples_written: writer_summary.samples_written,
        dropped_packets: shared.dropped_packets.load(Ordering::Relaxed),
        discontinuities: shared.discontinuities.load(Ordering::Relaxed),
        qpc_first,
        qpc_last,
        clock_anchors: shared.clock_anchors(),
    })
}

fn send_writer_packet(sender: &SyncSender<WriterMessage>, packet: Vec<u8>) -> CoreResult<()> {
    match sender.try_send(WriterMessage::Packet(packet)) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => Err(CoreError::Audio(
            "CAPTURE_WRITER_BACKPRESSURE: disk writer could not keep up; recording stopped before audio could be lost".into(),
        )),
        Err(TrySendError::Disconnected(_)) => Err(CoreError::Audio(
            "recording writer exited unexpectedly".into(),
        )),
    }
}

enum WriterMessage {
    Packet(Vec<u8>),
    Stop,
}

struct WriterSummary {
    segment_paths: Vec<PathBuf>,
    samples_written: u64,
}

fn writer_loop(
    directory: PathBuf,
    channels: u16,
    receiver: Receiver<WriterMessage>,
) -> CoreResult<WriterSummary> {
    use hound::{SampleFormat, WavSpec, WavWriter};

    let wav_spec = WavSpec {
        channels,
        sample_rate: CAPTURE_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let segment_sample_limit = CAPTURE_SAMPLE_RATE as u64 * channels as u64 * SEGMENT_SECONDS;
    let mut segment_index = 0_u32;
    let mut segment_samples = 0_u64;
    let mut total_samples = 0_u64;
    let mut writer = None;
    let mut current_partial = None;
    let mut segments = Vec::new();
    let mut last_flush = Instant::now();
    while let Ok(message) = receiver.recv() {
        match message {
            WriterMessage::Packet(packet) => {
                if writer.is_none() {
                    segment_index += 1;
                    let final_path = directory.join(format!("segment-{segment_index:05}.wav"));
                    let partial =
                        PathBuf::from(format!("{}.partial", final_path.to_string_lossy()));
                    writer = Some(WavWriter::create(&partial, wav_spec).map_err(|error| {
                        CoreError::Audio(format!("recording segment create failed: {error}"))
                    })?);
                    current_partial = Some((partial, final_path));
                    segment_samples = 0;
                }
                if let Some(output) = writer.as_mut() {
                    let packet_samples = u32::try_from(packet.len() / 2).map_err(|_| {
                        CoreError::Audio("recording packet exceeds WAV writer limits".into())
                    })?;
                    let mut sample_writer = output.get_i16_writer(packet_samples);
                    for bytes in packet.chunks_exact(2) {
                        sample_writer.write_sample(i16::from_le_bytes([bytes[0], bytes[1]]));
                    }
                    sample_writer.flush().map_err(|error| {
                        CoreError::Audio(format!("recording segment write failed: {error}"))
                    })?;
                    segment_samples += u64::from(packet_samples);
                    total_samples += u64::from(packet_samples);
                }
                if last_flush.elapsed() >= Duration::from_secs(1) {
                    if let Some(output) = writer.as_mut() {
                        output.flush().map_err(|error| {
                            CoreError::Audio(format!(
                                "recording segment checkpoint failed: {error}"
                            ))
                        })?;
                    }
                    last_flush = Instant::now();
                }
                if segment_samples >= segment_sample_limit {
                    finalize_segment(&mut writer, &mut current_partial, &mut segments)?;
                }
            }
            WriterMessage::Stop => break,
        }
    }
    finalize_segment(&mut writer, &mut current_partial, &mut segments)?;
    Ok(WriterSummary {
        segment_paths: segments,
        samples_written: total_samples,
    })
}

fn finalize_segment(
    writer: &mut Option<hound::WavWriter<std::io::BufWriter<fs::File>>>,
    current: &mut Option<(PathBuf, PathBuf)>,
    segments: &mut Vec<PathBuf>,
) -> CoreResult<()> {
    if let Some(output) = writer.take() {
        output
            .finalize()
            .map_err(|error| CoreError::Audio(format!("segment finalize failed: {error}")))?;
    }
    if let Some((partial, final_path)) = current.take() {
        fs::rename(partial, &final_path)?;
        segments.push(final_path);
    }
    Ok(())
}

#[cfg(windows)]
struct MmcssAudioGuard(*mut std::ffi::c_void);

#[cfg(windows)]
unsafe impl Send for MmcssAudioGuard {}

#[cfg(windows)]
impl MmcssAudioGuard {
    fn register() -> CoreResult<Self> {
        #[link(name = "avrt")]
        extern "system" {
            fn AvSetMmThreadCharacteristicsW(
                task_name: *const u16,
                task_index: *mut u32,
            ) -> *mut std::ffi::c_void;
        }
        let name = "Audio\0".encode_utf16().collect::<Vec<_>>();
        let mut task_index = 0_u32;
        let handle = unsafe { AvSetMmThreadCharacteristicsW(name.as_ptr(), &mut task_index) };
        if handle.is_null() {
            return Err(CoreError::Audio(
                "could not register capture thread with MMCSS Audio profile".into(),
            ));
        }
        Ok(Self(handle))
    }
}

#[cfg(windows)]
impl Drop for MmcssAudioGuard {
    fn drop(&mut self) {
        #[link(name = "avrt")]
        extern "system" {
            fn AvRevertMmThreadCharacteristics(handle: *mut std::ffi::c_void) -> i32;
        }
        let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn capture_thread(
    _spec: CaptureSpec,
    _commands: Receiver<CaptureCommand>,
    _caption_sender: Option<SyncSender<CaptionChunk>>,
    shared: Arc<CaptureShared>,
    ready_sender: SyncSender<Result<(), String>>,
) -> CoreResult<CaptureSummary> {
    let message = "audio recording is not available on this platform".to_string();
    shared.mark_failed(message.clone());
    let _ = ready_sender.try_send(Err(message.clone()));
    Err(CoreError::Audio(message))
}

fn nonzero(value: u64) -> Option<u64> {
    (value != 0).then_some(value)
}

#[cfg(windows)]
pub fn query_performance_counter() -> Option<u64> {
    #[link(name = "kernel32")]
    extern "system" {
        fn QueryPerformanceCounter(value: *mut i64) -> i32;
    }
    let mut value = 0_i64;
    let succeeded = unsafe { QueryPerformanceCounter(&mut value) };
    (succeeded != 0 && value >= 0).then_some(value as u64)
}

#[cfg(target_os = "macos")]
pub fn query_performance_counter() -> Option<u64> {
    Some(monotonic_nanoseconds())
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn query_performance_counter() -> Option<u64> {
    None
}

#[cfg(windows)]
pub fn query_performance_frequency() -> Option<u64> {
    #[link(name = "kernel32")]
    extern "system" {
        fn QueryPerformanceFrequency(value: *mut i64) -> i32;
    }
    let mut value = 0_i64;
    let succeeded = unsafe { QueryPerformanceFrequency(&mut value) };
    (succeeded != 0 && value > 0).then_some(value as u64)
}

#[cfg(target_os = "macos")]
pub fn query_performance_frequency() -> Option<u64> {
    Some(1_000_000_000)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn query_performance_frequency() -> Option<u64> {
    None
}

#[cfg(target_os = "macos")]
fn mach_timebase_ratio() -> (u64, u64) {
    #[repr(C)]
    struct MachTimebaseInfo {
        numerator: u32,
        denominator: u32,
    }
    unsafe extern "C" {
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
    }

    static TIMEBASE: OnceLock<(u64, u64)> = OnceLock::new();
    *TIMEBASE.get_or_init(|| {
        let mut info = MachTimebaseInfo {
            numerator: 0,
            denominator: 0,
        };
        let status = unsafe { mach_timebase_info(&mut info) };
        if status == 0 && info.numerator > 0 && info.denominator > 0 {
            (u64::from(info.numerator), u64::from(info.denominator))
        } else {
            (1, 1)
        }
    })
}

#[cfg(target_os = "macos")]
fn mach_ticks_to_nanoseconds(ticks: u64) -> u64 {
    let (numerator, denominator) = mach_timebase_ratio();
    (ticks as u128 * numerator as u128 / denominator as u128).min(u64::MAX as u128) as u64
}

#[cfg(target_os = "macos")]
fn monotonic_nanoseconds() -> u64 {
    unsafe extern "C" {
        fn mach_absolute_time() -> u64;
    }
    let ticks = unsafe { mach_absolute_time() };
    mach_ticks_to_nanoseconds(ticks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    fn test_pcm_format(
        encoding: MacosPcmEncoding,
        channels: u32,
        bits_per_channel: u32,
        storage_bytes: u32,
        flags: u32,
    ) -> MacosPcmFormat {
        let bytes_per_frame = if flags & MACOS_PCM_FLAG_IS_NON_INTERLEAVED != 0 {
            storage_bytes
        } else {
            storage_bytes * channels
        };
        MacosPcmFormat {
            sample_rate: CAPTURE_SAMPLE_RATE,
            channels,
            bits_per_channel,
            bytes_per_frame,
            flags,
            encoding,
        }
    }

    #[test]
    fn shared_levels_are_atomic_and_clamped() {
        let shared = CaptureShared::default();
        shared.set_level(3.0);
        assert_eq!(shared.level(), 1.0);
        shared.set_level(-1.0);
        assert_eq!(shared.level(), 0.0);
    }

    #[test]
    fn shared_liveness_exposes_terminal_capture_error() {
        let shared = CaptureShared::default();
        assert!(!shared.is_live());
        assert!(shared.mark_live());
        assert!(shared.is_live());
        shared.mark_failed("USB microphone disconnected");
        assert!(!shared.is_live());
        assert!(!shared.mark_live());
        assert!(!shared.is_live());
        assert_eq!(
            shared.failure().as_deref(),
            Some("USB microphone disconnected")
        );
        // A later cleanup transition must not erase the actionable failure.
        shared.mark_stopped();
        assert_eq!(
            shared.failure().as_deref(),
            Some("USB microphone disconnected")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn screen_capture_planar_audio_is_interleaved_before_writing() {
        let left = [0.25_f32, 0.5_f32]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();
        let right = [-0.25_f32, -0.5_f32]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();

        let format = test_pcm_format(
            MacosPcmEncoding::Float32,
            2,
            32,
            4,
            MACOS_PCM_FLAG_IS_FLOAT | MACOS_PCM_FLAG_IS_NON_INTERLEAVED,
        );
        let pcm = pcm_audio_buffers_to_s16(&[(1, &left), (1, &right)], format, 2).unwrap();
        let samples = pcm
            .bytes
            .chunks_exact(2)
            .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]))
            .collect::<Vec<_>>();

        assert_eq!(samples, [8_192, -8_192, 16_384, -16_384]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn screen_capture_downmixes_stereo_microphone_audio() {
        let stereo = [0.5_f32, 0.25_f32, -0.5_f32, -0.25_f32]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();

        let format = test_pcm_format(MacosPcmEncoding::Float32, 2, 32, 4, MACOS_PCM_FLAG_IS_FLOAT);
        let pcm = pcm_audio_buffers_to_s16(&[(2, &stereo)], format, 1).unwrap();
        let samples = pcm
            .bytes
            .chunks_exact(2)
            .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]))
            .collect::<Vec<_>>();

        assert_eq!(samples, [12_288, -12_288]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn screen_capture_duplicates_mono_audio_for_stereo_output() {
        let mono = [0.25_f32, -0.5_f32]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();

        let format = test_pcm_format(MacosPcmEncoding::Float32, 1, 32, 4, MACOS_PCM_FLAG_IS_FLOAT);
        let pcm = pcm_audio_buffers_to_s16(&[(1, &mono)], format, 2).unwrap();
        let samples = pcm
            .bytes
            .chunks_exact(2)
            .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]))
            .collect::<Vec<_>>();

        assert_eq!(samples, [8_192, 8_192, -16_384, -16_384]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn screen_capture_rejects_unsupported_channel_layout() {
        let surround = [0.25_f32, 0.25_f32, 0.25_f32]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();

        let format = test_pcm_format(MacosPcmEncoding::Float32, 2, 32, 4, MACOS_PCM_FLAG_IS_FLOAT);
        assert!(pcm_audio_buffers_to_s16(&[(3, &surround)], format, 2).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn screen_capture_decodes_native_signed_integer_audio() {
        let integer = [0x4000_0000_i32, -0x4000_0000_i32]
            .into_iter()
            .flat_map(i32::to_le_bytes)
            .collect::<Vec<_>>();
        let format = test_pcm_format(
            MacosPcmEncoding::SignedInteger,
            1,
            32,
            4,
            MACOS_PCM_FLAG_IS_SIGNED_INTEGER,
        );

        let pcm = pcm_audio_buffers_to_s16(&[(1, &integer)], format, 1).unwrap();
        let samples = pcm
            .bytes
            .chunks_exact(2)
            .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]))
            .collect::<Vec<_>>();

        assert_eq!(samples, [16_384, -16_384]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn screen_capture_decodes_high_aligned_24_bit_audio() {
        let integer = [0x4000_0000_i32, -0x4000_0000_i32]
            .into_iter()
            .flat_map(i32::to_le_bytes)
            .collect::<Vec<_>>();
        let format = test_pcm_format(
            MacosPcmEncoding::SignedInteger,
            1,
            24,
            4,
            MACOS_PCM_FLAG_IS_SIGNED_INTEGER | MACOS_PCM_FLAG_IS_ALIGNED_HIGH,
        );

        let pcm = pcm_audio_buffers_to_s16(&[(1, &integer)], format, 1).unwrap();
        let samples = pcm
            .bytes
            .chunks_exact(2)
            .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]))
            .collect::<Vec<_>>();

        assert_eq!(samples, [16_384, -16_384]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn screen_capture_decodes_big_endian_float64_audio() {
        let float = [0.25_f64, -0.5_f64]
            .into_iter()
            .flat_map(f64::to_be_bytes)
            .collect::<Vec<_>>();
        let format = test_pcm_format(
            MacosPcmEncoding::Float64,
            1,
            64,
            8,
            MACOS_PCM_FLAG_IS_FLOAT | MACOS_PCM_FLAG_IS_BIG_ENDIAN,
        );

        let pcm = pcm_audio_buffers_to_s16(&[(1, &float)], format, 1).unwrap();
        let samples = pcm
            .bytes
            .chunks_exact(2)
            .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]))
            .collect::<Vec<_>>();

        assert_eq!(samples, [8_192, -16_384]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn screen_capture_rejects_malformed_pcm_frame_stride() {
        let format = MacosPcmFormat {
            sample_rate: CAPTURE_SAMPLE_RATE,
            channels: 2,
            bits_per_channel: 32,
            bytes_per_frame: 7,
            flags: MACOS_PCM_FLAG_IS_FLOAT,
            encoding: MacosPcmEncoding::Float32,
        };

        assert!(format.validate().is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_capture_group_requires_unique_supported_sources() {
        assert!(validate_macos_capture_kinds([CaptureKind::Microphone]).is_ok());
        assert!(validate_macos_capture_kinds([CaptureKind::Loopback]).is_ok());
        assert!(
            validate_macos_capture_kinds([CaptureKind::Microphone, CaptureKind::Loopback,]).is_ok()
        );

        assert!(validate_macos_capture_kinds([]).is_err());
        assert!(
            validate_macos_capture_kinds([CaptureKind::Microphone, CaptureKind::Microphone,])
                .is_err()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn screen_capture_rejects_non_numeric_core_media_times() {
        use screencapturekit::cm::CMTime;

        assert!(is_numeric_cmtime(CMTime::new(1, 1_000_000_000)));
        assert!(!is_numeric_cmtime(CMTime::INVALID));
        assert!(!is_numeric_cmtime(CMTime {
            value: 0,
            timescale: 1,
            flags: (1 << 0) | (1 << 4),
            epoch: 0,
        }));
        assert!(!is_numeric_cmtime(CMTime::new(-1, 1_000_000_000)));
        assert!(!is_numeric_cmtime(CMTime {
            value: 1,
            timescale: 1_000_000_000,
            flags: 1,
            epoch: 1,
        }));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn screen_capture_converts_host_time_without_a_float_round_trip() {
        use screencapturekit::cm::{CMClock, CMTime};

        let converter = MacosTimestampConverter::new(CMClock::host_time_clock());
        let expected_ns = 123_456_789_u64;
        let converted_ns = converter
            .to_host_nanoseconds(CMTime::new(expected_ns as i64, 1_000_000_000))
            .expect("host time should convert to host nanoseconds");

        assert!(converted_ns.abs_diff(expected_ns) <= 1_000);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_host_clock_uses_a_stable_nanosecond_domain() {
        let first = query_performance_counter().unwrap();
        thread::sleep(Duration::from_millis(1));
        let second = query_performance_counter().unwrap();

        assert!(first > 0);
        assert!(second > first);
        assert_eq!(query_performance_frequency(), Some(1_000_000_000));
    }
}
