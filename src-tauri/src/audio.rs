use std::{
    collections::VecDeque,
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

    pub fn mark_live(&self) {
        self.liveness.store(1, Ordering::Release);
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
    #[cfg(not(windows))]
    {
        Err(CoreError::Audio(
            "WASAPI device enumeration is available only on Windows".into(),
        ))
    }
}

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

        send_writer_packet(&writer_sender, packet.clone())?;
        written_frames = written_frames.saturating_add(packet_frames);
        shared.samples_written.fetch_add(
            packet_frames.saturating_mul(spec.channels as u64),
            Ordering::Relaxed,
        );
        last_packet_end_qpc = Some(packet_end_qpc);

        if spec.live_captions {
            if let Some(sender) = caption_sender {
                let chunk = CaptionChunk {
                    session_id: spec.session_id.clone(),
                    stream_id: spec.kind.as_str().into(),
                    sequence: caption_sequence,
                    start_ms: spec
                        .session_qpc_start
                        .filter(|start| packet_qpc >= *start)
                        .map(|start| {
                            (((packet_qpc - start) as u128 * 1000_u128) / qpc_frequency as u128)
                                as u64
                        })
                        .unwrap_or_default(),
                    sample_rate: CAPTURE_SAMPLE_RATE,
                    channels: spec.channels,
                    pcm_s16le: packet,
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

#[cfg(windows)]
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

#[cfg(windows)]
enum WriterMessage {
    Packet(Vec<u8>),
    Stop,
}

#[cfg(windows)]
struct WriterSummary {
    segment_paths: Vec<PathBuf>,
    samples_written: u64,
}

#[cfg(windows)]
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
                    for bytes in packet.chunks_exact(2) {
                        output
                            .write_sample(i16::from_le_bytes([bytes[0], bytes[1]]))
                            .map_err(|error| {
                                CoreError::Audio(format!("recording segment write failed: {error}"))
                            })?;
                        segment_samples += 1;
                        total_samples += 1;
                    }
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

#[cfg(windows)]
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

#[cfg(not(windows))]
fn capture_thread(
    _spec: CaptureSpec,
    _commands: Receiver<CaptureCommand>,
    _caption_sender: Option<SyncSender<CaptionChunk>>,
    shared: Arc<CaptureShared>,
    ready_sender: SyncSender<Result<(), String>>,
) -> CoreResult<CaptureSummary> {
    let message = "WASAPI recording is available only on Windows".to_string();
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

#[cfg(not(windows))]
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

#[cfg(not(windows))]
pub fn query_performance_frequency() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
        shared.mark_live();
        assert!(shared.is_live());
        shared.mark_failed("USB microphone disconnected");
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
}
