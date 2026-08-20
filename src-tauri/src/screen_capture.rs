use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(target_os = "macos")]
use std::sync::mpsc::TryRecvError;

#[cfg(not(target_os = "macos"))]
use std::{
    ffi::OsString,
    io::{BufRead, BufReader, Write},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

use parking_lot::Mutex;
use serde::Serialize;

use crate::{
    audio,
    error::{CoreError, CoreResult},
    layout::AppLayout,
    media_tools::{self, MediaTool},
};

pub(crate) const SCREEN_FRAMES_PER_SECOND: u32 = 5;
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(8);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_GRACE_PERIOD: Duration = Duration::from_secs(5);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(25);
#[cfg(target_os = "macos")]
const STOP_ACK_HANDOFF_TIMEOUT: Duration = Duration::from_millis(250);
#[cfg(target_os = "macos")]
const SANITIZE_TIMEOUT: Duration = Duration::from_secs(7);
#[cfg(target_os = "macos")]
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(not(target_os = "macos"))]
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(200);
#[cfg(not(target_os = "macos"))]
const STDERR_LOG_LIMIT: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandSendError {
    Timeout,
    Disconnected,
}

fn try_send_until<T>(
    sender: &SyncSender<T>,
    mut value: T,
    deadline: Instant,
) -> Result<(), CommandSendError> {
    loop {
        match sender.try_send(value) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(returned)) => value = returned,
            Err(TrySendError::Disconnected(_)) => return Err(CommandSendError::Disconnected),
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(CommandSendError::Timeout);
        }
        thread::sleep(PROCESS_POLL_INTERVAL.min(remaining));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    Starting,
    Capturing,
    Paused,
    Stopped,
    Failed,
}

#[derive(Debug)]
struct SharedState {
    lifecycle: Lifecycle,
    failure: Option<String>,
}

#[derive(Debug)]
pub(crate) struct ScreenCaptureShared {
    state: Mutex<SharedState>,
}

impl Default for ScreenCaptureShared {
    fn default() -> Self {
        Self {
            state: Mutex::new(SharedState {
                lifecycle: Lifecycle::Starting,
                failure: None,
            }),
        }
    }
}

impl ScreenCaptureShared {
    pub(crate) fn is_active(&self) -> bool {
        self.state.lock().lifecycle == Lifecycle::Capturing
    }

    pub(crate) fn failure(&self) -> Option<String> {
        self.state.lock().failure.clone()
    }

    fn transition(&self, lifecycle: Lifecycle) {
        self.state.lock().lifecycle = lifecycle;
    }

    fn fail(&self, message: impl Into<String>) {
        let mut state = self.state.lock();
        state.lifecycle = Lifecycle::Failed;
        state.failure = Some(message.into());
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScreenSegmentSummary {
    pub sequence: u32,
    pub path: PathBuf,
    pub timeline_start_ms: i64,
    pub duration_ms: i64,
    pub frame_count: u64,
    pub qpc_started: Option<u64>,
    pub qpc_first: Option<u64>,
    pub qpc_last: Option<u64>,
    pub stderr_log: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScreenCaptureSummary {
    pub segments: Vec<ScreenSegmentSummary>,
    pub warning: Option<String>,
}

impl ScreenCaptureSummary {
    pub(crate) fn duration_ms(&self) -> i64 {
        self.segments
            .iter()
            .map(|segment| segment.timeline_start_ms + segment.duration_ms)
            .max()
            .unwrap_or(0)
    }

    pub(crate) fn start_offset_ms(&self) -> i64 {
        self.segments
            .first()
            .map(|segment| segment.timeline_start_ms)
            .unwrap_or(0)
    }

    fn warn(&mut self, message: impl Into<String>) {
        let message = message.into();
        match self.warning.as_mut() {
            Some(existing) => {
                existing.push_str("; ");
                existing.push_str(&message);
            }
            None => self.warning = Some(message),
        }
    }

    fn warn_once(&mut self, message: impl Into<String>) {
        let message = message.into();
        let already_present = self
            .warning
            .as_deref()
            .is_some_and(|warning| warning.split("; ").any(|item| item == message));
        if !already_present {
            self.warn(message);
        }
    }
}

pub(crate) enum ScreenCommand {
    Pause(mpsc::Sender<Result<(), String>>),
    Resume(mpsc::Sender<Result<(), String>>),
    Stop(mpsc::Sender<Result<(), String>>),
}

struct PendingScreenStop {
    response: Receiver<Result<(), String>>,
    sent: bool,
}

pub(crate) struct ScreenCapture {
    command_sender: SyncSender<ScreenCommand>,
    handle: Option<thread::JoinHandle<ScreenCaptureSummary>>,
    #[cfg(target_os = "macos")]
    result_receiver: Option<Receiver<Result<ScreenCaptureSummary, String>>>,
    pending_stop: Option<PendingScreenStop>,
    shared: Arc<ScreenCaptureShared>,
}

impl ScreenCapture {
    pub(crate) fn shared(&self) -> Arc<ScreenCaptureShared> {
        self.shared.clone()
    }

    pub(crate) fn pause(&self) -> CoreResult<()> {
        self.control(ScreenCommand::Pause)
    }

    pub(crate) fn resume(&self) -> CoreResult<()> {
        self.control(ScreenCommand::Resume)
    }

    pub(crate) fn stop(&mut self) -> CoreResult<ScreenCaptureSummary> {
        self.request_stop()?;
        self.finish_stop()
    }

    /// Enqueues screen shutdown before the shared macOS coordinator is told to
    /// stop its audio tracks. This split phase closes the race where the
    /// coordinator could otherwise exit before observing the screen command.
    pub(crate) fn request_stop(&mut self) -> CoreResult<()> {
        if self.pending_stop.is_some() {
            return Ok(());
        }
        if !self.is_running() {
            return Err(CoreError::Conflict(
                "screen capture has already been stopped".into(),
            ));
        }
        let (acknowledge, response) = mpsc::channel();
        let sent = match self
            .command_sender
            .try_send(ScreenCommand::Stop(acknowledge))
        {
            Ok(()) => true,
            Err(TrySendError::Disconnected(_)) => false,
            Err(TrySendError::Full(_)) => {
                return Err(CoreError::Media(
                    "screen capture stop command queue is busy".into(),
                ));
            }
        };
        self.pending_stop = Some(PendingScreenStop { response, sent });
        Ok(())
    }

    pub(crate) fn finish_stop(&mut self) -> CoreResult<ScreenCaptureSummary> {
        if self.pending_stop.is_none() {
            self.request_stop()?;
        }

        #[cfg(target_os = "macos")]
        if self.handle.is_none() {
            return self.stop_coordinated_macos();
        }

        self.stop_thread_backed()
    }

    fn stop_thread_backed(&mut self) -> CoreResult<ScreenCaptureSummary> {
        let deadline = Instant::now() + CONTROL_TIMEOUT;
        let PendingScreenStop { response, sent } = self
            .pending_stop
            .take()
            .expect("screen stop request checked above");
        let acknowledgement = if sent {
            response
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .ok()
        } else {
            None
        };

        let handle = self
            .handle
            .take()
            .expect("thread-backed screen capture handle checked above");
        let mut summary = handle
            .join()
            .map_err(|_| CoreError::Media("screen capture thread panicked".into()))?;

        match acknowledgement {
            Some(Ok(())) => {}
            Some(Err(message)) => summary.warn(message),
            None if sent => summary.warn("screen capture stop acknowledgement timed out"),
            None => {}
        }
        Ok(summary)
    }

    #[cfg(target_os = "macos")]
    fn stop_coordinated_macos(&mut self) -> CoreResult<ScreenCaptureSummary> {
        let result_receiver = self
            .result_receiver
            .take()
            .expect("coordinated screen result checked above");
        let deadline = Instant::now() + CONTROL_TIMEOUT;
        let PendingScreenStop { response, sent } = self
            .pending_stop
            .take()
            .expect("screen stop request checked above");
        let mut acknowledgement = None;
        let mut acknowledgement_disconnected = false;
        let mut summary = None;
        let mut acknowledgement_handoff_deadline = None;

        loop {
            if sent && acknowledgement.is_none() && !acknowledgement_disconnected {
                match response.try_recv() {
                    Ok(result) => acknowledgement = Some(result),
                    Err(TryRecvError::Disconnected) => acknowledgement_disconnected = true,
                    Err(TryRecvError::Empty) => {}
                }
            }

            if summary.is_none() {
                match result_receiver.try_recv() {
                    Ok(Ok(received)) => {
                        summary = Some(received);
                        acknowledgement_handoff_deadline =
                            Some((Instant::now() + STOP_ACK_HANDOFF_TIMEOUT).min(deadline));
                    }
                    Ok(Err(message)) => return Err(CoreError::Media(message)),
                    Err(TryRecvError::Disconnected) => {
                        let message = match acknowledgement.as_ref() {
                            Some(Err(acknowledgement)) => format!(
                                "{acknowledgement}; coordinated screen capture ended without a result"
                            ),
                            _ => "coordinated screen capture ended without a result".into(),
                        };
                        return Err(CoreError::Media(message));
                    }
                    Err(TryRecvError::Empty) => {}
                }
            }

            if let Some(mut completed) = summary.take() {
                match acknowledgement.take() {
                    Some(Ok(())) => return Ok(completed),
                    Some(Err(message)) => {
                        completed.warn_once(message);
                        return Ok(completed);
                    }
                    None if !sent => return Ok(completed),
                    None if acknowledgement_disconnected => {
                        completed
                            .warn_once("screen capture stopped without a control acknowledgement");
                        return Ok(completed);
                    }
                    None => {
                        let handoff_deadline = acknowledgement_handoff_deadline
                            .expect("summary establishes acknowledgement handoff deadline");
                        if Instant::now() >= handoff_deadline {
                            completed.warn_once(
                                "screen capture stop result arrived without its control acknowledgement",
                            );
                            return Ok(completed);
                        }
                        summary = Some(completed);
                    }
                }
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let message = match acknowledgement.as_ref() {
                    Some(Err(acknowledgement)) => {
                        format!("{acknowledgement}; coordinated screen capture result timed out")
                    }
                    _ => "coordinated screen capture result timed out".into(),
                };
                self.shared.fail(message.clone());
                return Err(CoreError::Media(message));
            }
            let acknowledgement_remaining = acknowledgement_handoff_deadline
                .map(|handoff| handoff.saturating_duration_since(Instant::now()))
                .unwrap_or(remaining);
            thread::sleep(
                PROCESS_POLL_INTERVAL
                    .min(remaining)
                    .min(acknowledgement_remaining),
            );
        }
    }

    fn is_running(&self) -> bool {
        if self.handle.is_some() {
            return true;
        }
        #[cfg(target_os = "macos")]
        {
            self.result_receiver.is_some()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }

    fn control(
        &self,
        command: fn(mpsc::Sender<Result<(), String>>) -> ScreenCommand,
    ) -> CoreResult<()> {
        if self.pending_stop.is_some() {
            return Err(CoreError::Conflict(
                "screen capture stop has already been requested".into(),
            ));
        }
        let deadline = Instant::now() + CONTROL_TIMEOUT;
        let (acknowledge, response) = mpsc::channel();
        match try_send_until(&self.command_sender, command(acknowledge), deadline) {
            Ok(()) => {}
            Err(CommandSendError::Timeout) => {
                return Err(CoreError::Media(
                    "screen capture control command queue timed out".into(),
                ));
            }
            Err(CommandSendError::Disconnected) => {
                return Err(CoreError::Media(
                    "screen capture thread is not running".into(),
                ));
            }
        }
        match response.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => Err(CoreError::Media(message)),
            Err(RecvTimeoutError::Timeout) => Err(CoreError::Media(
                "screen capture control acknowledgement timed out".into(),
            )),
            Err(RecvTimeoutError::Disconnected) => Err(CoreError::Media(
                "screen capture thread ended before acknowledging control".into(),
            )),
        }
    }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        if self.is_running() {
            let _ = self.stop();
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn spawn(
    layout: &AppLayout,
    directory: PathBuf,
    session_qpc_start: Option<u64>,
    qpc_frequency: Option<u64>,
) -> CoreResult<ScreenCapture> {
    if !cfg!(windows) {
        return Err(CoreError::Media(
            "screen capture is currently supported only on Windows".into(),
        ));
    }

    fs::create_dir_all(&directory)?;
    let ffmpeg = media_tools::resolve(layout, MediaTool::Ffmpeg)?;
    let (command_sender, command_receiver) = mpsc::sync_channel(4);
    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    let shared = Arc::new(ScreenCaptureShared::default());
    let thread_shared = shared.clone();
    let handle = thread::Builder::new()
        .name("screen-capture".into())
        .spawn(move || {
            capture_loop(
                ffmpeg,
                directory,
                session_qpc_start,
                qpc_frequency,
                command_receiver,
                ready_sender,
                thread_shared,
            )
        })?;

    let mut capture = ScreenCapture {
        command_sender,
        handle: Some(handle),
        pending_stop: None,
        shared,
    };
    match ready_receiver.recv_timeout(FIRST_FRAME_TIMEOUT + Duration::from_secs(2)) {
        Ok(Ok(())) => Ok(capture),
        Ok(Err(message)) => {
            let _ = capture.stop();
            Err(CoreError::Media(message))
        }
        Err(RecvTimeoutError::Timeout) => {
            let _ = capture.stop();
            Err(CoreError::Media(
                "screen capture did not produce a first frame within ten seconds".into(),
            ))
        }
        Err(RecvTimeoutError::Disconnected) => {
            let warning = capture
                .shared
                .failure()
                .unwrap_or_else(|| "screen capture thread ended during startup".into());
            let _ = capture.stop();
            Err(CoreError::Media(warning))
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) struct MacosScreenCaptureRequest {
    layout: AppLayout,
    ffmpeg: PathBuf,
    directory: PathBuf,
    session_qpc_start: Option<u64>,
    qpc_frequency: Option<u64>,
    commands: Receiver<ScreenCommand>,
    ready_sender: SyncSender<Result<(), String>>,
    result_sender: SyncSender<Result<ScreenCaptureSummary, String>>,
    shared: Arc<ScreenCaptureShared>,
}

#[cfg(target_os = "macos")]
#[derive(Clone)]
pub(crate) struct MacosScreenFailureSinks {
    ready_sender: SyncSender<Result<(), String>>,
    result_sender: SyncSender<Result<ScreenCaptureSummary, String>>,
    shared: Arc<ScreenCaptureShared>,
}

#[cfg(target_os = "macos")]
impl MacosScreenFailureSinks {
    pub(crate) fn fail(&self, message: impl Into<String>) {
        let message = message.into();
        if self.shared.state.lock().lifecycle == Lifecycle::Stopped {
            return;
        }
        self.shared.fail(message.clone());
        let _ = self.ready_sender.try_send(Err(message.clone()));
        let _ = self.result_sender.try_send(Err(message));
    }
}

#[cfg(target_os = "macos")]
impl MacosScreenCaptureRequest {
    pub(crate) fn failure_sinks(&self) -> MacosScreenFailureSinks {
        MacosScreenFailureSinks {
            ready_sender: self.ready_sender.clone(),
            result_sender: self.result_sender.clone(),
            shared: self.shared.clone(),
        }
    }
}

/// Prepares the screen-control proxy that is attached to the existing guarded
/// macOS ScreenCaptureKit stream by `audio::spawn_macos_captures`.
#[cfg(target_os = "macos")]
type MacosPreparedCapture = (
    ScreenCapture,
    MacosScreenCaptureRequest,
    Receiver<Result<(), String>>,
);

#[cfg(target_os = "macos")]
pub(crate) fn prepare_macos(
    layout: &AppLayout,
    directory: PathBuf,
    session_qpc_start: Option<u64>,
    qpc_frequency: Option<u64>,
) -> CoreResult<MacosPreparedCapture> {
    fs::create_dir_all(&directory)?;
    let ffmpeg = media_tools::resolve(layout, MediaTool::Ffmpeg)?;
    let (command_sender, commands) = mpsc::sync_channel(4);
    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    let (result_sender, result_receiver) = mpsc::sync_channel(1);
    let shared = Arc::new(ScreenCaptureShared::default());
    Ok((
        ScreenCapture {
            command_sender,
            handle: None,
            result_receiver: Some(result_receiver),
            pending_stop: None,
            shared: shared.clone(),
        },
        MacosScreenCaptureRequest {
            layout: layout.clone(),
            ffmpeg,
            directory,
            session_qpc_start,
            qpc_frequency,
            commands,
            ready_sender,
            result_sender,
            shared,
        },
        ready_receiver,
    ))
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
enum MacosRecordingEvent {
    Started(Option<u64>),
    Finished,
    Failed(String),
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MacosSegmentFinalization {
    Attached,
    AwaitingDelegate,
    Finished,
}

#[cfg(target_os = "macos")]
#[derive(Debug, PartialEq, Eq)]
struct MacosSegmentFinalizeError {
    message: String,
    retryable: bool,
}

#[cfg(target_os = "macos")]
impl MacosSegmentFinalizeError {
    fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
        }
    }

    fn terminal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }
}

#[cfg(target_os = "macos")]
struct MacosRunningSegment {
    sequence: u32,
    final_path: PathBuf,
    partial_path: PathBuf,
    timeline_start_ms: i64,
    qpc_started: Option<u64>,
    qpc_first: Option<u64>,
    qpc_last: Option<u64>,
    output: screencapturekit::recording_output::SCRecordingOutput,
    events: Receiver<MacosRecordingEvent>,
    finalization: MacosSegmentFinalization,
}

/// Screen recording state owned by the same coordinator and lifecycle lock as
/// microphone/system-audio capture. `SCRecordingOutput` provides hardware H.264
/// encoding without a second `SCStream` or an external capture process.
#[cfg(target_os = "macos")]
pub(crate) struct MacosScreenRuntime {
    layout: AppLayout,
    ffmpeg: PathBuf,
    directory: PathBuf,
    session_qpc_start: Option<u64>,
    qpc_frequency: Option<u64>,
    commands: Receiver<ScreenCommand>,
    ready_sender: Option<SyncSender<Result<(), String>>>,
    result_sender: Option<SyncSender<Result<ScreenCaptureSummary, String>>>,
    shared: Arc<ScreenCaptureShared>,
    summary: ScreenCaptureSummary,
    running: Option<MacosRunningSegment>,
    next_sequence: u32,
    next_timeline_start_ms: i64,
    completed: bool,
}

#[cfg(target_os = "macos")]
impl MacosScreenRuntime {
    pub(crate) fn attach_before_start(
        request: MacosScreenCaptureRequest,
        stream: &screencapturekit::stream::SCStream,
    ) -> Result<Self, String> {
        let mut runtime = Self {
            layout: request.layout,
            ffmpeg: request.ffmpeg,
            directory: request.directory,
            session_qpc_start: request.session_qpc_start,
            qpc_frequency: request.qpc_frequency,
            commands: request.commands,
            ready_sender: Some(request.ready_sender),
            result_sender: Some(request.result_sender),
            shared: request.shared,
            summary: ScreenCaptureSummary::default(),
            running: None,
            next_sequence: 1,
            next_timeline_start_ms: 0,
            completed: false,
        };
        runtime.start_segment(stream)?;
        Ok(runtime)
    }

    pub(crate) fn mark_stream_started(&mut self) -> Result<(), String> {
        let result = self.wait_for_first_frame();
        match &result {
            Ok(()) => {
                self.shared.transition(Lifecycle::Capturing);
                if let Some(sender) = self.ready_sender.take() {
                    let _ = sender.try_send(Ok(()));
                }
            }
            Err(message) => {
                self.shared.fail(message.clone());
                if let Some(sender) = self.ready_sender.take() {
                    let _ = sender.try_send(Err(message.clone()));
                }
            }
        }
        result
    }

    pub(crate) fn process_commands(&mut self, stream: &screencapturekit::stream::SCStream) {
        loop {
            match self.commands.try_recv() {
                Ok(ScreenCommand::Pause(acknowledge)) => {
                    let result = self.pause(stream);
                    let _ = acknowledge.send(result.clone());
                    if let Err(message) = result {
                        self.fail_and_complete(stream, message);
                    }
                }
                Ok(ScreenCommand::Resume(acknowledge)) => {
                    let result = self.resume(stream);
                    let _ = acknowledge.send(result.clone());
                    if let Err(message) = result {
                        self.fail_and_complete(stream, message);
                    }
                }
                Ok(ScreenCommand::Stop(acknowledge)) => {
                    let result = self.finish_and_complete(stream);
                    let result_is_ready = self.running.is_none();
                    let _ = acknowledge.send(result);
                    if result_is_ready {
                        self.send_result();
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    let _ = self.finish_and_complete(stream);
                    if self.running.is_none() {
                        self.send_result();
                    }
                    return;
                }
                Err(TryRecvError::Empty) => return,
            }
        }
    }

    pub(crate) fn finish_with_audio(&mut self, stream: &screencapturekit::stream::SCStream) {
        let _ = self.finish_and_complete(stream);
        self.send_result();
    }

    fn pause(&mut self, stream: &screencapturekit::stream::SCStream) -> Result<(), String> {
        if self.completed {
            return Err("screen capture has already stopped".into());
        }
        if self.running.is_none() {
            return Err("screen capture is already paused".into());
        }
        self.finish_running_segment(stream)?;
        self.shared.transition(Lifecycle::Paused);
        Ok(())
    }

    fn resume(&mut self, stream: &screencapturekit::stream::SCStream) -> Result<(), String> {
        if self.completed {
            return Err("screen capture has already stopped".into());
        }
        if self.running.is_some() {
            return Err("screen capture is already running".into());
        }
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.start_segment(stream)?;
        self.wait_for_first_frame()?;
        self.shared.transition(Lifecycle::Capturing);
        Ok(())
    }

    fn start_segment(&mut self, stream: &screencapturekit::stream::SCStream) -> Result<(), String> {
        use screencapturekit::recording_output::{
            RecordingCallbacks, SCRecordingOutput, SCRecordingOutputCodec,
            SCRecordingOutputConfiguration, SCRecordingOutputFileType,
        };

        let sequence = self.next_sequence;
        let final_path = self
            .directory
            .join(format!("screen-segment-{sequence:05}.mp4"));
        let partial_path = self
            .directory
            .join(format!("screen-segment-{sequence:05}.mp4.partial"));
        let _ = fs::remove_file(&final_path);
        let _ = fs::remove_file(&partial_path);
        let (event_sender, events) = mpsc::channel();
        let started_sender = event_sender.clone();
        let failed_sender = event_sender.clone();
        let callbacks = RecordingCallbacks::new()
            .on_start(move || {
                let _ = started_sender.send(MacosRecordingEvent::Started(
                    audio::query_performance_counter(),
                ));
            })
            .on_finish(move || {
                let _ = event_sender.send(MacosRecordingEvent::Finished);
            })
            .on_fail(move |message| {
                let _ = failed_sender.send(MacosRecordingEvent::Failed(message));
            });
        let configuration = SCRecordingOutputConfiguration::new()
            .with_output_url(&partial_path)
            .with_video_codec(SCRecordingOutputCodec::H264)
            .with_output_file_type(SCRecordingOutputFileType::MP4);
        let output =
            SCRecordingOutput::new_with_delegate(&configuration, callbacks).ok_or_else(|| {
                "ScreenCaptureKit could not create an H.264 recording output".to_string()
            })?;
        stream.add_recording_output(&output).map_err(|error| {
            format!("ScreenCaptureKit could not attach screen recording: {error}")
        })?;
        self.running = Some(MacosRunningSegment {
            sequence,
            final_path,
            partial_path,
            timeline_start_ms: self.next_timeline_start_ms,
            qpc_started: None,
            qpc_first: None,
            qpc_last: None,
            output,
            events,
            finalization: MacosSegmentFinalization::Attached,
        });
        Ok(())
    }

    fn wait_for_first_frame(&mut self) -> Result<(), String> {
        let segment = self
            .running
            .as_mut()
            .ok_or_else(|| "screen recording output is unavailable".to_string())?;
        let deadline = Instant::now() + FIRST_FRAME_TIMEOUT;
        let mut started = false;
        loop {
            while let Ok(event) = segment.events.try_recv() {
                match event {
                    MacosRecordingEvent::Started(qpc_started) => {
                        started = true;
                        if segment.qpc_started.is_none() {
                            segment.qpc_started = qpc_started;
                        }
                    }
                    MacosRecordingEvent::Failed(message) => {
                        return Err(format!(
                            "ScreenCaptureKit screen recording failed: {message}"
                        ));
                    }
                    MacosRecordingEvent::Finished => {
                        return Err(
                            "ScreenCaptureKit screen recording ended before its first frame".into(),
                        );
                    }
                }
            }
            if started && macos_duration_ms(segment.output.recorded_duration()) > 0 {
                let qpc_first = audio::query_performance_counter();
                segment.qpc_first = qpc_first;
                if self.summary.segments.is_empty() {
                    segment.timeline_start_ms =
                        qpc_offset_ms(qpc_first, self.session_qpc_start, self.qpc_frequency)
                            .unwrap_or(0);
                    self.next_timeline_start_ms = segment.timeline_start_ms;
                }
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(
                    "ScreenCaptureKit did not produce a screen frame within eight seconds".into(),
                );
            }
            thread::sleep(PROCESS_POLL_INTERVAL);
        }
    }

    fn finish_and_complete(
        &mut self,
        stream: &screencapturekit::stream::SCStream,
    ) -> Result<(), String> {
        let was_completed = self.completed;
        let existing_failure = self.shared.failure();
        let result = self.finish_running_segment(stream);
        if !was_completed {
            match &result {
                Ok(()) if existing_failure.is_none() => {
                    self.shared.transition(Lifecycle::Stopped);
                }
                Ok(()) => {
                    if let Some(message) = existing_failure {
                        self.summary.warn_once(message);
                    }
                }
                Err(message) => {
                    if let Some(existing) = existing_failure {
                        self.summary.warn_once(existing);
                    }
                    self.shared.fail(message.clone());
                    self.summary.warn_once(message.clone());
                }
            }
            self.completed = true;
        } else if let Err(message) = &result {
            self.summary.warn_once(message.clone());
        }
        result
    }

    fn fail_and_complete(&mut self, stream: &screencapturekit::stream::SCStream, message: String) {
        if self.completed {
            return;
        }
        self.shared.fail(message.clone());
        self.summary.warn_once(message);
        self.completed = true;
        if let Err(cleanup) = self.finish_running_segment(stream) {
            self.summary.warn_once(cleanup);
        }
        if self.running.is_none() {
            self.send_result();
        }
    }

    fn finish_running_segment(
        &mut self,
        stream: &screencapturekit::stream::SCStream,
    ) -> Result<(), String> {
        let Some(mut segment) = self.running.take() else {
            return Ok(());
        };
        // This is the serialized coordinator gate: audio has already paused or
        // stopped, but native recording finalization has not begun. Delegate
        // completion may lag this boundary and must not extend logical video.
        if segment.qpc_last.is_none() {
            segment.qpc_last = audio::query_performance_counter();
        }
        match finish_macos_segment(stream, &self.layout, &self.ffmpeg, &mut segment) {
            Ok(completed) => {
                self.next_timeline_start_ms = completed
                    .timeline_start_ms
                    .saturating_add(completed.duration_ms);
                self.summary.segments.push(completed);
                Ok(())
            }
            Err(error) => {
                if error.retryable {
                    self.running = Some(segment);
                }
                Err(error.message)
            }
        }
    }

    fn send_result(&mut self) {
        if let Some(sender) = self.result_sender.take() {
            let _ = sender.try_send(Ok(self.summary.clone()));
        }
    }
}

#[cfg(target_os = "macos")]
fn finish_macos_segment(
    stream: &screencapturekit::stream::SCStream,
    layout: &AppLayout,
    ffmpeg: &Path,
    segment: &mut MacosRunningSegment,
) -> Result<ScreenSegmentSummary, MacosSegmentFinalizeError> {
    if segment.finalization == MacosSegmentFinalization::Attached {
        stream
            .remove_recording_output(&segment.output)
            .map_err(|error| {
                MacosSegmentFinalizeError::retryable(format!(
                    "ScreenCaptureKit could not finalize screen recording: {error}"
                ))
            })?;
        segment.finalization = MacosSegmentFinalization::AwaitingDelegate;
    }
    wait_for_macos_recording_finished(
        &mut segment.finalization,
        &segment.events,
        STOP_GRACE_PERIOD,
    )?;
    if segment.finalization != MacosSegmentFinalization::Finished {
        return Err(MacosSegmentFinalizeError::terminal(
            "ScreenCaptureKit screen recording is not finalized",
        ));
    }
    let size = fs::metadata(&segment.partial_path)
        .map_err(|error| {
            MacosSegmentFinalizeError::terminal(format!(
                "ScreenCaptureKit screen segment is unavailable: {error}"
            ))
        })?
        .len();
    if size == 0 {
        return Err(MacosSegmentFinalizeError::terminal(
            "ScreenCaptureKit finalized an empty screen segment",
        ));
    }
    let duration_ms = publish_video_only_macos_segment(
        layout,
        ffmpeg,
        &segment.partial_path,
        &segment.final_path,
    )
    .map_err(MacosSegmentFinalizeError::retryable)?;
    Ok(ScreenSegmentSummary {
        sequence: segment.sequence,
        path: segment.final_path.clone(),
        timeline_start_ms: segment.timeline_start_ms,
        duration_ms,
        frame_count: ((duration_ms as u64)
            .saturating_mul(SCREEN_FRAMES_PER_SECOND as u64)
            .saturating_add(999)
            / 1_000)
            .max(1),
        qpc_started: segment.qpc_started,
        qpc_first: segment.qpc_first,
        qpc_last: segment.qpc_last,
        stderr_log: None,
    })
}

#[cfg(target_os = "macos")]
fn wait_for_macos_recording_finished(
    finalization: &mut MacosSegmentFinalization,
    events: &Receiver<MacosRecordingEvent>,
    timeout: Duration,
) -> Result<(), MacosSegmentFinalizeError> {
    match finalization {
        MacosSegmentFinalization::Finished => return Ok(()),
        MacosSegmentFinalization::Attached => {
            return Err(MacosSegmentFinalizeError::terminal(
                "ScreenCaptureKit recording output was not removed before finalization",
            ));
        }
        MacosSegmentFinalization::AwaitingDelegate => {}
    }

    let deadline = Instant::now() + timeout;
    loop {
        match events.try_recv() {
            Ok(MacosRecordingEvent::Finished) => {
                *finalization = MacosSegmentFinalization::Finished;
                return Ok(());
            }
            Ok(MacosRecordingEvent::Failed(message)) => {
                return Err(MacosSegmentFinalizeError::terminal(format!(
                    "ScreenCaptureKit screen recording failed: {message}"
                )));
            }
            Ok(MacosRecordingEvent::Started(_)) | Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                return Err(MacosSegmentFinalizeError::terminal(
                    "ScreenCaptureKit recording delegate disconnected before finalization",
                ));
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(MacosSegmentFinalizeError::retryable(
                "ScreenCaptureKit recording delegate did not confirm finalization within five seconds; the unfinalized partial was retained",
            ));
        }
        thread::sleep(PROCESS_POLL_INTERVAL.min(remaining));
    }
}

#[cfg(target_os = "macos")]
fn publish_video_only_macos_segment(
    layout: &AppLayout,
    ffmpeg: &Path,
    source: &Path,
    destination: &Path,
) -> Result<i64, String> {
    let video_only = PathBuf::from(format!(
        "{}.video-only.partial",
        destination.to_string_lossy()
    ));
    let _ = fs::remove_file(&video_only);
    let mut command = std::process::Command::new(ffmpeg);
    command
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(source)
        .args([
            "-map",
            "0:v:0",
            "-an",
            "-c:v",
            "copy",
            "-movflags",
            "+faststart",
            "-f",
            "mp4",
        ])
        .arg(&video_only);
    let output = run_macos_command_bounded(command, SANITIZE_TIMEOUT).map_err(|error| {
        let _ = fs::remove_file(&video_only);
        format!("could not sanitize the screen segment: {error}")
    })?;
    if !output.status.success() {
        let _ = fs::remove_file(&video_only);
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "screen segment video-only remux failed: {}",
            detail.trim()
        ));
    }
    if fs::metadata(&video_only)
        .map_err(|error| format!("sanitized screen segment is unavailable: {error}"))?
        .len()
        == 0
    {
        let _ = fs::remove_file(&video_only);
        return Err("sanitized screen segment is empty".into());
    }
    let duration_ms = match probe_macos_segment_duration(layout, &video_only) {
        Ok(duration_ms) => duration_ms,
        Err(error) => {
            let _ = fs::remove_file(&video_only);
            return Err(error);
        }
    };
    let _ = fs::remove_file(destination);
    fs::rename(&video_only, destination)
        .map_err(|error| format!("could not publish screen capture segment: {error}"))?;
    let _ = fs::remove_file(source);
    Ok(duration_ms)
}

#[cfg(target_os = "macos")]
fn probe_macos_segment_duration(layout: &AppLayout, path: &Path) -> Result<i64, String> {
    layout
        .relative_to_root(path)
        .map_err(|error| format!("sanitized screen segment is outside app storage: {error}"))?;
    let ffprobe = media_tools::resolve(layout, MediaTool::Ffprobe)
        .map_err(|error| format!("FFprobe is unavailable: {error}"))?;
    let mut command = std::process::Command::new(ffprobe);
    command
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=index:format=duration",
            "-of",
            "json",
        ])
        .arg(path);
    let output = run_macos_command_bounded(command, PROBE_TIMEOUT).map_err(|error| {
        format!("FFprobe could not inspect the sanitized screen segment: {error}")
    })?;
    if !output.status.success() {
        return Err(format!(
            "FFprobe rejected the sanitized screen segment: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    parse_macos_probe_duration(&output.stdout)
}

#[cfg(target_os = "macos")]
fn parse_macos_probe_duration(output: &[u8]) -> Result<i64, String> {
    let parsed: serde_json::Value = serde_json::from_slice(output)
        .map_err(|error| format!("FFprobe returned invalid screen metadata: {error}"))?;
    let has_video = parsed
        .get("streams")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|streams| !streams.is_empty());
    if !has_video {
        return Err("FFprobe found no timed video stream in the sanitized screen segment".into());
    }
    let duration_ms = parsed
        .get("format")
        .and_then(|format| format.get("duration"))
        .and_then(serde_json::Value::as_str)
        .and_then(|duration| duration.parse::<f64>().ok())
        .filter(|duration| duration.is_finite() && *duration > 0.0)
        .map(|duration| (duration * 1_000.0).round() as i64);
    positive_probed_duration_ms(duration_ms)
}

#[cfg(target_os = "macos")]
fn positive_probed_duration_ms(duration_ms: Option<i64>) -> Result<i64, String> {
    duration_ms.filter(|duration| *duration > 0).ok_or_else(|| {
        "FFprobe did not report a positive duration for the sanitized screen segment".into()
    })
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct BoundedCommandOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[cfg(target_os = "macos")]
fn run_macos_command_bounded(
    mut command: std::process::Command,
    timeout: Duration,
) -> Result<BoundedCommandOutput, String> {
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("process could not start: {error}"))?;
    let stdout = child.stdout.take().ok_or_else(|| {
        let _ = child.kill();
        let _ = child.wait();
        "process stdout was unavailable".to_string()
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        let _ = child.kill();
        let _ = child.wait();
        "process stderr was unavailable".to_string()
    })?;
    let stdout_reader =
        spawn_macos_output_reader("screen-media-stdout", stdout).inspect_err(|_error| {
            let _ = child.kill();
            let _ = child.wait();
        })?;
    let stderr_reader = match spawn_macos_output_reader("screen-media-stderr", stderr) {
        Ok(reader) => reader,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            return Err(error);
        }
    };

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_reader.join();
                    let stderr = stderr_reader
                        .join()
                        .ok()
                        .and_then(Result::ok)
                        .unwrap_or_default();
                    let detail = String::from_utf8_lossy(&stderr);
                    return Err(format!(
                        "process did not finish within {} seconds{}{}",
                        timeout.as_secs_f64(),
                        if detail.trim().is_empty() { "" } else { ": " },
                        detail.trim()
                    ));
                }
                thread::sleep(PROCESS_POLL_INTERVAL.min(remaining));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("could not inspect process status: {error}"));
            }
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| "process stdout reader panicked".to_string())?
        .map_err(|error| format!("could not read process stdout: {error}"))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "process log reader panicked".to_string())?
        .map_err(|error| format!("could not read process stderr: {error}"))?;
    Ok(BoundedCommandOutput {
        status,
        stdout,
        stderr,
    })
}

#[cfg(target_os = "macos")]
fn spawn_macos_output_reader<R>(
    name: &str,
    mut reader: R,
) -> Result<thread::JoinHandle<std::io::Result<Vec<u8>>>, String>
where
    R: std::io::Read + Send + 'static,
{
    thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).map(|_| bytes)
        })
        .map_err(|error| format!("process output reader could not start: {error}"))
}

#[cfg(target_os = "macos")]
fn macos_duration_ms(time: screencapturekit::cm::CMTime) -> i64 {
    if time.value <= 0 || time.timescale <= 0 {
        return 0;
    }
    ((time.value as i128)
        .saturating_mul(1_000)
        .checked_div(time.timescale as i128)
        .unwrap_or_default())
    .clamp(0, i64::MAX as i128) as i64
}

#[cfg(not(target_os = "macos"))]
fn capture_loop(
    ffmpeg: PathBuf,
    directory: PathBuf,
    session_qpc_start: Option<u64>,
    qpc_frequency: Option<u64>,
    commands: Receiver<ScreenCommand>,
    ready: SyncSender<Result<(), String>>,
    shared: Arc<ScreenCaptureShared>,
) -> ScreenCaptureSummary {
    let mut summary = ScreenCaptureSummary::default();
    let mut sequence = 1_u32;
    let mut timeline_start_ms = 0_i64;
    let mut running = match start_segment(&ffmpeg, &directory, sequence, timeline_start_ms) {
        Ok(mut segment) => {
            timeline_start_ms =
                qpc_offset_ms(segment.qpc_first, session_qpc_start, qpc_frequency).unwrap_or(0);
            segment.timeline_start_ms = timeline_start_ms;
            shared.transition(Lifecycle::Capturing);
            let _ = ready.send(Ok(()));
            Some(segment)
        }
        Err(message) => {
            shared.fail(message.clone());
            let _ = ready.send(Err(message.clone()));
            summary.warn(message);
            return summary;
        }
    };

    loop {
        match commands.recv_timeout(COMMAND_POLL_INTERVAL) {
            Ok(ScreenCommand::Pause(acknowledge)) => {
                let result = match running.take() {
                    Some(segment) => finish_segment(segment).map(|segment| {
                        timeline_start_ms = segment.timeline_start_ms + segment.duration_ms;
                        summary.segments.push(segment);
                        shared.transition(Lifecycle::Paused);
                    }),
                    None => Err("screen capture is already paused".into()),
                };
                if let Err(message) = &result {
                    shared.fail(message.clone());
                    summary.warn(message.clone());
                }
                let _ = acknowledge.send(result.clone());
                if result.is_err() {
                    return summary;
                }
            }
            Ok(ScreenCommand::Resume(acknowledge)) => {
                let result = if running.is_some() {
                    Err("screen capture is already running".into())
                } else {
                    sequence = sequence.saturating_add(1);
                    match start_segment(&ffmpeg, &directory, sequence, timeline_start_ms) {
                        Ok(segment) => {
                            running = Some(segment);
                            shared.transition(Lifecycle::Capturing);
                            Ok(())
                        }
                        Err(message) => Err(message),
                    }
                };
                if let Err(message) = &result {
                    shared.fail(message.clone());
                    summary.warn(message.clone());
                }
                let _ = acknowledge.send(result.clone());
                if result.is_err() {
                    return summary;
                }
            }
            Ok(ScreenCommand::Stop(acknowledge)) => {
                let result = match running.take() {
                    Some(segment) => finish_segment(segment).map(|segment| {
                        summary.segments.push(segment);
                    }),
                    None => Ok(()),
                };
                match &result {
                    Ok(()) => shared.transition(Lifecycle::Stopped),
                    Err(message) => {
                        shared.fail(message.clone());
                        summary.warn(message.clone());
                    }
                }
                let _ = acknowledge.send(result);
                return summary;
            }
            Err(RecvTimeoutError::Timeout) => {
                let Some(segment) = running.as_mut() else {
                    continue;
                };
                match segment.child.try_wait() {
                    Ok(Some(status)) => {
                        let segment = running.take().expect("running segment checked above");
                        match finish_exited_segment(segment, status) {
                            Ok(completed) => {
                                summary.segments.push(completed);
                                let message = "FFmpeg screen capture ended unexpectedly";
                                shared.fail(message);
                                summary.warn(message);
                            }
                            Err(message) => {
                                shared.fail(message.clone());
                                summary.warn(message);
                            }
                        }
                        return summary;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let message = format!("could not inspect FFmpeg screen capture: {error}");
                        shared.fail(message.clone());
                        summary.warn(message);
                        if let Some(segment) = running.take() {
                            let _ = abort_segment(segment);
                        }
                        return summary;
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                if let Some(segment) = running.take() {
                    match finish_segment(segment) {
                        Ok(completed) => summary.segments.push(completed),
                        Err(message) => summary.warn(message),
                    }
                }
                shared.transition(Lifecycle::Stopped);
                return summary;
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
struct RunningSegment {
    sequence: u32,
    final_path: PathBuf,
    partial_path: PathBuf,
    timeline_start_ms: i64,
    child: Child,
    stderr_handle: Option<thread::JoinHandle<String>>,
    frame_count: Arc<AtomicU64>,
    qpc_first: Option<u64>,
}

#[cfg(not(target_os = "macos"))]
fn start_segment(
    ffmpeg: &Path,
    directory: &Path,
    sequence: u32,
    timeline_start_ms: i64,
) -> Result<RunningSegment, String> {
    let final_path = directory.join(format!("screen-segment-{sequence:05}.mp4"));
    let partial_path = directory.join(format!("screen-segment-{sequence:05}.mp4.partial"));
    let _ = fs::remove_file(&partial_path);

    let mut command = Command::new(ffmpeg);
    command
        .args(ffmpeg_capture_args(&partial_path))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("FFmpeg screen capture could not start: {error}"))?;
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("FFmpeg screen capture stderr was not available".into());
        }
    };
    let frame_count = Arc::new(AtomicU64::new(0));
    let reader_frames = frame_count.clone();
    let stderr_handle = match thread::Builder::new()
        .name(format!("screen-capture-log-{sequence}"))
        .spawn(move || drain_stderr(stderr, reader_frames))
    {
        Ok(handle) => handle,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "screen capture log reader could not start: {error}"
            ));
        }
    };

    let mut segment = RunningSegment {
        sequence,
        final_path,
        partial_path,
        timeline_start_ms,
        child,
        stderr_handle: Some(stderr_handle),
        frame_count,
        qpc_first: None,
    };
    let deadline = Instant::now() + FIRST_FRAME_TIMEOUT;
    loop {
        if segment.frame_count.load(Ordering::Acquire) > 0 {
            segment.qpc_first = audio::query_performance_counter();
            return Ok(segment);
        }
        match segment.child.try_wait() {
            Ok(Some(status)) => {
                let log = take_stderr_log(&mut segment);
                let _ = fs::remove_file(&segment.partial_path);
                return Err(process_error(
                    "FFmpeg screen capture exited before its first frame",
                    status,
                    &log,
                ));
            }
            Ok(None) => {}
            Err(error) => {
                let warning = format!("could not inspect FFmpeg during startup: {error}");
                let _ = abort_segment(segment);
                return Err(warning);
            }
        }
        if Instant::now() >= deadline {
            let log = abort_segment(segment);
            return Err(with_log(
                "FFmpeg screen capture timed out waiting for its first frame",
                &log,
            ));
        }
        thread::sleep(PROCESS_POLL_INTERVAL);
    }
}

#[cfg(not(target_os = "macos"))]
fn finish_segment(mut segment: RunningSegment) -> Result<ScreenSegmentSummary, String> {
    let write_error = segment
        .child
        .stdin
        .as_mut()
        .and_then(|stdin| stdin.write_all(b"q\n").and_then(|_| stdin.flush()).err());
    let deadline = Instant::now() + STOP_GRACE_PERIOD;
    let mut killed = false;
    let status = loop {
        match segment.child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(PROCESS_POLL_INTERVAL),
            Ok(None) => {
                killed = true;
                let _ = segment.child.kill();
                break segment
                    .child
                    .wait()
                    .map_err(|error| format!("could not reap FFmpeg screen capture: {error}"))?;
            }
            Err(error) => {
                let _ = segment.child.kill();
                let _ = segment.child.wait();
                let log = take_stderr_log(&mut segment);
                return Err(with_log(
                    &format!("could not wait for FFmpeg screen capture: {error}"),
                    &log,
                ));
            }
        }
    };
    let log = take_stderr_log(&mut segment);
    if killed {
        return Err(with_log(
            "FFmpeg screen capture did not stop within five seconds and was terminated",
            &log,
        ));
    }
    if !status.success() {
        return Err(process_error(
            "FFmpeg screen capture failed while finalizing a segment",
            status,
            &log,
        ));
    }
    if segment.frame_count.load(Ordering::Acquire) == 0 {
        return Err(with_log(
            "FFmpeg screen capture finalized a segment with no frames",
            &log,
        ));
    }
    if let Some(error) = write_error {
        log::warn!("could not send q to FFmpeg screen capture: {error}");
    }
    finalize_segment_file(segment, log)
}

#[cfg(not(target_os = "macos"))]
fn finish_exited_segment(
    mut segment: RunningSegment,
    status: ExitStatus,
) -> Result<ScreenSegmentSummary, String> {
    let log = take_stderr_log(&mut segment);
    if !status.success() {
        return Err(process_error(
            "FFmpeg screen capture exited unexpectedly",
            status,
            &log,
        ));
    }
    finalize_segment_file(segment, log)
}

#[cfg(not(target_os = "macos"))]
fn finalize_segment_file(
    segment: RunningSegment,
    log: String,
) -> Result<ScreenSegmentSummary, String> {
    let frames = segment.frame_count.load(Ordering::Acquire);
    if frames == 0 {
        return Err(with_log("FFmpeg screen capture produced no frames", &log));
    }
    fs::rename(&segment.partial_path, &segment.final_path)
        .map_err(|error| format!("could not publish screen capture segment: {error}"))?;
    Ok(ScreenSegmentSummary {
        sequence: segment.sequence,
        path: segment.final_path,
        timeline_start_ms: segment.timeline_start_ms,
        duration_ms: frames.saturating_mul(1_000) as i64 / SCREEN_FRAMES_PER_SECOND as i64,
        frame_count: frames,
        qpc_started: None,
        qpc_first: segment.qpc_first,
        qpc_last: audio::query_performance_counter(),
        stderr_log: (!log.trim().is_empty()).then(|| log.trim().to_string()),
    })
}

#[cfg(not(target_os = "macos"))]
fn abort_segment(mut segment: RunningSegment) -> String {
    let _ = segment.child.kill();
    let _ = segment.child.wait();
    let log = take_stderr_log(&mut segment);
    let _ = fs::remove_file(&segment.partial_path);
    log
}

#[cfg(not(target_os = "macos"))]
fn take_stderr_log(segment: &mut RunningSegment) -> String {
    segment
        .stderr_handle
        .take()
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default()
}

#[cfg(not(target_os = "macos"))]
fn drain_stderr(stderr: impl std::io::Read, frame_count: Arc<AtomicU64>) -> String {
    let mut log = String::new();
    for line in BufReader::new(stderr).lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                push_bounded_log(
                    &mut log,
                    &format!("screen capture log read failed: {error}"),
                );
                break;
            }
        };
        if let Some(frames) = parse_frame_progress(&line) {
            frame_count.fetch_max(frames, Ordering::Release);
        } else if !is_progress_line(&line) {
            push_bounded_log(&mut log, &line);
        }
    }
    log
}

#[cfg(not(target_os = "macos"))]
fn parse_frame_progress(line: &str) -> Option<u64> {
    line.trim()
        .strip_prefix("frame=")
        .and_then(|value| value.trim().parse().ok())
}

#[cfg(not(target_os = "macos"))]
fn is_progress_line(line: &str) -> bool {
    const KEYS: &[&str] = &[
        "fps=",
        "stream_",
        "bitrate=",
        "total_size=",
        "out_time_us=",
        "out_time_ms=",
        "out_time=",
        "dup_frames=",
        "drop_frames=",
        "speed=",
        "progress=",
    ];
    let line = line.trim();
    KEYS.iter().any(|key| line.starts_with(key))
}

#[cfg(not(target_os = "macos"))]
fn push_bounded_log(log: &mut String, line: &str) {
    if !log.is_empty() {
        log.push('\n');
    }
    log.push_str(line);
    if log.len() > STDERR_LOG_LIMIT {
        let mut remove = log.len() - STDERR_LOG_LIMIT;
        while remove < log.len() && !log.is_char_boundary(remove) {
            remove += 1;
        }
        log.drain(..remove);
    }
}

#[cfg(not(target_os = "macos"))]
fn process_error(prefix: &str, status: ExitStatus, log: &str) -> String {
    with_log(&format!("{prefix} ({status})"), log)
}

#[cfg(not(target_os = "macos"))]
fn with_log(prefix: &str, log: &str) -> String {
    if log.trim().is_empty() {
        prefix.into()
    } else {
        format!("{prefix}: {}", log.trim())
    }
}

fn qpc_offset_ms(
    value: Option<u64>,
    session_start: Option<u64>,
    frequency: Option<u64>,
) -> Option<i64> {
    let value = value?;
    let session_start = session_start?;
    let frequency = frequency?.max(1);
    Some(
        value
            .saturating_sub(session_start)
            .saturating_mul(1_000)
            .checked_div(frequency)
            .unwrap_or_default()
            .min(i64::MAX as u64) as i64,
    )
}

#[cfg(not(target_os = "macos"))]
fn ffmpeg_capture_args(output: &Path) -> Vec<OsString> {
    [
        "-y",
        "-hide_banner",
        "-loglevel",
        "warning",
        "-nostats",
        "-progress",
        "pipe:2",
        "-stats_period",
        "0.2",
        "-f",
        "gdigrab",
        "-framerate",
        "5",
        "-draw_mouse",
        "1",
        "-rtbufsize",
        "256M",
        "-i",
        "desktop",
        "-an",
        "-vf",
        "scale=trunc(iw/2)*2:trunc(ih/2)*2,format=yuv420p",
        "-fps_mode",
        "cfr",
        "-c:v",
        "h264_mf",
        "-rate_control",
        "quality",
        "-quality",
        "75",
        "-scenario",
        "display_remoting",
        "-g",
        "10",
        "-movflags",
        "+frag_keyframe+empty_moov+default_base_moof",
        "-f",
        "mp4",
    ]
    .into_iter()
    .map(OsString::from)
    .chain(std::iter::once(output.as_os_str().to_owned()))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_control_queue_times_out_without_blocking_forever() {
        let (sender, receiver) = mpsc::sync_channel(1);
        sender.try_send(1_u8).unwrap();
        let started = Instant::now();

        assert_eq!(
            try_send_until(&sender, 2_u8, started + Duration::from_millis(30)),
            Err(CommandSendError::Timeout)
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(receiver);
    }

    #[cfg(not(target_os = "macos"))]
    fn arguments() -> Vec<String> {
        ffmpeg_capture_args(Path::new("capture.mp4.partial"))
            .into_iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn capture_arguments_use_gdigrab_media_foundation_h264_and_fragmented_mp4() {
        let arguments = arguments();
        assert!(arguments.windows(2).any(|pair| pair == ["-f", "gdigrab"]));
        assert!(arguments.windows(2).any(|pair| pair == ["-framerate", "5"]));
        assert!(arguments.windows(2).any(|pair| pair == ["-i", "desktop"]));
        assert!(arguments
            .windows(2)
            .any(|pair| pair == ["-draw_mouse", "1"]));
        assert!(arguments.windows(2).any(|pair| pair == ["-c:v", "h264_mf"]));
        assert!(arguments
            .windows(2)
            .any(|pair| pair == ["-scenario", "display_remoting"]));
        assert!(arguments
            .iter()
            .any(|argument| { argument == "+frag_keyframe+empty_moov+default_base_moof" }));
        assert!(!arguments
            .iter()
            .any(|argument| argument.contains("libx264")));
        assert_eq!(arguments.last().unwrap(), "capture.mp4.partial");
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn progress_parser_requires_numeric_frame_records() {
        assert_eq!(parse_frame_progress("frame=1"), Some(1));
        assert_eq!(parse_frame_progress(" frame= 25 "), Some(25));
        assert_eq!(parse_frame_progress("fps=5.0"), None);
        assert_eq!(parse_frame_progress("frame=unknown"), None);
    }

    #[test]
    fn summary_duration_follows_concatenated_segment_timeline() {
        let summary = ScreenCaptureSummary {
            segments: vec![
                ScreenSegmentSummary {
                    sequence: 1,
                    path: "one.mp4".into(),
                    timeline_start_ms: 0,
                    duration_ms: 1_000,
                    frame_count: 5,
                    qpc_started: None,
                    qpc_first: None,
                    qpc_last: None,
                    stderr_log: None,
                },
                ScreenSegmentSummary {
                    sequence: 2,
                    path: "two.mp4".into(),
                    timeline_start_ms: 1_000,
                    duration_ms: 2_000,
                    frame_count: 10,
                    qpc_started: None,
                    qpc_first: None,
                    qpc_last: None,
                    stderr_log: None,
                },
            ],
            warning: None,
        };
        assert_eq!(summary.duration_ms(), 3_000);
        assert_eq!(summary.start_offset_ms(), 0);
    }

    #[test]
    fn qpc_offset_uses_the_recording_session_epoch() {
        assert_eq!(
            qpc_offset_ms(Some(125_000_000), Some(100_000_000), Some(10_000_000)),
            Some(2_500)
        );
        assert_eq!(qpc_offset_ms(Some(1), None, Some(1)), None);
    }

    #[test]
    fn lifecycle_reports_only_live_capture_as_active() {
        let shared = ScreenCaptureShared::default();
        assert!(!shared.is_active());
        shared.transition(Lifecycle::Capturing);
        assert!(shared.is_active());
        shared.transition(Lifecycle::Paused);
        assert!(!shared.is_active());
        shared.fail("encoder stopped");
        assert!(!shared.is_active());
        assert_eq!(shared.failure().as_deref(), Some("encoder stopped"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn recording_output_duration_is_only_used_for_readiness() {
        assert_eq!(
            macos_duration_ms(screencapturekit::cm::CMTime::new(15, 10)),
            1_500
        );
        assert_eq!(macos_duration_ms(screencapturekit::cm::CMTime::INVALID), 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn finalized_segment_duration_requires_positive_ffprobe_result() {
        assert_eq!(positive_probed_duration_ms(Some(15_898)).unwrap(), 15_898);
        assert!(positive_probed_duration_ms(Some(0)).is_err());
        assert!(positive_probed_duration_ms(None).is_err());

        let probed = parse_macos_probe_duration(
            br#"{"format":{"duration":"15.898333"},"streams":[{"index":0}]}"#,
        )
        .unwrap();
        assert_eq!(probed, 15_898);
        assert!(
            parse_macos_probe_duration(br#"{"format":{"duration":"15.898333"},"streams":[]}"#)
                .is_err()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn delegate_timeout_retains_state_until_finished_arrives() {
        let (sender, events) = mpsc::channel();
        let mut finalization = MacosSegmentFinalization::AwaitingDelegate;

        let error =
            wait_for_macos_recording_finished(&mut finalization, &events, Duration::from_millis(5))
                .unwrap_err();
        assert!(error.retryable);
        assert_eq!(finalization, MacosSegmentFinalization::AwaitingDelegate);

        sender.send(MacosRecordingEvent::Finished).unwrap();
        wait_for_macos_recording_finished(&mut finalization, &events, Duration::from_secs(1))
            .unwrap();
        assert_eq!(finalization, MacosSegmentFinalization::Finished);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn delegate_failure_is_terminal_and_never_finalized() {
        let (sender, events) = mpsc::channel();
        sender
            .send(MacosRecordingEvent::Failed("disk full".into()))
            .unwrap();
        let mut finalization = MacosSegmentFinalization::AwaitingDelegate;

        let error =
            wait_for_macos_recording_finished(&mut finalization, &events, Duration::from_secs(1))
                .unwrap_err();
        assert!(!error.retryable);
        assert!(error.message.contains("disk full"));
        assert_ne!(finalization, MacosSegmentFinalization::Finished);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn coordinated_stop_bounds_a_missing_ack_after_result() {
        let (command_sender, command_receiver) = mpsc::sync_channel(1);
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        result_sender
            .send(Ok(ScreenCaptureSummary::default()))
            .unwrap();
        let mut capture = ScreenCapture {
            command_sender,
            handle: None,
            result_receiver: Some(result_receiver),
            pending_stop: None,
            shared: Arc::new(ScreenCaptureShared::default()),
        };

        capture.request_stop().unwrap();
        let started = Instant::now();
        let summary = capture.finish_stop().unwrap();

        assert!(summary.segments.is_empty());
        assert!(summary
            .warning
            .as_deref()
            .unwrap()
            .contains("without its control acknowledgement"));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(matches!(
            command_receiver.try_recv(),
            Ok(ScreenCommand::Stop(_))
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn coordinated_stop_preserves_result_first_summary_and_late_ack_error() {
        let (command_sender, command_receiver) = mpsc::sync_channel(1);
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        result_sender
            .send(Ok(ScreenCaptureSummary {
                segments: vec![ScreenSegmentSummary {
                    sequence: 1,
                    path: "preserved.mp4".into(),
                    timeline_start_ms: 0,
                    duration_ms: 1_000,
                    frame_count: 5,
                    qpc_started: Some(1),
                    qpc_first: Some(10),
                    qpc_last: Some(20),
                    stderr_log: None,
                }],
                warning: None,
            }))
            .unwrap();
        let mut capture = ScreenCapture {
            command_sender,
            handle: None,
            result_receiver: Some(result_receiver),
            pending_stop: None,
            shared: Arc::new(ScreenCaptureShared::default()),
        };

        capture.request_stop().unwrap();
        let acknowledge = match command_receiver.recv().unwrap() {
            ScreenCommand::Stop(acknowledge) => acknowledge,
            _ => panic!("expected queued stop command"),
        };
        let acknowledgement = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            acknowledge
                .send(Err("delegate failed after summary publication".into()))
                .unwrap();
        });

        let summary = capture.finish_stop().unwrap();
        acknowledgement.join().unwrap();

        assert_eq!(summary.segments.len(), 1);
        assert_eq!(summary.segments[0].path, PathBuf::from("preserved.mp4"));
        assert!(summary
            .warning
            .as_deref()
            .unwrap()
            .contains("delegate failed after summary publication"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bounded_child_is_killed_after_deadline() {
        let mut command = std::process::Command::new("/bin/sleep");
        command.arg("10");
        let started = Instant::now();

        let error = run_macos_command_bounded(command, Duration::from_millis(30)).unwrap_err();

        assert!(error.contains("did not finish"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn stderr_log_is_bounded_and_keeps_the_tail() {
        let mut log = String::new();
        push_bounded_log(&mut log, &"a".repeat(STDERR_LOG_LIMIT));
        push_bounded_log(&mut log, "last warning");
        assert!(log.len() <= STDERR_LOG_LIMIT);
        assert!(log.ends_with("last warning"));
    }
}
