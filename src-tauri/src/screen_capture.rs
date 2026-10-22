use std::{
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use serde::Serialize;

use crate::{
    audio,
    error::{CoreError, CoreResult},
    layout::AppLayout,
    media_tools::{self, MediaTool},
};

const SCREEN_FRAMES_PER_SECOND: u32 = 5;
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(8);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_GRACE_PERIOD: Duration = Duration::from_secs(5);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(25);
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(200);
const STDERR_LOG_LIMIT: usize = 32 * 1024;

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
}

enum ScreenCommand {
    Pause(mpsc::Sender<Result<(), String>>),
    Resume(mpsc::Sender<Result<(), String>>),
    Stop(mpsc::Sender<Result<(), String>>),
}

pub(crate) struct ScreenCapture {
    command_sender: SyncSender<ScreenCommand>,
    handle: Option<thread::JoinHandle<ScreenCaptureSummary>>,
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
        if self.handle.is_none() {
            return Err(CoreError::Conflict(
                "screen capture has already been stopped".into(),
            ));
        }

        let (acknowledge, response) = mpsc::channel();
        let sent = self.command_sender.send(ScreenCommand::Stop(acknowledge));
        let acknowledgement = if sent.is_ok() {
            response.recv_timeout(CONTROL_TIMEOUT).ok()
        } else {
            None
        };

        let handle = self.handle.take().expect("screen handle checked above");
        let mut summary = handle
            .join()
            .map_err(|_| CoreError::Media("screen capture thread panicked".into()))?;

        match acknowledgement {
            Some(Ok(())) => {}
            Some(Err(message)) => summary.warn(message),
            None if sent.is_ok() => summary.warn("screen capture stop acknowledgement timed out"),
            None => {}
        }
        Ok(summary)
    }

    fn control(
        &self,
        command: fn(mpsc::Sender<Result<(), String>>) -> ScreenCommand,
    ) -> CoreResult<()> {
        let (acknowledge, response) = mpsc::channel();
        self.command_sender
            .send(command(acknowledge))
            .map_err(|_| CoreError::Media("screen capture thread is not running".into()))?;
        match response.recv_timeout(CONTROL_TIMEOUT) {
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
        if self.handle.is_some() {
            let _ = self.stop();
        }
    }
}

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
        qpc_first: segment.qpc_first,
        qpc_last: audio::query_performance_counter(),
        stderr_log: (!log.trim().is_empty()).then(|| log.trim().to_string()),
    })
}

fn abort_segment(mut segment: RunningSegment) -> String {
    let _ = segment.child.kill();
    let _ = segment.child.wait();
    let log = take_stderr_log(&mut segment);
    let _ = fs::remove_file(&segment.partial_path);
    log
}

fn take_stderr_log(segment: &mut RunningSegment) -> String {
    segment
        .stderr_handle
        .take()
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default()
}

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

fn parse_frame_progress(line: &str) -> Option<u64> {
    line.trim()
        .strip_prefix("frame=")
        .and_then(|value| value.trim().parse().ok())
}

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

fn process_error(prefix: &str, status: ExitStatus, log: &str) -> String {
    with_log(&format!("{prefix} ({status})"), log)
}

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

    fn arguments() -> Vec<String> {
        ffmpeg_capture_args(Path::new("capture.mp4.partial"))
            .into_iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
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

    #[test]
    fn stderr_log_is_bounded_and_keeps_the_tail() {
        let mut log = String::new();
        push_bounded_log(&mut log, &"a".repeat(STDERR_LOG_LIMIT));
        push_bounded_log(&mut log, "last warning");
        assert!(log.len() <= STDERR_LOG_LIMIT);
        assert!(log.ends_with("last warning"));
    }
}
