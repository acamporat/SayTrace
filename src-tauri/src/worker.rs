use std::{
    collections::VecDeque,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{mpsc, Arc},
    thread,
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroize::Zeroize;

use crate::{
    db::now_ms,
    error::{CoreError, CoreResult},
    layout::AppLayout,
    media_tools::{self, MediaTool},
    models::WorkerStatus,
};

pub const PROTOCOL_VERSION: &str = "1.0";
pub const PIPELINE_VERSION: &str = "2026.07.28.1";
pub const MAX_CONTROL_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_AUDIO_BYTES: usize = 64 * 1024 * 1024;
const MAGIC: &[u8; 4] = b"LTW1";
const FRAME_VERSION_MAJOR: u8 = 1;
const HEADER_BYTES: usize = 16;
// A first launch of the 5 GB packaged Python/CUDA runtime can spend well over
// 15 seconds in Windows Defender and DLL loader work before Python executes.
// Pipe closure still reports a crash immediately; this is only the cold-start
// ceiling for an otherwise live child process.
const WORKER_HELLO_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const JOB_HEARTBEAT_TIMEOUT_MS: i64 = 60_000;
const MODEL_SETUP_STALL_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const MODEL_SETUP_OVERALL_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    Json = 1,
    Audio = 2,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AudioMetadata {
    pub session_id: String,
    pub stream_id: String,
    pub sequence: u64,
    pub start_ms: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerRequest {
    #[serde(rename = "type")]
    pub message_type: &'static str,
    pub protocol_version: &'static str,
    pub request_id: String,
    pub job_id: Option<String>,
    pub pipeline_version: Option<String>,
    pub command: String,
    pub payload: Value,
}

impl WorkerRequest {
    pub fn new(command: impl Into<String>, payload: Value) -> Self {
        Self {
            message_type: "request",
            protocol_version: PROTOCOL_VERSION,
            request_id: Uuid::now_v7().to_string(),
            job_id: None,
            pipeline_version: Some(PIPELINE_VERSION.into()),
            command: command.into(),
            payload,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkerEvent {
    #[serde(rename = "type")]
    pub message_type: String,
    pub protocol_version: Option<String>,
    pub request_id: Option<String>,
    pub job_id: Option<String>,
    pub sequence: Option<u64>,
    pub event: Option<String>,
    pub error_code: Option<String>,
    pub message: Option<String>,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub result: Value,
    pub ok: Option<bool>,
    pub error: Option<WorkerErrorPayload>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkerErrorPayload {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub details: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelSetupProgress {
    pub request_id: String,
    pub key: String,
    pub code: String,
    pub phase: String,
    pub completed_steps: u64,
    pub total_steps: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
}

pub fn write_json_frame(writer: &mut impl Write, value: &impl Serialize) -> CoreResult<()> {
    let payload = serde_json::to_vec(value)?;
    write_frame(writer, FrameKind::Json, 0, &payload)
}

fn write_sensitive_json_frame(writer: &mut impl Write, value: &impl Serialize) -> CoreResult<()> {
    let mut payload = serde_json::to_vec(value)?;
    let result = write_frame(writer, FrameKind::Json, 0, &payload);
    payload.zeroize();
    result
}

pub fn write_audio_frame(
    writer: &mut impl Write,
    metadata: &AudioMetadata,
    pcm_s16le: &[u8],
) -> CoreResult<()> {
    if pcm_s16le.is_empty() || pcm_s16le.len() % (metadata.channels as usize * 2) != 0 {
        return Err(CoreError::InvalidInput(
            "live PCM frame is empty or not sample-aligned".into(),
        ));
    }
    let metadata = serde_json::to_vec(metadata)?;
    if metadata.len() > MAX_CONTROL_BYTES {
        return Err(CoreError::InvalidInput(
            "live audio metadata exceeds protocol limits".into(),
        ));
    }
    let mut payload = Vec::with_capacity(4 + metadata.len() + pcm_s16le.len());
    payload.extend_from_slice(&(metadata.len() as u32).to_be_bytes());
    payload.extend_from_slice(&metadata);
    payload.extend_from_slice(pcm_s16le);
    write_frame(writer, FrameKind::Audio, 0, &payload)
}

pub fn read_json_frame(reader: &mut impl Read) -> CoreResult<WorkerEvent> {
    let (kind, _, payload) = read_frame(reader)?;
    if kind != FrameKind::Json {
        return Err(CoreError::Worker(
            "expected a JSON control frame from worker".into(),
        ));
    }
    let event: WorkerEvent = serde_json::from_slice(&payload)?;
    if let Some(version) = &event.protocol_version {
        if version != PROTOCOL_VERSION {
            return Err(CoreError::Worker(format!(
                "worker protocol {version} is incompatible with host {PROTOCOL_VERSION}"
            )));
        }
    }
    Ok(event)
}

fn write_frame(
    writer: &mut impl Write,
    kind: FrameKind,
    flags: u16,
    payload: &[u8],
) -> CoreResult<()> {
    let limit = if kind == FrameKind::Audio {
        MAX_AUDIO_BYTES
    } else {
        MAX_CONTROL_BYTES
    };
    if payload.len() > limit {
        return Err(CoreError::InvalidInput(format!(
            "worker frame exceeds {limit} byte limit"
        )));
    }
    writer.write_all(MAGIC)?;
    writer.write_all(&[FRAME_VERSION_MAJOR, kind as u8])?;
    writer.write_all(&flags.to_be_bytes())?;
    writer.write_all(&(payload.len() as u64).to_be_bytes())?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

fn read_frame(reader: &mut impl Read) -> CoreResult<(FrameKind, u16, Vec<u8>)> {
    let mut header = [0_u8; HEADER_BYTES];
    reader.read_exact(&mut header)?;
    if &header[..4] != MAGIC || header[4] != FRAME_VERSION_MAJOR {
        return Err(CoreError::Worker(
            "worker emitted an invalid frame header".into(),
        ));
    }
    let kind = match header[5] {
        1 => FrameKind::Json,
        2 => FrameKind::Audio,
        value => {
            return Err(CoreError::Worker(format!(
                "worker emitted unknown frame kind {value}"
            )));
        }
    };
    let flags = u16::from_be_bytes([header[6], header[7]]);
    let length = u64::from_be_bytes(header[8..16].try_into().unwrap()) as usize;
    let limit = if kind == FrameKind::Audio {
        MAX_AUDIO_BYTES
    } else {
        MAX_CONTROL_BYTES
    };
    if length > limit {
        return Err(CoreError::Worker(format!(
            "worker frame declared {length} bytes, above {limit} byte limit"
        )));
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    Ok((kind, flags, payload))
}

struct WorkerProcess {
    child: Child,
    stdin: ChildStdin,
    events: mpsc::Receiver<CoreResult<WorkerEvent>>,
    reader: Option<thread::JoinHandle<()>>,
}

struct SupervisorState {
    process: Option<WorkerProcess>,
    pending_events: VecDeque<WorkerEvent>,
    state: String,
    last_heartbeat_ms: Option<i64>,
    error: Option<String>,
}

pub struct WorkerSupervisor {
    layout: AppLayout,
    state: Arc<Mutex<SupervisorState>>,
    lifecycle: Mutex<()>,
}

impl WorkerSupervisor {
    pub fn new(layout: AppLayout) -> Self {
        Self {
            layout,
            state: Arc::new(Mutex::new(SupervisorState {
                process: None,
                pending_events: VecDeque::new(),
                state: "offline".into(),
                last_heartbeat_ms: None,
                error: None,
            })),
            lifecycle: Mutex::new(()),
        }
    }

    pub fn status(&self) -> WorkerStatus {
        let mut state = self.state.lock();
        receive_available_events(&mut state);
        let mut process_id = None;
        let mut exited = None;
        if let Some(process) = state.process.as_mut() {
            process_id = Some(process.child.id());
            if let Ok(Some(status)) = process.child.try_wait() {
                exited = Some(format!("worker exited with {status}"));
            }
        }
        if let Some(error) = exited {
            state.process = None;
            state.state = "offline".into();
            state.error = Some(error);
            process_id = None;
        }
        WorkerStatus {
            state: state.state.clone(),
            protocol_version: 1,
            pipeline_version: PIPELINE_VERSION.into(),
            process_id,
            last_heartbeat_ms: state.last_heartbeat_ms,
            error: state.error.clone(),
        }
    }

    pub fn start(&self) -> CoreResult<WorkerStatus> {
        let _lifecycle = self.lifecycle.lock();
        self.start_inner()
    }

    fn start_inner(&self) -> CoreResult<WorkerStatus> {
        {
            let status = self.status();
            if status.process_id.is_some() && status.state == "ready" && status.error.is_none() {
                return Ok(status);
            }
            if status.process_id.is_some() {
                self.stop_inner();
            }
        }
        let mut command = self.worker_command(false)?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let mut child = command
            .spawn()
            .map_err(|error| CoreError::Worker(format!("could not launch worker: {error}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| CoreError::Worker("worker stdin pipe was not created".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CoreError::Worker("worker stdout pipe was not created".into()))?;
        let (event_sender, event_receiver) = mpsc::channel();
        let reader = match thread::Builder::new()
            .name("transcription-worker-events".into())
            .spawn(move || worker_reader_loop(stdout, event_sender))
        {
            Ok(reader) => reader,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CoreError::Io(error));
            }
        };
        let process = WorkerProcess {
            child,
            stdin,
            events: event_receiver,
            reader: Some(reader),
        };
        if let Err(error) = wait_for_compatible_hello(&process.events, WORKER_HELLO_TIMEOUT, false)
        {
            let message = error.to_string();
            terminate_worker_process(process);
            let mut state = self.state.lock();
            state.process = None;
            state.pending_events.clear();
            state.state = "offline".into();
            state.last_heartbeat_ms = None;
            state.error = Some(message);
            return Err(error);
        }
        let mut state = self.state.lock();
        state.process = Some(process);
        state.pending_events.clear();
        state.state = "ready".into();
        state.last_heartbeat_ms = Some(now_ms());
        state.error = None;
        drop(state);
        Ok(self.status())
    }

    pub fn stop(&self) {
        let _lifecycle = self.lifecycle.lock();
        self.stop_inner();
    }

    fn stop_inner(&self) {
        let mut state = self.state.lock();
        let process = state.process.take();
        state.pending_events.clear();
        state.state = "offline".into();
        state.last_heartbeat_ms = None;
        drop(state);
        if let Some(mut process) = process {
            let _ = write_json_frame(
                &mut process.stdin,
                &WorkerRequest::new("shutdown", json!({})),
            );
            terminate_worker_process(process);
        }
    }

    pub fn restart(&self) -> CoreResult<WorkerStatus> {
        let _lifecycle = self.lifecycle.lock();
        self.stop_inner();
        self.start_inner()
    }

    pub fn request(&self, request: WorkerRequest) -> CoreResult<WorkerEvent> {
        self.start()?;
        let mut state = self.state.lock();
        let mut deferred = Vec::new();
        let event = {
            let process = state
                .process
                .as_mut()
                .ok_or_else(|| CoreError::Worker("worker is offline".into()))?;
            write_json_frame(&mut process.stdin, &request)?;
            loop {
                let event = process
                    .events
                    .recv_timeout(Duration::from_secs(30))
                    .map_err(|error| {
                        CoreError::Worker(format!(
                            "worker did not answer request {}: {error}",
                            request.request_id
                        ))
                    })??;
                if matches!(event.event.as_deref(), Some("hello" | "heartbeat"))
                    || matches!(event.message_type.as_str(), "hello" | "heartbeat")
                {
                    continue;
                }
                if event.request_id.as_deref() == Some(request.request_id.as_str()) {
                    break event;
                }
                deferred.push(event);
            }
        };
        state.pending_events.extend(deferred);
        state.last_heartbeat_ms = Some(now_ms());
        if event.ok == Some(false) || event.message_type == "error" {
            let (code, message) = event
                .error
                .as_ref()
                .map(|error| (error.code.as_str(), error.message.as_str()))
                .unwrap_or((
                    event.error_code.as_deref().unwrap_or("WORKER_ERROR"),
                    event.message.as_deref().unwrap_or("worker request failed"),
                ));
            return Err(CoreError::Worker(format!("{}: {}", code, message)));
        }
        Ok(event)
    }

    pub fn install_model_pack(
        &self,
        token: &str,
        mut on_progress: impl FnMut(&ModelSetupProgress),
    ) -> CoreResult<Value> {
        if token.trim().is_empty() {
            return Err(CoreError::InvalidInput(
                "a Hugging Face access token is required for first-run model setup".into(),
            ));
        }
        let _lifecycle = self.lifecycle.lock();
        self.stop_inner();
        let mut command = self.worker_command(true)?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let mut child = command.spawn().map_err(|error| {
            CoreError::Worker(format!("could not launch model setup worker: {error}"))
        })?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| CoreError::Worker("setup worker stdin was not created".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CoreError::Worker("setup worker stdout was not created".into()))?;
        let (event_sender, event_receiver) = mpsc::channel();
        let reader = match thread::Builder::new()
            .name("model-setup-worker-events".into())
            .spawn(move || worker_reader_loop(stdout, event_sender))
        {
            Ok(reader) => reader,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CoreError::Io(error));
            }
        };
        if let Err(error) = wait_for_compatible_hello(&event_receiver, WORKER_HELLO_TIMEOUT, true) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err(error);
        }
        let mut installed = Vec::new();
        let mut progress = Vec::new();
        let setup_started = Instant::now();
        let install_result = (|| -> CoreResult<()> {
            for key in [
                "live_asr_en",
                "final_asr_en",
                "alignment_en",
                "diarization",
                "speaker_embedding",
            ] {
                let overall_remaining =
                    MODEL_SETUP_OVERALL_TIMEOUT.saturating_sub(setup_started.elapsed());
                if overall_remaining.is_zero() {
                    return Err(CoreError::Worker(
                        "MODEL_SETUP_OVERALL_TIMEOUT: model pack setup exceeded its time limit"
                            .into(),
                    ));
                }
                let mut request =
                    WorkerRequest::new("model.install", json!({"key":key,"token":token}));
                let write_result = write_sensitive_json_frame(&mut stdin, &request);
                if let Err(error) = write_result {
                    zeroize_request_token(&mut request);
                    return Err(error);
                }
                let mut relay_progress = |event: &ModelSetupProgress| {
                    progress.push(json!(event));
                    on_progress(event);
                };
                let wait_result = wait_for_setup_response(
                    &event_receiver,
                    &request.request_id,
                    key,
                    MODEL_SETUP_STALL_TIMEOUT,
                    overall_remaining,
                    &mut relay_progress,
                );
                zeroize_request_token(&mut request);
                let event = wait_result?;
                if event.ok == Some(false) {
                    let error = event.error.ok_or_else(|| {
                        CoreError::Worker("model setup failed without an error payload".into())
                    })?;
                    return Err(CoreError::Worker(format!(
                        "{}: {}",
                        error.code, error.message
                    )));
                }
                installed.push(event.result);
            }
            Ok(())
        })();
        let _ = write_json_frame(&mut stdin, &WorkerRequest::new("shutdown", json!({})));
        let _ = child.kill();
        let _ = child.wait();
        let _ = reader.join();
        install_result?;
        let _ = self.start_inner();
        Ok(json!({"models":installed,"progress":progress}))
    }

    pub fn send_live_audio(&self, metadata: AudioMetadata, pcm: &[u8]) -> CoreResult<()> {
        self.start()?;
        let mut state = self.state.lock();
        let process = state
            .process
            .as_mut()
            .ok_or_else(|| CoreError::Worker("worker is offline".into()))?;
        write_audio_frame(&mut process.stdin, &metadata, pcm)?;
        Ok(())
    }

    pub fn drain_events(&self) -> Vec<WorkerEvent> {
        let mut state = self.state.lock();
        receive_available_events(&mut state);
        let mut live = Vec::new();
        let mut retained = VecDeque::new();
        while let Some(event) = state.pending_events.pop_front() {
            if matches!(
                event.event.as_deref(),
                Some("draft_revision" | "device_warning" | "live_error")
            ) {
                live.push(event);
            } else {
                retained.push_back(event);
            }
        }
        state.pending_events = retained;
        live
    }

    /// Drains only events for one durable pipeline job. Live-caption events and
    /// events for other jobs remain queued for their owner.
    pub fn drain_job_events(&self, job_id: &str) -> Vec<WorkerEvent> {
        let mut state = self.state.lock();
        receive_available_events(&mut state);
        let mut matched = Vec::new();
        let mut retained = VecDeque::new();
        while let Some(event) = state.pending_events.pop_front() {
            let event_job_id = event
                .job_id
                .as_deref()
                .or_else(|| event.payload.get("job_id").and_then(Value::as_str));
            if event_job_id == Some(job_id) {
                matched.push(event);
            } else {
                retained.push_back(event);
            }
        }
        state.pending_events = retained;
        let terminal = matched
            .iter()
            .any(|event| matches!(event.event.as_deref(), Some("job_complete" | "job_error")));
        let failure = (!terminal)
            .then(|| detect_job_liveness_failure(&mut state, now_ms()))
            .flatten();
        let failed_process = if let Some(failure) = &failure {
            state.state = "offline".into();
            state.error = Some(failure.message.clone());
            state.last_heartbeat_ms = None;
            state.pending_events.clear();
            state.process.take()
        } else {
            None
        };
        drop(state);

        if let Some(process) = failed_process {
            terminate_worker_process(process);
        }
        if let Some(failure) = failure {
            let restart_error = self.start().err().map(|error| error.to_string());
            matched.push(job_liveness_error_event(job_id, failure, restart_error));
        }
        matched
    }

    fn worker_command(&self, allow_model_downloads: bool) -> CoreResult<Command> {
        let packaged = self.layout.runtime().join("local-transcript-worker.exe");
        let worker_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("worker");
        let worker_src = worker_root.join("src");
        let mut command = if packaged.is_file() {
            Command::new(packaged)
        } else if worker_src.is_dir() {
            let packaged_python = self.layout.runtime().join("python").join("python.exe");
            let project_python = worker_root.join(".venv").join("Scripts").join("python.exe");
            let executable = if packaged_python.is_file() {
                packaged_python
            } else if project_python.is_file() {
                project_python
            } else {
                return Err(CoreError::Worker(
                    "local worker environment is missing; run `uv sync --project worker --group dev` for development or install the packaged runtime".into(),
                ));
            };
            let mut command = Command::new(executable);
            command
                .args(["-u", "-m", "local_transcript_worker"])
                .current_dir(&worker_root)
                .env("PYTHONPATH", worker_src)
                .env("PYTHONUNBUFFERED", "1")
                .env("LOCAL_TRANSCRIPT_MODEL_ROOT", self.layout.models());
            command
        } else {
            return Err(CoreError::Worker(
                "local transcription worker runtime is not installed".into(),
            ));
        };
        let model_root = canonical_or_create(self.layout.models())?;
        let library_root = canonical_or_create(self.layout.library())?;
        let temp_root = canonical_or_create(self.layout.temp())?;
        let ffmpeg =
            worker_compatible_path(&media_tools::resolve(&self.layout, MediaTool::Ffmpeg)?);
        command
            .arg("--model-root")
            .arg(model_root)
            .arg("--allowed-root")
            .arg(library_root)
            .arg("--allowed-root")
            .arg(temp_root)
            .arg("--ffmpeg")
            .arg(ffmpeg);
        if allow_model_downloads {
            command.arg("--allow-model-downloads");
        }
        Ok(command)
    }
}

fn zeroize_request_token(request: &mut WorkerRequest) {
    if let Some(Value::String(secret)) = request.payload.get_mut("token") {
        secret.zeroize();
    }
}

fn receive_available_events(state: &mut SupervisorState) {
    let mut received = Vec::new();
    let mut heartbeat = false;
    let mut error_message = None;
    if let Some(process) = state.process.as_mut() {
        while let Ok(event) = process.events.try_recv() {
            match event {
                Ok(event) => {
                    if is_worker_liveness_event(&event) {
                        heartbeat = true;
                    } else {
                        received.push(event);
                    }
                }
                Err(error) => {
                    error_message = Some(error.to_string());
                    break;
                }
            }
        }
    }
    state.pending_events.extend(received);
    if heartbeat {
        state.last_heartbeat_ms = Some(now_ms());
    }
    if let Some(error) = error_message {
        state.error = Some(error);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct JobLivenessFailure {
    code: &'static str,
    message: String,
}

fn evaluate_job_liveness(
    process_present: bool,
    reader_error: Option<&str>,
    last_heartbeat_ms: Option<i64>,
    current_ms: i64,
    heartbeat_timeout_ms: i64,
) -> Option<JobLivenessFailure> {
    if !process_present {
        return Some(JobLivenessFailure {
            code: "WORKER_EXITED",
            message: "worker process exited during final processing".into(),
        });
    }
    if let Some(error) = reader_error {
        return Some(JobLivenessFailure {
            code: "WORKER_PIPE_CLOSED",
            message: format!("worker event pipe failed during final processing: {error}"),
        });
    }
    let Some(last_heartbeat_ms) = last_heartbeat_ms else {
        return Some(JobLivenessFailure {
            code: "WORKER_HEARTBEAT_TIMEOUT",
            message: "worker did not establish a heartbeat for final processing".into(),
        });
    };
    let age_ms = current_ms.saturating_sub(last_heartbeat_ms).max(0);
    (age_ms > heartbeat_timeout_ms).then(|| JobLivenessFailure {
        code: "WORKER_HEARTBEAT_TIMEOUT",
        message: format!("worker heartbeat stalled for {age_ms} ms during final processing"),
    })
}

fn detect_job_liveness_failure(
    state: &mut SupervisorState,
    current_ms: i64,
) -> Option<JobLivenessFailure> {
    let Some(process) = state.process.as_mut() else {
        return evaluate_job_liveness(
            false,
            state.error.as_deref(),
            state.last_heartbeat_ms,
            current_ms,
            JOB_HEARTBEAT_TIMEOUT_MS,
        );
    };
    match process.child.try_wait() {
        Ok(Some(status)) => {
            return Some(JobLivenessFailure {
                code: "WORKER_EXITED",
                message: format!("worker exited with {status} during final processing"),
            });
        }
        Err(error) => {
            return Some(JobLivenessFailure {
                code: "WORKER_PROCESS_ERROR",
                message: format!("could not inspect worker process: {error}"),
            });
        }
        Ok(None) => {}
    }
    evaluate_job_liveness(
        true,
        state.error.as_deref(),
        state.last_heartbeat_ms,
        current_ms,
        JOB_HEARTBEAT_TIMEOUT_MS,
    )
}

fn job_liveness_error_event(
    job_id: &str,
    failure: JobLivenessFailure,
    restart_error: Option<String>,
) -> WorkerEvent {
    let message = restart_error.map_or_else(
        || failure.message.clone(),
        |restart| format!("{}; worker restart failed: {restart}", failure.message),
    );
    WorkerEvent {
        message_type: "event".into(),
        protocol_version: Some(PROTOCOL_VERSION.into()),
        request_id: None,
        job_id: Some(job_id.into()),
        sequence: None,
        event: Some("job_error".into()),
        error_code: None,
        message: None,
        payload: json!({
            "job_id":job_id,
            "error":{
                "code":failure.code,
                "message":message,
                "retryable":true
            }
        }),
        result: Value::Null,
        ok: None,
        error: None,
    }
}

impl Drop for WorkerSupervisor {
    fn drop(&mut self) {
        self.stop();
    }
}

fn canonical_or_create(path: &Path) -> CoreResult<PathBuf> {
    fs::create_dir_all(path)?;
    Ok(worker_compatible_path(&path.canonicalize()?))
}

/// Convert Rust's Windows verbatim paths into the ordinary drive/UNC spelling
/// accepted by CTranslate2 and the other native ML libraries. Security checks
/// still happen against canonical paths before this interop-only conversion.
pub(crate) fn worker_compatible_path(path: &Path) -> PathBuf {
    PathBuf::from(worker_compatible_path_text(&path.to_string_lossy()))
}

pub(crate) fn worker_compatible_path_text(raw: &str) -> String {
    if let Some(unc) = raw.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{unc}");
    }
    if let Some(local) = raw.strip_prefix(r"\\?\") {
        let bytes = local.as_bytes();
        if bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/')
        {
            return local.to_owned();
        }
    }
    raw.to_owned()
}

fn worker_reader_loop(mut stdout: ChildStdout, sender: mpsc::Sender<CoreResult<WorkerEvent>>) {
    loop {
        match read_json_frame(&mut stdout) {
            Ok(event) => {
                if sender.send(Ok(event)).is_err() {
                    break;
                }
            }
            Err(error) => {
                let _ = sender.send(Err(error));
                break;
            }
        }
    }
}

fn terminate_worker_process(mut process: WorkerProcess) {
    let _ = process.child.kill();
    let _ = process.child.wait();
    if let Some(reader) = process.reader.take() {
        let _ = reader.join();
    }
}

fn is_worker_liveness_event(event: &WorkerEvent) -> bool {
    matches!(event.event.as_deref(), Some("hello" | "heartbeat"))
        || matches!(event.message_type.as_str(), "hello" | "heartbeat")
}

fn validate_compatible_hello(event: &WorkerEvent, setup_enabled: bool) -> CoreResult<()> {
    if event.message_type != "event" || event.event.as_deref() != Some("hello") {
        let details = event
            .error
            .as_ref()
            .map(|error| format!("{}: {}", error.code, error.message))
            .or_else(|| event.message.clone())
            .unwrap_or_else(|| format!("received worker message type {:?}", event.message_type));
        return Err(CoreError::Worker(format!(
            "WORKER_HELLO_INVALID: expected hello before worker traffic; {details}"
        )));
    }
    let top_protocol = event.protocol_version.as_deref();
    let payload_protocol = event
        .payload
        .get("protocol_version")
        .and_then(Value::as_str);
    let pipeline = event
        .payload
        .get("pipeline_version")
        .and_then(Value::as_str);
    let setup = event.payload.get("setup_enabled").and_then(Value::as_bool);
    if top_protocol != Some(PROTOCOL_VERSION)
        || payload_protocol != Some(PROTOCOL_VERSION)
        || pipeline != Some(PIPELINE_VERSION)
        || setup != Some(setup_enabled)
    {
        return Err(CoreError::Worker(format!(
            "WORKER_HELLO_INCOMPATIBLE: expected protocol {PROTOCOL_VERSION}, pipeline \
             {PIPELINE_VERSION}, setup_enabled={setup_enabled}; received protocol \
             {payload_protocol:?}, pipeline {pipeline:?}, setup_enabled={setup:?}"
        )));
    }
    Ok(())
}

fn wait_for_compatible_hello(
    receiver: &mpsc::Receiver<CoreResult<WorkerEvent>>,
    timeout: Duration,
    setup_enabled: bool,
) -> CoreResult<()> {
    let event = match receiver.recv_timeout(timeout) {
        Ok(event) => event?,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            return Err(CoreError::Worker(
                "WORKER_HELLO_TIMEOUT: worker did not provide a startup hello".into(),
            ));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err(CoreError::Worker(
                "WORKER_HELLO_PIPE_CLOSED: worker exited before startup hello".into(),
            ));
        }
    };
    validate_compatible_hello(&event, setup_enabled)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupTimeout {
    Stalled,
    Overall,
}

struct SetupWaitBudget {
    started: Instant,
    last_activity: Instant,
    stall_timeout: Duration,
    overall_timeout: Duration,
}

impl SetupWaitBudget {
    fn new(stall_timeout: Duration, overall_timeout: Duration) -> Self {
        Self::new_at(Instant::now(), stall_timeout, overall_timeout)
    }

    fn new_at(started: Instant, stall_timeout: Duration, overall_timeout: Duration) -> Self {
        Self {
            started,
            last_activity: started,
            stall_timeout,
            overall_timeout,
        }
    }

    fn note_activity(&mut self, now: Instant) {
        self.last_activity = now;
    }

    fn timeout_at(&self, now: Instant) -> Option<SetupTimeout> {
        if now.duration_since(self.started) >= self.overall_timeout {
            Some(SetupTimeout::Overall)
        } else if now.duration_since(self.last_activity) >= self.stall_timeout {
            Some(SetupTimeout::Stalled)
        } else {
            None
        }
    }

    fn next_wait(&self, now: Instant) -> Duration {
        let overall_remaining = self
            .overall_timeout
            .saturating_sub(now.duration_since(self.started));
        let stall_remaining = self
            .stall_timeout
            .saturating_sub(now.duration_since(self.last_activity));
        overall_remaining.min(stall_remaining)
    }
}

fn wait_for_setup_response(
    receiver: &mpsc::Receiver<CoreResult<WorkerEvent>>,
    request_id: &str,
    model_key: &str,
    stall_timeout: Duration,
    overall_timeout: Duration,
    on_progress: &mut impl FnMut(&ModelSetupProgress),
) -> CoreResult<WorkerEvent> {
    let mut budget = SetupWaitBudget::new(stall_timeout, overall_timeout);
    loop {
        let now = Instant::now();
        if let Some(timeout) = budget.timeout_at(now) {
            let code = match timeout {
                SetupTimeout::Stalled => "MODEL_SETUP_STALLED",
                SetupTimeout::Overall => "MODEL_SETUP_OVERALL_TIMEOUT",
            };
            return Err(CoreError::Worker(format!(
                "{code}: model setup did not complete request {request_id}"
            )));
        }
        let event = match receiver.recv_timeout(budget.next_wait(now)) {
            Ok(event) => event?,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(CoreError::Worker(
                    "MODEL_SETUP_WORKER_EXITED: model setup worker pipe closed".into(),
                ));
            }
        };
        if event.request_id.as_deref() == Some(request_id) {
            return Ok(event);
        }
        let heartbeat = is_worker_liveness_event(&event);
        let setup_progress = (event.event.as_deref() == Some("model_setup_progress"))
            .then(|| validate_model_setup_progress(&event.payload, request_id, model_key))
            .transpose()?;
        if heartbeat || setup_progress.is_some() {
            budget.note_activity(Instant::now());
        }
        if let Some(progress) = setup_progress {
            on_progress(&progress);
        }
    }
}

fn validate_model_setup_progress(
    payload: &Value,
    request_id: &str,
    model_key: &str,
) -> CoreResult<ModelSetupProgress> {
    let progress: ModelSetupProgress =
        serde_json::from_value(payload.clone()).map_err(|error| {
            CoreError::Worker(format!(
                "MODEL_SETUP_PROGRESS_INVALID: invalid setup progress payload: {error}"
            ))
        })?;
    let phase_valid = matches!(
        progress.phase.as_str(),
        "checking" | "downloading" | "verifying" | "publishing" | "complete" | "failed"
    );
    let code_valid = !progress.code.is_empty()
        && progress.code.len() <= 64
        && progress
            .code
            .bytes()
            .all(|value| value.is_ascii_uppercase() || value.is_ascii_digit() || value == b'_');
    if progress.request_id != request_id
        || progress.key != model_key
        || !phase_valid
        || !code_valid
        || progress.total_steps == 0
        || progress.total_steps > 100
        || progress.completed_steps > progress.total_steps
    {
        return Err(CoreError::Worker(
            "MODEL_SETUP_PROGRESS_INVALID: setup progress failed contract validation".into(),
        ));
    }
    Ok(progress)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello_event(pipeline_version: &str, setup_enabled: bool) -> WorkerEvent {
        WorkerEvent {
            message_type: "event".into(),
            protocol_version: Some(PROTOCOL_VERSION.into()),
            request_id: None,
            job_id: None,
            sequence: Some(1),
            event: Some("hello".into()),
            error_code: None,
            message: None,
            payload: json!({
                "protocol_version":PROTOCOL_VERSION,
                "pipeline_version":pipeline_version,
                "setup_enabled":setup_enabled
            }),
            result: Value::Null,
            ok: None,
            error: None,
        }
    }

    #[test]
    fn frame_matches_python_network_order_contract() {
        let request = WorkerRequest::new("ping", json!({"value": 3}));
        let mut encoded = Vec::new();
        write_json_frame(&mut encoded, &request).unwrap();
        assert_eq!(&encoded[..4], b"LTW1");
        assert_eq!(encoded[4], 1);
        assert_eq!(encoded[5], FrameKind::Json as u8);
        let length = u64::from_be_bytes(encoded[8..16].try_into().unwrap()) as usize;
        assert_eq!(encoded.len(), HEADER_BYTES + length);
        let value: Value = serde_json::from_slice(&encoded[16..]).unwrap();
        assert_eq!(value["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(value["type"], "request");
    }

    #[test]
    fn frame_reader_rejects_oversized_control_frames_before_allocation() {
        let mut header = Vec::new();
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&[1, 1]);
        header.extend_from_slice(&0_u16.to_be_bytes());
        header.extend_from_slice(&((MAX_CONTROL_BYTES + 1) as u64).to_be_bytes());
        assert!(read_frame(&mut header.as_slice()).is_err());
    }

    #[test]
    fn audio_frame_contains_big_endian_metadata_length() {
        let metadata = AudioMetadata {
            session_id: "session".into(),
            stream_id: "microphone".into(),
            sequence: 1,
            start_ms: 0,
            sample_rate: 48_000,
            channels: 1,
            sample_format: "s16le".into(),
        };
        let mut encoded = Vec::new();
        write_audio_frame(&mut encoded, &metadata, &[0, 0, 1, 0]).unwrap();
        assert_eq!(encoded[5], FrameKind::Audio as u8);
        let metadata_length = u32::from_be_bytes(encoded[16..20].try_into().unwrap()) as usize;
        assert!(metadata_length > 10);
        assert_eq!(&encoded[20 + metadata_length..], &[0, 0, 1, 0]);
    }

    #[test]
    fn startup_wait_accepts_only_a_protocol_and_pipeline_compatible_hello() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(Ok(hello_event(PIPELINE_VERSION, false)))
            .unwrap();

        wait_for_compatible_hello(&receiver, Duration::from_millis(1), false).unwrap();
    }

    #[test]
    fn packaged_worker_cold_start_budget_survives_security_scanning() {
        assert!(WORKER_HELLO_TIMEOUT >= Duration::from_secs(2 * 60));
    }

    #[test]
    fn native_worker_paths_remove_windows_verbatim_drive_prefixes() {
        assert_eq!(
            worker_compatible_path_text(r"\\?\C:\Users\Example\models"),
            r"C:\Users\Example\models"
        );
        assert_eq!(
            worker_compatible_path_text(r"\\?\UNC\server\share\models"),
            r"\\server\share\models"
        );
        assert_eq!(
            worker_compatible_path_text(r"\\?\GLOBALROOT\Device\HarddiskVolume1"),
            r"\\?\GLOBALROOT\Device\HarddiskVolume1"
        );
    }

    #[test]
    fn startup_rejects_incompatible_pipeline_before_ready() {
        let error = validate_compatible_hello(&hello_event("old-pipeline", false), false)
            .unwrap_err()
            .to_string();

        assert!(error.contains("WORKER_HELLO_INCOMPATIBLE"));
        assert!(error.contains("old-pipeline"));
    }

    #[test]
    fn startup_rejects_setup_mode_mismatch() {
        let error = validate_compatible_hello(&hello_event(PIPELINE_VERSION, true), false)
            .unwrap_err()
            .to_string();

        assert!(error.contains("WORKER_HELLO_INCOMPATIBLE"));
        assert!(error.contains("setup_enabled"));
    }

    #[test]
    fn rust_pipeline_contract_matches_packaged_worker_manifest() {
        let manifest: Value =
            serde_json::from_str(include_str!("../../worker/model-manifest.json")).unwrap();

        assert_eq!(manifest["pipeline_version"], PIPELINE_VERSION);
    }

    #[test]
    fn durable_job_liveness_has_a_deterministic_heartbeat_boundary() {
        assert_eq!(
            evaluate_job_liveness(true, None, Some(1_000), 61_000, 60_000),
            None
        );
        let failure = evaluate_job_liveness(true, None, Some(1_000), 61_001, 60_000).unwrap();

        assert_eq!(failure.code, "WORKER_HEARTBEAT_TIMEOUT");
        assert!(failure.message.contains("60001 ms"));
    }

    #[test]
    fn durable_job_reader_failure_is_retryable_and_namespaced_to_job() {
        let failure =
            evaluate_job_liveness(true, Some("invalid frame"), Some(5_000), 5_001, 60_000).unwrap();
        assert_eq!(failure.code, "WORKER_PIPE_CLOSED");

        let event = job_liveness_error_event("job-7", failure, None);

        assert_eq!(event.event.as_deref(), Some("job_error"));
        assert_eq!(event.job_id.as_deref(), Some("job-7"));
        assert_eq!(event.payload["job_id"], "job-7");
        assert_eq!(event.payload["error"]["code"], "WORKER_PIPE_CLOSED");
        assert_eq!(event.payload["error"]["retryable"], true);
    }

    #[test]
    fn setup_budget_accepts_progress_beyond_thirty_seconds_without_waiting() {
        let start = Instant::now();
        let mut budget =
            SetupWaitBudget::new_at(start, Duration::from_secs(30), Duration::from_secs(120));

        budget.note_activity(start + Duration::from_secs(25));
        assert_eq!(budget.timeout_at(start + Duration::from_secs(40)), None);
        budget.note_activity(start + Duration::from_secs(55));
        assert_eq!(budget.timeout_at(start + Duration::from_secs(70)), None);
        assert_eq!(
            budget.timeout_at(start + Duration::from_secs(86)),
            Some(SetupTimeout::Stalled)
        );
        assert_eq!(
            budget.timeout_at(start + Duration::from_secs(120)),
            Some(SetupTimeout::Overall)
        );
    }

    #[test]
    fn setup_wait_collects_matching_progress_before_response() {
        let (sender, receiver) = mpsc::channel();
        let request_id = "setup-request";
        sender
            .send(Ok(WorkerEvent {
                message_type: "event".into(),
                protocol_version: Some(PROTOCOL_VERSION.into()),
                request_id: None,
                job_id: None,
                sequence: Some(1),
                event: Some("model_setup_progress".into()),
                error_code: None,
                message: None,
                payload: json!({
                    "request_id":request_id,
                    "key":"live_asr_en",
                    "code":"MODEL_SETUP_PROGRESS",
                    "phase":"downloading",
                    "completed_steps":1,
                    "total_steps":4
                }),
                result: Value::Null,
                ok: None,
                error: None,
            }))
            .unwrap();
        sender
            .send(Ok(WorkerEvent {
                message_type: "response".into(),
                protocol_version: Some(PROTOCOL_VERSION.into()),
                request_id: Some(request_id.into()),
                job_id: None,
                sequence: Some(2),
                event: None,
                error_code: None,
                message: None,
                payload: Value::Null,
                result: json!({"ready":true}),
                ok: Some(true),
                error: None,
            }))
            .unwrap();
        let mut progress: Vec<ModelSetupProgress> = Vec::new();

        let response = wait_for_setup_response(
            &receiver,
            request_id,
            "live_asr_en",
            Duration::from_millis(10),
            Duration::from_millis(20),
            &mut |event| progress.push(event.clone()),
        )
        .unwrap();

        assert_eq!(response.ok, Some(true));
        assert_eq!(progress.len(), 1);
        assert_eq!(progress[0].phase, "downloading");
        assert_eq!(progress[0].completed_steps, 1);
    }

    #[test]
    fn setup_progress_rejects_unknown_fields_before_callback() {
        let payload = json!({
            "request_id":"setup-request",
            "key":"live_asr_en",
            "code":"MODEL_SETUP_PROGRESS",
            "phase":"downloading",
            "completed_steps":1,
            "total_steps":4,
            "token":"must-not-escape"
        });

        let error = validate_model_setup_progress(&payload, "setup-request", "live_asr_en")
            .unwrap_err()
            .to_string();

        assert!(error.contains("MODEL_SETUP_PROGRESS_INVALID"));
        assert!(!json!(ModelSetupProgress {
            request_id: "setup-request".into(),
            key: "live_asr_en".into(),
            code: "MODEL_SETUP_PROGRESS".into(),
            phase: "downloading".into(),
            completed_steps: 1,
            total_steps: 4,
            retryable: None,
        })
        .to_string()
        .contains("token"));
    }
}
