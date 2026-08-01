use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use rusqlite::{params, OptionalExtension, Transaction};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use uuid::Uuid;

use crate::{
    audio::{
        self, CaptionChunk, CaptureCommand, CaptureKind, CaptureShared, CaptureSpec,
        CaptureSummary, ClockAnchor, CAPTURE_SAMPLE_RATE,
    },
    db::{iso_from_ms, now_ms},
    error::{CoreError, CoreResult},
    media,
    media_tools::{self, MediaTool},
    models::{
        AudioDeviceList, Meeting, RecordingConfig, RecordingMarker, RecordingSession,
        RecordingState, RecordingStatus,
    },
    service::CoreService,
    worker::{AudioMetadata, WorkerRequest, WorkerSupervisor},
};

const CAPTION_QUEUE_CHUNKS: usize = 2048;
const CAPTURE_READY_TIMEOUT: Duration = Duration::from_secs(8);

struct ActiveTrack {
    track_id: String,
    kind: CaptureKind,
    command_sender: SyncSender<CaptureCommand>,
    handle: Option<thread::JoinHandle<CoreResult<CaptureSummary>>>,
    shared: Arc<CaptureShared>,
}

struct ActiveRecording {
    id: String,
    meeting_id: String,
    state: RecordingState,
    started_at_ms: i64,
    started_instant: Instant,
    paused_at: Option<Instant>,
    paused_total: Duration,
    current_pause_id: Option<String>,
    tracks: Vec<ActiveTrack>,
    manifest_path: PathBuf,
    monitor_stop: Arc<AtomicBool>,
    monitor_handle: Option<thread::JoinHandle<()>>,
    caption_handle: Option<thread::JoinHandle<()>>,
    qpc_start: Option<u64>,
    qpc_frequency: Option<u64>,
}

pub struct RecordingManager {
    core: Arc<CoreService>,
    worker: Arc<WorkerSupervisor>,
    active: Arc<Mutex<Option<ActiveRecording>>>,
}

impl RecordingManager {
    pub fn new(core: Arc<CoreService>, worker: Arc<WorkerSupervisor>) -> Self {
        Self {
            core,
            worker,
            active: Arc::new(Mutex::new(None)),
        }
    }

    pub fn list_devices(&self) -> CoreResult<AudioDeviceList> {
        audio::enumerate_audio_devices()
    }

    pub fn status(&self) -> RecordingStatus {
        let active = self.active.lock();
        let Some(active) = active.as_ref() else {
            return RecordingStatus::default();
        };
        status_from_active(active)
    }

    pub fn start(
        &self,
        title: String,
        config: RecordingConfig,
        app: AppHandle,
    ) -> CoreResult<RecordingSession> {
        if !config.capture_microphone && !config.capture_system_audio {
            return Err(CoreError::InvalidInput(
                "select a microphone, system audio, or both".into(),
            ));
        }
        if self.active.lock().is_some() {
            return Err(CoreError::Conflict(
                "another recording is already active".into(),
            ));
        }
        let title = if title.trim().is_empty() {
            format!("Meeting {}", chrono::Local::now().format("%Y-%m-%d %H-%M"))
        } else {
            title.trim().chars().take(180).collect()
        };
        let meeting_id = new_id();
        let session_id = new_id();
        let started_at_ms = now_ms();
        let session_directory = self
            .core
            .layout()
            .recordings()
            .join(&meeting_id)
            .join(&session_id);
        fs::create_dir_all(&session_directory)?;
        let manifest_path = session_directory.join("recording-manifest.jsonl");
        let qpc_start = audio::query_performance_counter();
        let qpc_frequency = audio::query_performance_frequency();
        append_manifest(
            &manifest_path,
            json!({
                "type": "recording_started",
                "manifestVersion": 1,
                "sessionId": session_id,
                "meetingId": meeting_id,
                "startedAtMs": started_at_ms,
                "qpcStart": qpc_start,
                "qpcFrequency": qpc_frequency,
                "config": config,
            }),
        )?;
        let manifest_relative = self.core.layout().relative_to_root(&manifest_path)?;

        let mut track_specs = Vec::new();
        if config.capture_microphone {
            track_specs.push((
                new_id(),
                CaptureKind::Microphone,
                config.microphone_device_id.clone(),
                1_u16,
            ));
        }
        if config.capture_system_audio {
            track_specs.push((
                new_id(),
                CaptureKind::Loopback,
                config.loopback_device_id.clone(),
                2_u16,
            ));
        }
        {
            let mut connection = self.core.database().connect()?;
            let transaction = connection.transaction()?;
            transaction.execute(
                "INSERT INTO meetings(
                    id,title,source_kind,status,created_at_ms,started_at_ms,language
                 ) VALUES (?1,?2,'recording','recording',?3,?3,'en')",
                params![meeting_id, title, started_at_ms],
            )?;
            transaction.execute(
                "INSERT INTO recording_sessions(
                    id,meeting_id,state,config_json,manifest_relative_path,qpc_frequency,
                    qpc_start,started_at_ms
                 ) VALUES (?1,?2,'starting',?3,?4,?5,?6,?7)",
                params![
                    session_id,
                    meeting_id,
                    serde_json::to_string(&config)?,
                    manifest_relative,
                    qpc_frequency.map(|value| value as i64),
                    qpc_start.map(|value| value as i64),
                    started_at_ms
                ],
            )?;
            for (track_id, kind, device_id, channels) in &track_specs {
                let directory = session_directory.join(kind.as_str());
                fs::create_dir_all(&directory)?;
                let relative = self.core.layout().relative_to_root(&directory)?;
                transaction.execute(
                    "INSERT INTO recording_tracks(
                        id,session_id,source_kind,device_id,relative_directory,
                        sample_rate_hz,channels,created_at_ms
                     ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                    params![
                        track_id,
                        session_id,
                        kind.as_str(),
                        device_id,
                        relative,
                        CAPTURE_SAMPLE_RATE,
                        channels,
                        started_at_ms
                    ],
                )?;
            }
            transaction.commit()?;
        }

        let (caption_sender, caption_receiver) = if config.live_captions {
            let (sender, receiver) = mpsc::sync_channel(CAPTION_QUEUE_CHUNKS);
            (Some(sender), Some(receiver))
        } else {
            (None, None)
        };
        let mut tracks = Vec::new();
        let mut readiness = Vec::new();
        for (track_id, kind, device_id, channels) in track_specs {
            let (command_sender, command_receiver) = mpsc::sync_channel(8);
            let shared = Arc::new(CaptureShared::default());
            let directory = session_directory.join(kind.as_str());
            let spec = CaptureSpec {
                session_id: session_id.clone(),
                kind,
                device_id,
                channels,
                directory,
                live_captions: config.live_captions,
                session_qpc_start: qpc_start,
                qpc_frequency,
            };
            match audio::spawn_capture(
                spec,
                command_receiver,
                caption_sender.clone(),
                shared.clone(),
            ) {
                Ok((handle, ready)) => {
                    readiness.push(ready);
                    tracks.push(ActiveTrack {
                        track_id,
                        kind,
                        command_sender,
                        handle: Some(handle),
                        shared,
                    });
                }
                Err(error) => {
                    stop_tracks(&mut tracks);
                    self.fail_start(&meeting_id, &session_id, &manifest_path, &error)?;
                    return Err(error);
                }
            }
        }
        drop(caption_sender);
        for ready in readiness {
            match ready.recv_timeout(CAPTURE_READY_TIMEOUT) {
                Ok(Ok(())) => {}
                Ok(Err(message)) => {
                    stop_tracks(&mut tracks);
                    let error = CoreError::Audio(message);
                    self.fail_start(&meeting_id, &session_id, &manifest_path, &error)?;
                    return Err(error);
                }
                Err(_) => {
                    stop_tracks(&mut tracks);
                    let error = CoreError::Audio(
                        "audio device did not become ready within eight seconds".into(),
                    );
                    self.fail_start(&meeting_id, &session_id, &manifest_path, &error)?;
                    return Err(error);
                }
            }
        }

        let live_streams = tracks
            .iter()
            .map(|track| {
                (
                    track.kind.as_str().to_string(),
                    Value::String(
                        live_source_type(track.kind, config.microphone_is_personal).into(),
                    ),
                )
            })
            .collect::<serde_json::Map<String, Value>>();
        let caption_handle = caption_receiver.map(|receiver| {
            spawn_caption_relay(
                self.worker.clone(),
                app.clone(),
                session_id.clone(),
                Value::Object(live_streams),
                receiver,
            )
        });
        let monitor_stop = Arc::new(AtomicBool::new(false));
        let monitor_handle = Some(spawn_monitor(
            self.core.clone(),
            app.clone(),
            meeting_id.clone(),
            session_id.clone(),
            manifest_path.clone(),
            tracks
                .iter()
                .map(|track| (track.kind, track.shared.clone()))
                .collect(),
            monitor_stop.clone(),
        ));
        let active = ActiveRecording {
            id: session_id.clone(),
            meeting_id: meeting_id.clone(),
            state: RecordingState::Recording,
            started_at_ms,
            started_instant: Instant::now(),
            paused_at: None,
            paused_total: Duration::ZERO,
            current_pause_id: None,
            tracks,
            manifest_path,
            monitor_stop,
            monitor_handle,
            caption_handle,
            qpc_start,
            qpc_frequency,
        };
        *self.active.lock() = Some(active);
        let connection = self.core.database().connect()?;
        connection.execute(
            "UPDATE recording_sessions SET state='recording' WHERE id=?1",
            [&session_id],
        )?;
        let session = self.session()?;
        let _ = app.emit("recording://state", &session);
        Ok(session)
    }

    pub fn pause(&self, session_id: &str, app: AppHandle) -> CoreResult<RecordingSession> {
        let mut active = self.active.lock();
        let recording = require_session_mut(&mut active, session_id)?;
        if recording.state != RecordingState::Recording {
            return Err(CoreError::Conflict(
                "only a recording in progress can be paused".into(),
            ));
        }
        ensure_capture_healthy(recording)?;
        let (sent, failures) =
            send_capture_control(&recording.tracks, CaptureCommand::Pause, "pause");
        if !failures.is_empty() {
            rollback_capture_control(
                &recording.tracks,
                &sent,
                CaptureCommand::Resume,
                "resume after failed pause",
            );
            for (source, message) in &failures {
                persist_capture_warning(
                    &self.core,
                    &app,
                    &recording.id,
                    &recording.manifest_path,
                    source,
                    message,
                );
            }
            return Err(CoreError::Audio(
                failures
                    .iter()
                    .map(|(_, message)| message.as_str())
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        recording.state = RecordingState::Paused;
        recording.paused_at = Some(Instant::now());
        let elapsed = elapsed_ms(recording);
        let pause_id = new_id();
        recording.current_pause_id = Some(pause_id.clone());
        let connection = self.core.database().connect()?;
        connection.execute(
            "UPDATE recording_sessions SET state='paused' WHERE id=?1",
            [session_id],
        )?;
        connection.execute(
            "INSERT INTO recording_pauses(id,session_id,started_offset_ms)
             VALUES (?1,?2,?3)",
            params![pause_id, session_id, elapsed],
        )?;
        append_manifest(
            &recording.manifest_path,
            json!({
                "type":"paused",
                "offsetMs":elapsed,
                "qpc":audio::query_performance_counter(),
                "createdAtMs":now_ms()
            }),
        )?;
        drop(active);
        let session = self.session()?;
        let _ = app.emit("recording://state", &session);
        Ok(session)
    }

    pub fn resume(&self, session_id: &str, app: AppHandle) -> CoreResult<RecordingSession> {
        let mut active = self.active.lock();
        let recording = require_session_mut(&mut active, session_id)?;
        if recording.state != RecordingState::Paused {
            return Err(CoreError::Conflict(
                "only a paused recording can be resumed".into(),
            ));
        }
        ensure_capture_healthy(recording)?;
        let (sent, failures) =
            send_capture_control(&recording.tracks, CaptureCommand::Resume, "resume");
        if !failures.is_empty() {
            rollback_capture_control(
                &recording.tracks,
                &sent,
                CaptureCommand::Pause,
                "pause after failed resume",
            );
            for (source, message) in &failures {
                persist_capture_warning(
                    &self.core,
                    &app,
                    &recording.id,
                    &recording.manifest_path,
                    source,
                    message,
                );
            }
            return Err(CoreError::Audio(
                failures
                    .iter()
                    .map(|(_, message)| message.as_str())
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        let paused_at = recording
            .paused_at
            .take()
            .ok_or_else(|| CoreError::Conflict("pause timing state is missing".into()))?;
        recording.paused_total += paused_at.elapsed();
        recording.state = RecordingState::Recording;
        let elapsed = elapsed_ms(recording);
        let connection = self.core.database().connect()?;
        connection.execute(
            "UPDATE recording_sessions
             SET state='recording',paused_duration_ms=?1 WHERE id=?2",
            params![recording.paused_total.as_millis() as i64, session_id],
        )?;
        if let Some(pause_id) = recording.current_pause_id.take() {
            connection.execute(
                "UPDATE recording_pauses SET ended_offset_ms=?1 WHERE id=?2",
                params![elapsed, pause_id],
            )?;
        }
        append_manifest(
            &recording.manifest_path,
            json!({
                "type":"resumed",
                "offsetMs":elapsed,
                "qpc":audio::query_performance_counter(),
                "createdAtMs":now_ms()
            }),
        )?;
        drop(active);
        let session = self.session()?;
        let _ = app.emit("recording://state", &session);
        Ok(session)
    }

    pub fn add_marker(
        &self,
        meeting_id: &str,
        at_ms: i64,
        label: String,
    ) -> CoreResult<RecordingMarker> {
        if at_ms < 0 {
            return Err(CoreError::InvalidInput(
                "marker time must not be negative".into(),
            ));
        }
        let meeting = self.core.get_meeting_summary(meeting_id)?;
        if at_ms > meeting.duration_ms.max(self.status().elapsed_ms + 2_000) {
            return Err(CoreError::InvalidInput(
                "marker time is beyond the meeting duration".into(),
            ));
        }
        let marker = RecordingMarker {
            id: new_id(),
            meeting_id: meeting_id.into(),
            at_ms,
            label: label.trim().chars().take(120).collect(),
            created_at_ms: now_ms(),
        };
        let connection = self.core.database().connect()?;
        connection.execute(
            "INSERT INTO recording_markers(id,meeting_id,offset_ms,label,created_at_ms)
             VALUES (?1,?2,?3,?4,?5)",
            params![
                marker.id,
                marker.meeting_id,
                marker.at_ms,
                marker.label,
                marker.created_at_ms
            ],
        )?;
        if let Some(active) = self.active.lock().as_ref() {
            if active.meeting_id == meeting_id {
                append_manifest(
                    &active.manifest_path,
                    json!({
                        "type":"marker",
                        "id":marker.id,
                        "offsetMs":marker.at_ms,
                        "label":marker.label,
                        "createdAtMs":marker.created_at_ms
                    }),
                )?;
            }
        }
        Ok(marker)
    }

    pub fn stop(&self, session_id: &str, app: AppHandle) -> CoreResult<Meeting> {
        let mut recording = {
            let mut active = self.active.lock();
            let current = active
                .as_ref()
                .ok_or_else(|| CoreError::Conflict("no recording is active".into()))?;
            if current.id != session_id {
                return Err(CoreError::Conflict(
                    "recording session id does not match the active session".into(),
                ));
            }
            let mut recording = active.take().unwrap();
            recording.state = RecordingState::Finalizing;
            recording
        };
        let finalizing = RecordingSession {
            id: recording.id.clone(),
            meeting_id: recording.meeting_id.clone(),
            state: RecordingState::Finalizing,
            elapsed_ms: elapsed_ms(&recording),
            started_at: iso_from_ms(recording.started_at_ms),
        };
        let _ = app.emit("recording://state", &finalizing);
        recording.monitor_stop.store(true, Ordering::Relaxed);
        if let Some(handle) = recording.monitor_handle.take() {
            let _ = handle.join();
        }
        let mut errors = Vec::new();
        for track in &recording.tracks {
            if let Err(error) = track.command_sender.try_send(CaptureCommand::Stop) {
                let message = format!(
                    "Could not stop {} capture cleanly: {error}",
                    track.kind.as_str()
                );
                track.shared.mark_failed(message.clone());
                persist_capture_warning(
                    &self.core,
                    &app,
                    &recording.id,
                    &recording.manifest_path,
                    track.kind.as_str(),
                    &message,
                );
                errors.push(message);
            }
        }
        let mut summaries = Vec::new();
        for track in &mut recording.tracks {
            if let Some(handle) = track.handle.take() {
                match handle.join() {
                    Ok(Ok(summary)) => summaries.push((track.track_id.clone(), summary)),
                    Ok(Err(error)) => {
                        let message = error.to_string();
                        persist_capture_warning(
                            &self.core,
                            &app,
                            &recording.id,
                            &recording.manifest_path,
                            track.kind.as_str(),
                            &message,
                        );
                        errors.push(message);
                    }
                    Err(_) => {
                        let message = format!("{} capture thread panicked", track.kind.as_str());
                        persist_capture_warning(
                            &self.core,
                            &app,
                            &recording.id,
                            &recording.manifest_path,
                            track.kind.as_str(),
                            &message,
                        );
                        errors.push(message);
                    }
                }
            }
        }
        // The caption queue is disposable; never let a stalled model worker block finalization.
        let _ = recording.caption_handle.take();
        let ended_at_ms = now_ms();
        let mut assets = Vec::new();
        for (track_id, summary) in &summaries {
            if summary.segment_paths.is_empty() {
                continue;
            }
            let clock_plan =
                build_clock_plan(summary, recording.qpc_start, recording.qpc_frequency);
            if clock_plan.unrepaired_discontinuities > 0 {
                errors.push(format!(
                    "{} contains {} discontinuities without a measurable clock gap",
                    summary.kind.as_str(),
                    clock_plan.unrepaired_discontinuities
                ));
            }
            match consolidate_track(
                self.core.layout(),
                &recording.meeting_id,
                &recording.id,
                summary,
                &clock_plan,
            ) {
                Ok(asset) => assets.push((track_id.clone(), summary, clock_plan, asset)),
                Err(error) => errors.push(error.to_string()),
            }
        }
        let playback = if assets.len() == 2 {
            match mix_tracks(
                self.core.layout(),
                &recording.meeting_id,
                &recording.id,
                &assets[0].3.path,
                &assets[1].3.path,
            ) {
                Ok(asset) => Some(asset),
                Err(error) => {
                    errors.push(error.to_string());
                    None
                }
            }
        } else {
            None
        };
        let duration_ms = assets
            .iter()
            .filter_map(|(_, _, _, asset)| asset.duration_ms)
            .max()
            .unwrap_or(0);

        {
            let mut connection = self.core.database().connect()?;
            let transaction = connection.transaction()?;
            for (track_id, summary, clock_plan, asset) in &assets {
                insert_recording_asset(&transaction, &recording.meeting_id, asset, ended_at_ms)?;
                transaction.execute(
                    "UPDATE recording_tracks SET
                        device_id=?1,final_asset_id=?2,dropped_packets=?3,discontinuities=?4,
                        qpc_first=?5,qpc_last=?6,frames_written=?7,clock_anchors_json=?8,
                        start_offset_ms=?9,clock_scale=?10
                     WHERE id=?11",
                    params![
                        summary.device_id,
                        asset.id,
                        summary.dropped_packets as i64,
                        summary.discontinuities as i64,
                        summary.qpc_first.map(|value| value as i64),
                        summary.qpc_last.map(|value| value as i64),
                        summary.samples_written as i64 / summary.channels as i64,
                        serde_json::to_string(&summary.clock_anchors)?,
                        clock_plan.start_offset_ms as i64,
                        clock_plan.duration_scale,
                        track_id
                    ],
                )?;
            }
            if let Some(asset) = &playback {
                insert_recording_asset(&transaction, &recording.meeting_id, asset, ended_at_ms)?;
            }
            let successful = !assets.is_empty();
            transaction.execute(
                "UPDATE recording_sessions SET
                    state=?1,ended_at_ms=?2,paused_duration_ms=?3,error_message=?4
                 WHERE id=?5",
                params![
                    if successful { "stopped" } else { "failed" },
                    ended_at_ms,
                    recording.paused_total.as_millis() as i64,
                    (!errors.is_empty()).then(|| errors.join("; ")),
                    recording.id
                ],
            )?;
            transaction.execute(
                "UPDATE meetings SET
                    status=?1,ended_at_ms=?2,duration_ms=?3,
                    needs_review=?4,recovery_warning=?5
                 WHERE id=?6",
                params![
                    if successful { "processing" } else { "failed" },
                    ended_at_ms,
                    duration_ms,
                    i64::from(!errors.is_empty()),
                    (!errors.is_empty()).then(|| errors.join("; ")),
                    recording.meeting_id
                ],
            )?;
            transaction.commit()?;
        }
        append_manifest(
            &recording.manifest_path,
            json!({
                "type":"recording_finalized",
                "endedAtMs":ended_at_ms,
                "durationMs":duration_ms,
                "assets":assets.iter().map(|(_,_,_,asset)| &asset.id).collect::<Vec<_>>(),
                "playbackAsset":playback.as_ref().map(|asset| &asset.id),
                "clockPlans":assets.iter().map(|(_,summary,plan,_)| json!({
                    "source":summary.kind.as_str(),
                    "startOffsetMs":plan.start_offset_ms,
                    "durationScale":plan.duration_scale,
                    "materializedGapFrames":plan.materialized_gap_frames,
                    "unrepairedDiscontinuities":plan.unrepaired_discontinuities,
                    "anchors":summary.clock_anchors,
                })).collect::<Vec<_>>(),
                "errors":errors,
            }),
        )?;
        if !assets.is_empty() {
            let _ = self.core.enqueue_final_pipeline(&recording.meeting_id)?;
        }
        let meeting = self.core.get_meeting_summary(&recording.meeting_id)?;
        let stopped = RecordingSession {
            id: recording.id,
            meeting_id: recording.meeting_id,
            state: RecordingState::Stopped,
            elapsed_ms: duration_ms,
            started_at: iso_from_ms(recording.started_at_ms),
        };
        let _ = app.emit("recording://state", stopped);
        Ok(meeting)
    }

    pub fn session(&self) -> CoreResult<RecordingSession> {
        let active = self.active.lock();
        let recording = active
            .as_ref()
            .ok_or_else(|| CoreError::Conflict("no recording is active".into()))?;
        Ok(RecordingSession {
            id: recording.id.clone(),
            meeting_id: recording.meeting_id.clone(),
            state: recording.state.clone(),
            elapsed_ms: elapsed_ms(recording),
            started_at: iso_from_ms(recording.started_at_ms),
        })
    }

    fn fail_start(
        &self,
        meeting_id: &str,
        session_id: &str,
        manifest: &Path,
        error: &CoreError,
    ) -> CoreResult<()> {
        let now = now_ms();
        let connection = self.core.database().connect()?;
        connection.execute(
            "UPDATE recording_sessions
             SET state='failed',ended_at_ms=?1,error_message=?2 WHERE id=?3",
            params![now, error.to_string(), session_id],
        )?;
        connection.execute(
            "UPDATE meetings SET status='failed',ended_at_ms=?1 WHERE id=?2",
            params![now, meeting_id],
        )?;
        append_manifest(
            manifest,
            json!({"type":"recording_failed","createdAtMs":now,"error":error.to_string()}),
        )?;
        Ok(())
    }
}

fn require_session_mut<'a>(
    active: &'a mut Option<ActiveRecording>,
    session_id: &str,
) -> CoreResult<&'a mut ActiveRecording> {
    let recording = active
        .as_mut()
        .ok_or_else(|| CoreError::Conflict("no recording is active".into()))?;
    if recording.id != session_id {
        return Err(CoreError::Conflict(
            "recording session id does not match the active session".into(),
        ));
    }
    Ok(recording)
}

fn status_from_active(active: &ActiveRecording) -> RecordingStatus {
    let microphone = active
        .tracks
        .iter()
        .find(|track| track.kind == CaptureKind::Microphone);
    let loopback = active
        .tracks
        .iter()
        .find(|track| track.kind == CaptureKind::Loopback);
    let failures = active
        .tracks
        .iter()
        .filter_map(|track| {
            track
                .shared
                .failure()
                .map(|error| format!("{}: {error}", track.kind.as_str()))
        })
        .collect::<Vec<_>>();
    let any_live = active.tracks.iter().any(|track| track.shared.is_live());
    RecordingStatus {
        state: if !failures.is_empty() && !any_live {
            RecordingState::Failed
        } else {
            active.state.clone()
        },
        session_id: Some(active.id.clone()),
        meeting_id: Some(active.meeting_id.clone()),
        elapsed_ms: elapsed_ms(active),
        microphone_active: microphone.is_some_and(|track| track.shared.is_live()),
        system_audio_active: loopback.is_some_and(|track| track.shared.is_live()),
        microphone_level: microphone
            .map(|track| track.shared.level())
            .unwrap_or_default(),
        system_audio_level: loopback
            .map(|track| track.shared.level())
            .unwrap_or_default(),
        dropped_capture_packets: active
            .tracks
            .iter()
            .map(|track| track.shared.dropped_packets.load(Ordering::Relaxed))
            .sum(),
        dropped_caption_chunks: active
            .tracks
            .iter()
            .map(|track| track.shared.dropped_caption_chunks.load(Ordering::Relaxed))
            .sum(),
        warning: (!failures.is_empty()).then(|| failures.join("; ")),
    }
}

fn live_source_type(kind: CaptureKind, microphone_is_personal: bool) -> &'static str {
    match (kind, microphone_is_personal) {
        (CaptureKind::Microphone, false) => "mixed",
        _ => kind.as_str(),
    }
}

fn elapsed_ms(recording: &ActiveRecording) -> i64 {
    let current_pause = recording
        .paused_at
        .map(|paused_at| paused_at.elapsed())
        .unwrap_or_default();
    recording
        .started_instant
        .elapsed()
        .saturating_sub(recording.paused_total + current_pause)
        .as_millis() as i64
}

fn stop_tracks(tracks: &mut [ActiveTrack]) {
    for track in tracks.iter() {
        let _ = track.command_sender.send(CaptureCommand::Stop);
    }
    for track in tracks.iter_mut() {
        if let Some(handle) = track.handle.take() {
            let _ = handle.join();
        }
    }
}

fn ensure_capture_healthy(recording: &ActiveRecording) -> CoreResult<()> {
    let mut failures = Vec::new();
    for track in &recording.tracks {
        if let Some(error) = track.shared.failure() {
            failures.push(format!("{}: {error}", track.kind.as_str()));
        } else if !track.shared.is_live() {
            let error = "capture stream ended unexpectedly".to_string();
            track.shared.mark_failed(error.clone());
            failures.push(format!("{}: {error}", track.kind.as_str()));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(CoreError::Audio(failures.join("; ")))
    }
}

fn send_capture_control(
    tracks: &[ActiveTrack],
    command: CaptureCommand,
    action: &str,
) -> (Vec<usize>, Vec<(String, String)>) {
    let mut sent = Vec::new();
    let mut failures = Vec::new();
    for (index, track) in tracks.iter().enumerate() {
        match track.command_sender.try_send(command) {
            Ok(()) => sent.push(index),
            Err(error) => {
                let message = format!(
                    "Could not {action} {} capture: {error}",
                    track.kind.as_str()
                );
                track.shared.mark_failed(message.clone());
                failures.push((track.kind.as_str().to_string(), message));
            }
        }
    }
    (sent, failures)
}

fn rollback_capture_control(
    tracks: &[ActiveTrack],
    sent: &[usize],
    command: CaptureCommand,
    action: &str,
) {
    for index in sent {
        let Some(track) = tracks.get(*index) else {
            continue;
        };
        if let Err(error) = track.command_sender.try_send(command) {
            track.shared.mark_failed(format!(
                "Could not {action} for {} capture: {error}",
                track.kind.as_str()
            ));
        }
    }
}

fn append_manifest(path: &Path, value: Value) -> CoreResult<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, &value)?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    Ok(())
}

fn persist_capture_warning(
    core: &CoreService,
    app: &AppHandle,
    session_id: &str,
    manifest_path: &Path,
    source: &str,
    message: &str,
) {
    if let Ok(connection) = core.database().connect() {
        if let Err(error) = connection.execute(
            "UPDATE recording_sessions SET
                error_message=CASE
                    WHEN error_message IS NULL OR error_message=''
                    THEN ?1 ELSE error_message || '; ' || ?1 END
             WHERE id=?2",
            params![message, session_id],
        ) {
            log::error!("could not persist capture warning: {error}");
        }
    }
    if let Err(error) = append_manifest(
        manifest_path,
        json!({
            "type":"capture_error",
            "source":source,
            "message":message,
            "createdAtMs":now_ms()
        }),
    ) {
        log::error!("could not append capture warning to manifest: {error}");
    }
    let _ = app.emit(
        "device://warning",
        json!({
            "deviceId":source,
            "code":"CAPTURE_FAILED",
            "message":message
        }),
    );
}

fn spawn_monitor(
    core: Arc<CoreService>,
    app: AppHandle,
    meeting_id: String,
    session_id: String,
    manifest_path: PathBuf,
    tracks: Vec<(CaptureKind, Arc<CaptureShared>)>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("recording-level-monitor".into())
        .spawn(move || {
            let mut ticks = 0_u32;
            let mut anchor_cursors = vec![0_usize; tracks.len()];
            let mut reported_failures = BTreeMap::<String, bool>::new();
            let mut terminal_failure_persisted = false;
            while !stop.load(Ordering::Relaxed) {
                let microphone = tracks
                    .iter()
                    .find(|(kind, _)| *kind == CaptureKind::Microphone)
                    .map(|(_, shared)| shared.level())
                    .unwrap_or_default();
                let system = tracks
                    .iter()
                    .find(|(kind, _)| *kind == CaptureKind::Loopback)
                    .map(|(_, shared)| shared.level())
                    .unwrap_or_default();
                let _ = app.emit(
                    "recording://levels",
                    json!({"microphone":microphone,"system":system}),
                );
                for (kind, shared) in &tracks {
                    let source = kind.as_str().to_string();
                    if reported_failures.contains_key(&source) {
                        continue;
                    }
                    if let Some(error) = shared.failure() {
                        let message = format!("{} capture failed: {error}", kind.as_str());
                        persist_capture_warning(
                            &core,
                            &app,
                            &session_id,
                            &manifest_path,
                            kind.as_str(),
                            &message,
                        );
                        reported_failures.insert(source, true);
                    }
                }
                if !terminal_failure_persisted
                    && !tracks.is_empty()
                    && tracks
                        .iter()
                        .all(|(_, shared)| !shared.is_live() && shared.failure().is_some())
                {
                    if let Ok(mut connection) = core.database().connect() {
                        if let Ok(transaction) = connection.transaction() {
                            let timestamp = now_ms();
                            let _ = transaction.execute(
                                "UPDATE recording_sessions
                                 SET state='failed',ended_at_ms=COALESCE(ended_at_ms,?1)
                                 WHERE id=?2",
                                params![timestamp, session_id],
                            );
                            let _ = transaction.execute(
                                "UPDATE meetings
                                 SET status='failed',ended_at_ms=COALESCE(ended_at_ms,?1)
                                 WHERE id=?2 AND status='recording'",
                                params![timestamp, meeting_id],
                            );
                            let _ = transaction.commit();
                        }
                    }
                    let _ = app.emit(
                        "meeting://changed",
                        json!({"meetingId":meeting_id,"reason":"recording_capture_failed"}),
                    );
                    terminal_failure_persisted = true;
                }
                if ticks % 50 == 0 {
                    let track_checkpoints = tracks
                        .iter()
                        .enumerate()
                        .map(|(index, (kind, shared))| {
                            let anchors = shared.clock_anchors();
                            let new_anchors = anchors
                                .get(anchor_cursors[index]..)
                                .unwrap_or_default()
                                .to_vec();
                            anchor_cursors[index] = anchors.len();
                            json!({
                                "source":kind.as_str(),
                                "samplesWritten":shared.samples_written.load(Ordering::Relaxed),
                                "droppedPackets":shared.dropped_packets.load(Ordering::Relaxed),
                                "discontinuities":shared.discontinuities.load(Ordering::Relaxed),
                                "qpcFirst":shared.qpc_first.load(Ordering::Relaxed),
                                "qpcLast":shared.qpc_last.load(Ordering::Relaxed),
                                "clockAnchors":new_anchors,
                            })
                        })
                        .collect::<Vec<_>>();
                    let checkpoint = json!({
                        "type":"capture_checkpoint",
                        "meetingId":meeting_id,
                        "sessionId":session_id,
                        "createdAtMs":now_ms(),
                        "tracks":track_checkpoints
                    });
                    if let Err(error) = append_manifest(&manifest_path, checkpoint) {
                        let _ = app.emit(
                            "device://warning",
                            json!({
                                "deviceId":"storage",
                                "code":"LOW_DISK",
                                "message":format!("Recording manifest checkpoint failed: {error}")
                            }),
                        );
                    }
                }
                ticks = ticks.wrapping_add(1);
                thread::sleep(Duration::from_millis(100));
            }
        })
        .expect("recording monitor thread creation failed")
}

fn spawn_caption_relay(
    worker: Arc<WorkerSupervisor>,
    app: AppHandle,
    session_id: String,
    streams: Value,
    receiver: mpsc::Receiver<CaptionChunk>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("live-caption-relay".into())
        .spawn(move || {
            let started = worker
                .request(WorkerRequest::new(
                    "live.start",
                    json!({
                        "session_id":session_id,
                        "streams":streams
                    }),
                ))
                .is_ok();
            let _ = app.emit(
                "worker://health",
                json!({
                    "status":if started {"ready"} else {"offline"},
                    "backend":"local"
                }),
            );
            loop {
                match receiver.recv_timeout(Duration::from_millis(50)) {
                    Ok(chunk) if started => {
                        let metadata = AudioMetadata {
                            session_id: chunk.session_id,
                            stream_id: chunk.stream_id,
                            sequence: chunk.sequence,
                            start_ms: chunk.start_ms,
                            sample_rate: chunk.sample_rate,
                            channels: chunk.channels,
                            sample_format: "s16le".into(),
                        };
                        if let Err(error) =
                            worker.send_live_audio(metadata, &chunk.pcm_s16le)
                        {
                            let _ = app.emit(
                                "worker://health",
                                json!({"status":"offline","backend":"local","error":error.to_string()}),
                            );
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
                for event in worker.drain_events() {
                    if event.event.as_deref() == Some("draft_revision") {
                        let _ = app.emit("transcript://draft-revision", event.payload);
                    } else if event.event.as_deref() == Some("device_warning") {
                        let _ = app.emit("device://warning", event.payload);
                    } else if event.event.as_deref() == Some("live_error") {
                        let _ = app.emit(
                            "worker://health",
                            json!({"status":"recovering","backend":"local","detail":event.payload}),
                        );
                    }
                }
            }
            let _ = worker.request(WorkerRequest::new(
                "live.stop",
                json!({"session_id":session_id}),
            ));
        })
        .expect("caption relay thread creation failed")
}

#[derive(Debug, Clone, PartialEq)]
struct TrackClockPlan {
    start_offset_ms: u64,
    duration_scale: f64,
    materialized_gap_frames: u64,
    unrepaired_discontinuities: u64,
}

fn build_clock_plan(
    summary: &CaptureSummary,
    session_qpc_start: Option<u64>,
    qpc_frequency: Option<u64>,
) -> TrackClockPlan {
    let frequency = qpc_frequency.filter(|value| *value > 0);
    let start_offset_ms = match (summary.qpc_first, session_qpc_start, frequency) {
        (Some(first), Some(start), Some(frequency)) if first >= start => {
            (((first - start) as u128 * 1000_u128) / frequency as u128) as u64
        }
        _ => 0,
    };
    let mut ratios = Vec::new();
    if let Some(frequency) = frequency {
        for anchors in summary.clock_anchors.windows(2) {
            let previous = &anchors[0];
            let current = &anchors[1];
            if current.pause_boundary
                || current.qpc <= previous.qpc
                || current.frame_index <= previous.frame_index
            {
                continue;
            }
            let qpc_seconds = (current.qpc - previous.qpc) as f64 / frequency as f64;
            let audio_seconds =
                (current.frame_index - previous.frame_index) as f64 / CAPTURE_SAMPLE_RATE as f64;
            let ratio = qpc_seconds / audio_seconds;
            if ratio.is_finite() && (0.98..=1.02).contains(&ratio) {
                ratios.push(ratio);
            }
        }
    }
    ratios.sort_by(f64::total_cmp);
    let duration_scale = ratios
        .get(ratios.len() / 2)
        .copied()
        .unwrap_or(1.0)
        .clamp(0.98, 1.02);
    let materialized_gap_frames = summary
        .clock_anchors
        .iter()
        .map(|anchor| anchor.inserted_gap_frames)
        .sum();
    let unrepaired_discontinuities = summary
        .clock_anchors
        .iter()
        .filter(|anchor| {
            anchor.discontinuity && !anchor.pause_boundary && anchor.inserted_gap_frames == 0
        })
        .count() as u64;
    TrackClockPlan {
        start_offset_ms,
        duration_scale,
        materialized_gap_frames,
        unrepaired_discontinuities,
    }
}

#[derive(Debug, Clone, Default)]
struct RecoveryCheckpoint {
    samples_written: u64,
    dropped_packets: u64,
    discontinuities: u64,
    qpc_first: Option<u64>,
    qpc_last: Option<u64>,
    clock_anchors: Vec<ClockAnchor>,
}

#[derive(Debug)]
struct RecoverableTrack {
    id: String,
    kind: CaptureKind,
    device_id: String,
    directory: PathBuf,
    channels: u16,
}

pub(crate) fn recover_interrupted_recordings(core: &CoreService) -> CoreResult<usize> {
    let connection = core.database().connect()?;
    let mut statement = connection.prepare(
        "SELECT id,meeting_id,config_json,manifest_relative_path,qpc_frequency,qpc_start
         FROM recording_sessions
         WHERE state IN ('starting','recording','paused','finalizing')
         ORDER BY started_at_ms,id",
    )?;
    let sessions = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    drop(connection);

    let mut recovered = 0_usize;
    for (session_id, meeting_id, config_json, manifest_relative, frequency, qpc_start) in sessions {
        let config =
            serde_json::from_str::<RecordingConfig>(&config_json).unwrap_or_else(|error| {
                log::warn!(
                    "recording {session_id} has invalid stored configuration ({error}); \
                     assuming the legacy personal-microphone defaults"
                );
                RecordingConfig::default()
            });
        let manifest_path = core.layout().root().join(&manifest_relative);
        let checkpoints = match read_recovery_checkpoints(&manifest_path) {
            Ok(checkpoints) => checkpoints,
            Err(error) => {
                log::warn!("could not read recording recovery checkpoints: {error}");
                BTreeMap::new()
            }
        };
        let connection = core.database().connect()?;
        let mut track_statement = connection.prepare(
            "SELECT id,source_kind,device_id,relative_directory,channels
             FROM recording_tracks
             WHERE session_id=?1
             ORDER BY created_at_ms,id",
        )?;
        let track_rows = track_statement
            .query_map([&session_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(track_statement);
        drop(connection);

        let mut tracks = Vec::new();
        for (id, source_kind, device_id, relative_directory, channels) in track_rows {
            let kind = match source_kind.as_str() {
                "microphone" => CaptureKind::Microphone,
                "loopback" => CaptureKind::Loopback,
                _ => continue,
            };
            let directory = core.layout().root().join(relative_directory);
            tracks.push(RecoverableTrack {
                id,
                kind,
                device_id: device_id.unwrap_or_else(|| "recovered-endpoint".into()),
                directory,
                channels: channels.max(1) as u16,
            });
        }

        let mut warnings = vec![
            "Recovered after the application exited during recording; verify the final audio"
                .to_string(),
        ];
        let mut recovered_assets = Vec::new();
        for track in &tracks {
            let segments = match recoverable_wav_segments(&track.directory) {
                Ok(segments) => segments,
                Err(error) => {
                    warnings.push(format!(
                        "{} segment directory could not be read: {error}",
                        track.kind.as_str()
                    ));
                    continue;
                }
            };
            if segments.is_empty() {
                continue;
            }
            let frames = match validate_recovery_segments(&segments, track.channels) {
                Ok(frames) => frames,
                Err(error) => {
                    warnings.push(format!(
                        "{} segments could not be recovered: {error}",
                        track.kind.as_str()
                    ));
                    continue;
                }
            };
            if frames == 0 {
                continue;
            }
            let checkpoint = checkpoints
                .get(track.kind.as_str())
                .cloned()
                .unwrap_or_default();
            let mut anchors = checkpoint
                .clock_anchors
                .into_iter()
                .filter(|anchor| anchor.frame_index <= frames)
                .collect::<Vec<_>>();
            anchors.sort_by_key(|anchor| (anchor.frame_index, anchor.qpc));
            anchors.dedup_by_key(|anchor| (anchor.frame_index, anchor.qpc));
            let qpc_first = checkpoint
                .qpc_first
                .or_else(|| anchors.first().map(|anchor| anchor.qpc));
            let qpc_last = checkpoint
                .qpc_last
                .or_else(|| anchors.last().map(|anchor| anchor.qpc));
            if anchors.is_empty() {
                if let (Some(first), Some(last)) = (qpc_first, qpc_last) {
                    anchors.push(ClockAnchor {
                        qpc: first,
                        frame_index: 0,
                        discontinuity: false,
                        pause_boundary: false,
                        inserted_gap_frames: 0,
                    });
                    anchors.push(ClockAnchor {
                        qpc: last,
                        frame_index: frames,
                        discontinuity: false,
                        pause_boundary: false,
                        inserted_gap_frames: 0,
                    });
                }
            }
            let summary = CaptureSummary {
                kind: track.kind,
                device_id: track.device_id.clone(),
                channels: track.channels,
                segment_paths: segments,
                samples_written: frames.saturating_mul(track.channels as u64),
                dropped_packets: checkpoint.dropped_packets,
                discontinuities: checkpoint.discontinuities,
                qpc_first,
                qpc_last,
                clock_anchors: anchors,
            };
            let plan = build_clock_plan(
                &summary,
                qpc_start.and_then(|value| u64::try_from(value).ok()),
                frequency.and_then(|value| u64::try_from(value).ok()),
            );
            if plan.unrepaired_discontinuities > 0 {
                warnings.push(format!(
                    "{} has {} discontinuities requiring review",
                    track.kind.as_str(),
                    plan.unrepaired_discontinuities
                ));
            }
            let (mut asset, used_fallback) =
                match consolidate_track(core.layout(), &meeting_id, &session_id, &summary, &plan) {
                    Ok(asset) => (asset, false),
                    Err(error) => {
                        log::warn!(
                            "compressed recovery failed for {} track in session {}: {error}",
                            track.kind.as_str(),
                            session_id
                        );
                        let compressed = core
                            .layout()
                            .recordings()
                            .join(&meeting_id)
                            .join(&session_id)
                            .join(format!("{}.flac", track.kind.as_str()));
                        let _ = fs::remove_file(compressed);
                        match consolidate_recovery_wav(
                            core.layout(),
                            &meeting_id,
                            &session_id,
                            &summary,
                            &plan,
                        ) {
                            Ok(asset) => (asset, true),
                            Err(fallback_error) => {
                                warnings.push(format!(
                                    "{} could not be consolidated: {fallback_error}",
                                    track.kind.as_str()
                                ));
                                continue;
                            }
                        }
                    }
                };
            asset.id =
                stable_recovery_id(&format!("recording:{session_id}:{}", track.kind.as_str()));
            if used_fallback {
                warnings.push(format!(
                    "{} was recovered as PCM without drift resampling because compressed \
                     finalization was unavailable",
                    track.kind.as_str()
                ));
            }
            recovered_assets.push((track, summary, plan, asset));
        }

        let expected_kinds = [
            config.capture_microphone.then_some(CaptureKind::Microphone),
            config.capture_system_audio.then_some(CaptureKind::Loopback),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        let missing = expected_kinds
            .iter()
            .filter(|kind| {
                !recovered_assets
                    .iter()
                    .any(|(_, summary, _, _)| summary.kind == **kind)
            })
            .map(|kind| kind.as_str())
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            warnings.push(format!(
                "No recoverable audio was found for: {}",
                missing.join(", ")
            ));
        }

        let mut playback = if recovered_assets.len() >= 2 {
            match mix_tracks(
                core.layout(),
                &meeting_id,
                &session_id,
                &recovered_assets[0].3.path,
                &recovered_assets[1].3.path,
            ) {
                Ok(mut asset) => {
                    asset.id = stable_recovery_id(&format!("recording:{session_id}:playback"));
                    Some(asset)
                }
                Err(error) => {
                    warnings.push(format!("Recovered tracks could not be mixed: {error}"));
                    None
                }
            }
        } else {
            None
        };
        let duration_ms = recovered_assets
            .iter()
            .filter_map(|(_, _, _, asset)| asset.duration_ms)
            .chain(playback.iter().filter_map(|asset| asset.duration_ms))
            .max()
            .unwrap_or_default();
        let ended_at_ms = now_ms();
        let warning = warnings.join("; ");
        let has_audio = !recovered_assets.is_empty();
        let complete = has_audio && missing.is_empty();
        let mut connection = core.database().connect()?;
        let transaction = connection.transaction()?;
        for (track, summary, plan, asset) in &recovered_assets {
            let asset_id = ensure_recording_asset(&transaction, &meeting_id, asset, ended_at_ms)?;
            transaction.execute(
                "UPDATE recording_tracks SET
                    device_id=?1,final_asset_id=?2,dropped_packets=?3,discontinuities=?4,
                    qpc_first=?5,qpc_last=?6,frames_written=?7,clock_anchors_json=?8,
                    start_offset_ms=?9,clock_scale=?10
                 WHERE id=?11",
                params![
                    summary.device_id,
                    asset_id,
                    summary.dropped_packets as i64,
                    summary.discontinuities as i64,
                    summary
                        .qpc_first
                        .and_then(|value| i64::try_from(value).ok()),
                    summary.qpc_last.and_then(|value| i64::try_from(value).ok()),
                    summary.samples_written as i64 / summary.channels as i64,
                    serde_json::to_string(&summary.clock_anchors)?,
                    plan.start_offset_ms as i64,
                    plan.duration_scale,
                    track.id
                ],
            )?;
        }
        if let Some(asset) = playback.as_mut() {
            asset.id = ensure_recording_asset(&transaction, &meeting_id, asset, ended_at_ms)?;
        }
        transaction.execute(
            "UPDATE recording_sessions
             SET state=?1,ended_at_ms=?2,error_message=?3
             WHERE id=?4",
            params![
                if complete { "stopped" } else { "failed" },
                ended_at_ms,
                warning,
                session_id
            ],
        )?;
        transaction.execute(
            "UPDATE meetings SET
                status=?1,ended_at_ms=COALESCE(ended_at_ms,?2),duration_ms=?3,
                needs_review=1,recovery_warning=?4
             WHERE id=?5",
            params![
                if has_audio { "processing" } else { "failed" },
                ended_at_ms,
                duration_ms,
                warning,
                meeting_id
            ],
        )?;
        if has_audio {
            enqueue_recovery_job(&transaction, &meeting_id, &session_id, ended_at_ms)?;
        }
        transaction.commit()?;
        if let Err(error) = append_manifest(
            &manifest_path,
            json!({
                "type":"recording_recovered",
                "endedAtMs":ended_at_ms,
                "complete":complete,
                "assets":recovered_assets.iter().map(|(_,_,_,asset)| &asset.id).collect::<Vec<_>>(),
                "playbackAsset":playback.as_ref().map(|asset| &asset.id),
                "warning":warning,
            }),
        ) {
            log::warn!("could not append recording recovery manifest: {error}");
        }
        recovered += 1;
    }
    Ok(recovered)
}

fn read_recovery_checkpoints(path: &Path) -> CoreResult<BTreeMap<String, RecoveryCheckpoint>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.into()),
    };
    let mut checkpoints = BTreeMap::<String, RecoveryCheckpoint>::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.len() > 8 * 1024 * 1024 {
            return Err(CoreError::InvalidInput(
                "recording manifest checkpoint is too large".into(),
            ));
        }
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                log::warn!("ignoring incomplete recording manifest line: {error}");
                continue;
            }
        };
        if value.get("type").and_then(Value::as_str) != Some("capture_checkpoint") {
            continue;
        }
        let Some(tracks) = value.get("tracks").and_then(Value::as_array) else {
            continue;
        };
        for track in tracks {
            let Some(source) = track.get("source").and_then(Value::as_str) else {
                continue;
            };
            let checkpoint = checkpoints.entry(source.to_string()).or_default();
            checkpoint.samples_written = track
                .get("samplesWritten")
                .and_then(Value::as_u64)
                .unwrap_or(checkpoint.samples_written);
            checkpoint.dropped_packets = track
                .get("droppedPackets")
                .and_then(Value::as_u64)
                .unwrap_or(checkpoint.dropped_packets);
            checkpoint.discontinuities = track
                .get("discontinuities")
                .and_then(Value::as_u64)
                .unwrap_or(checkpoint.discontinuities);
            checkpoint.qpc_first = nonzero_json_u64(track.get("qpcFirst")).or(checkpoint.qpc_first);
            checkpoint.qpc_last = nonzero_json_u64(track.get("qpcLast")).or(checkpoint.qpc_last);
            if let Some(anchors) = track.get("clockAnchors").and_then(Value::as_array) {
                for anchor in anchors {
                    match serde_json::from_value::<ClockAnchor>(anchor.clone()) {
                        Ok(anchor) => checkpoint.clock_anchors.push(anchor),
                        Err(error) => log::warn!("ignoring invalid clock anchor: {error}"),
                    }
                }
            }
        }
    }
    Ok(checkpoints)
}

fn nonzero_json_u64(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64).filter(|value| *value > 0)
}

fn recoverable_wav_segments(directory: &Path) -> CoreResult<Vec<PathBuf>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().and_then(|value| value.to_str()) == Some("wav")
                && path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .is_some_and(|name| name.starts_with("segment-"))
        })
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn validate_recovery_segments(paths: &[PathBuf], channels: u16) -> CoreResult<u64> {
    let mut frames = 0_u64;
    for path in paths {
        let reader = hound::WavReader::open(path).map_err(|error| {
            CoreError::Audio(format!("recovered WAV segment is invalid: {error}"))
        })?;
        let spec = reader.spec();
        if spec.sample_rate != CAPTURE_SAMPLE_RATE
            || spec.channels != channels
            || spec.bits_per_sample != 16
            || spec.sample_format != hound::SampleFormat::Int
        {
            return Err(CoreError::Audio(format!(
                "recovered WAV segment {:?} has an unexpected format",
                path
            )));
        }
        frames = frames.saturating_add(reader.duration() as u64);
    }
    Ok(frames)
}

fn consolidate_recovery_wav(
    layout: &crate::layout::AppLayout,
    meeting_id: &str,
    session_id: &str,
    summary: &CaptureSummary,
    plan: &TrackClockPlan,
) -> CoreResult<ConsolidatedAsset> {
    let directory = layout.recordings().join(meeting_id).join(session_id);
    let destination = directory.join(format!("{}-recovered.wav", summary.kind.as_str()));
    let partial = PathBuf::from(format!("{}.partial", destination.to_string_lossy()));
    let _ = fs::remove_file(&partial);
    let spec = hound::WavSpec {
        channels: summary.channels,
        sample_rate: CAPTURE_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&partial, spec)
        .map_err(|error| CoreError::Audio(format!("recovery WAV create failed: {error}")))?;
    let leading_samples = plan
        .start_offset_ms
        .saturating_mul(CAPTURE_SAMPLE_RATE as u64)
        .saturating_mul(summary.channels as u64)
        / 1000;
    for _ in 0..leading_samples {
        writer
            .write_sample(0_i16)
            .map_err(|error| CoreError::Audio(format!("recovery WAV write failed: {error}")))?;
    }
    for path in &summary.segment_paths {
        let mut reader = hound::WavReader::open(path)
            .map_err(|error| CoreError::Audio(format!("recovery WAV read failed: {error}")))?;
        for sample in reader.samples::<i16>() {
            writer
                .write_sample(sample.map_err(|error| {
                    CoreError::Audio(format!("recovery WAV sample failed: {error}"))
                })?)
                .map_err(|error| CoreError::Audio(format!("recovery WAV write failed: {error}")))?;
        }
    }
    writer
        .finalize()
        .map_err(|error| CoreError::Audio(format!("recovery WAV finalize failed: {error}")))?;
    let _ = fs::remove_file(&destination);
    fs::rename(&partial, &destination)?;
    let frames = summary.samples_written / summary.channels as u64
        + leading_samples / summary.channels as u64;
    Ok(ConsolidatedAsset {
        id: new_id(),
        kind: summary.kind.as_str().into(),
        display_name: format!("{}-recovered.wav", summary.kind.as_str()),
        relative_path: layout.relative_to_root(&destination)?,
        content_type: "audio/wav".into(),
        size_bytes: fs::metadata(&destination)?.len(),
        sha256: media::sha256_file(&destination)?,
        duration_ms: Some((frames.saturating_mul(1000) / CAPTURE_SAMPLE_RATE as u64) as i64),
        codec: Some("pcm_s16le".into()),
        sample_rate_hz: Some(CAPTURE_SAMPLE_RATE),
        channels: Some(summary.channels),
        path: destination,
    })
}

fn stable_recovery_id(key: &str) -> String {
    Uuid::new_v5(&Uuid::NAMESPACE_URL, key.as_bytes()).to_string()
}

fn ensure_recording_asset(
    transaction: &Transaction<'_>,
    meeting_id: &str,
    asset: &ConsolidatedAsset,
    created_at_ms: i64,
) -> CoreResult<String> {
    if let Some(existing) = transaction
        .query_row(
            "SELECT id FROM media_assets WHERE relative_path=?1",
            [&asset.relative_path],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(existing);
    }
    insert_recording_asset(transaction, meeting_id, asset, created_at_ms)?;
    Ok(asset.id.clone())
}

fn enqueue_recovery_job(
    transaction: &Transaction<'_>,
    meeting_id: &str,
    session_id: &str,
    created_at_ms: i64,
) -> CoreResult<()> {
    let existing: Option<String> = transaction
        .query_row(
            "SELECT id FROM processing_jobs
             WHERE meeting_id=?1
               AND status IN ('queued','running','retry_wait','interrupted','cancel_requested')
             ORDER BY created_at_ms DESC LIMIT 1",
            [meeting_id],
            |row| row.get(0),
        )
        .optional()?;
    if existing.is_some() {
        return Ok(());
    }
    let id = stable_recovery_id(&format!("recording:{session_id}:final-pipeline"));
    transaction.execute(
        "INSERT OR IGNORE INTO processing_jobs(
            id,meeting_id,stage,status,progress,input_json,created_at_ms,updated_at_ms
         ) VALUES (?1,?2,'normalize','queued',0,?3,?4,?4)",
        params![
            id,
            meeting_id,
            json!({
                "meetingId":meeting_id,
                "pipelineVersion":crate::worker::PIPELINE_VERSION,
                "recoveredSessionId":session_id
            })
            .to_string(),
            created_at_ms
        ],
    )?;
    Ok(())
}

struct ConsolidatedAsset {
    id: String,
    kind: String,
    display_name: String,
    path: PathBuf,
    relative_path: String,
    content_type: String,
    size_bytes: u64,
    sha256: String,
    duration_ms: Option<i64>,
    codec: Option<String>,
    sample_rate_hz: Option<u32>,
    channels: Option<u16>,
}

fn consolidate_track(
    layout: &crate::layout::AppLayout,
    meeting_id: &str,
    session_id: &str,
    summary: &CaptureSummary,
    clock_plan: &TrackClockPlan,
) -> CoreResult<ConsolidatedAsset> {
    let kind = summary.kind.as_str();
    let directory = layout.recordings().join(meeting_id).join(session_id);
    let list_path = directory.join(format!("{kind}-segments.txt"));
    let list = summary
        .segment_paths
        .iter()
        .map(|path| format!("file '{}'\n", path.to_string_lossy().replace('\'', "'\\''")))
        .collect::<String>();
    fs::write(&list_path, list)?;
    let destination = directory.join(format!("{kind}.flac"));
    let filter = format!(
        "asetpts=PTS-STARTPTS,asetrate={:.6},aresample={},adelay={}:all=1",
        CAPTURE_SAMPLE_RATE as f64 / clock_plan.duration_scale,
        CAPTURE_SAMPLE_RATE,
        clock_plan.start_offset_ms
    );
    let partial = format!("{}.partial", destination.to_string_lossy());
    let _ = fs::remove_file(&partial);
    run_ffmpeg_owned(
        layout,
        &[
            "-y".into(),
            "-v".into(),
            "error".into(),
            "-f".into(),
            "concat".into(),
            "-safe".into(),
            "0".into(),
            "-i".into(),
            list_path.to_string_lossy().into_owned(),
            "-vn".into(),
            "-af".into(),
            filter,
            "-c:a".into(),
            "flac".into(),
            "-compression_level".into(),
            "5".into(),
            "-f".into(),
            "flac".into(),
            partial.clone(),
        ],
    )?;
    let _ = fs::remove_file(&destination);
    fs::rename(partial, &destination)?;
    let _ = fs::remove_file(list_path);
    consolidated_from_path(layout, destination, kind.into(), format!("{kind}.flac"))
}

fn mix_tracks(
    layout: &crate::layout::AppLayout,
    meeting_id: &str,
    session_id: &str,
    first: &Path,
    second: &Path,
) -> CoreResult<ConsolidatedAsset> {
    let destination = layout
        .recordings()
        .join(meeting_id)
        .join(session_id)
        .join("playback.flac");
    let partial = format!("{}.partial", destination.to_string_lossy());
    let _ = fs::remove_file(&partial);
    run_ffmpeg(
        layout,
        [
            "-y",
            "-v",
            "error",
            "-i",
            &first.to_string_lossy(),
            "-i",
            &second.to_string_lossy(),
            "-filter_complex",
            "amix=inputs=2:duration=longest:dropout_transition=0",
            "-c:a",
            "flac",
            "-compression_level",
            "5",
            "-f",
            "flac",
            &partial,
        ],
    )?;
    let _ = fs::remove_file(&destination);
    fs::rename(partial, &destination)?;
    consolidated_from_path(
        layout,
        destination,
        "playback".into(),
        "playback.flac".into(),
    )
}

fn consolidated_from_path(
    layout: &crate::layout::AppLayout,
    path: PathBuf,
    kind: String,
    display_name: String,
) -> CoreResult<ConsolidatedAsset> {
    let probe = media::probe(layout, &path)?;
    Ok(ConsolidatedAsset {
        id: new_id(),
        kind,
        display_name,
        relative_path: layout.relative_to_root(&path)?,
        content_type: probe.content_type,
        size_bytes: fs::metadata(&path)?.len(),
        sha256: media::sha256_file(&path)?,
        duration_ms: probe.duration_ms,
        codec: probe.codec,
        sample_rate_hz: probe.sample_rate_hz,
        channels: probe.channels,
        path,
    })
}

fn insert_recording_asset(
    transaction: &rusqlite::Transaction<'_>,
    meeting_id: &str,
    asset: &ConsolidatedAsset,
    created_at_ms: i64,
) -> CoreResult<()> {
    transaction.execute(
        "INSERT INTO media_assets(
            id,meeting_id,kind,display_name,relative_path,content_type,size_bytes,sha256,
            duration_ms,codec,sample_rate_hz,channels,created_at_ms
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        params![
            asset.id,
            meeting_id,
            asset.kind,
            asset.display_name,
            asset.relative_path,
            asset.content_type,
            asset.size_bytes as i64,
            asset.sha256,
            asset.duration_ms,
            asset.codec,
            asset.sample_rate_hz,
            asset.channels,
            created_at_ms
        ],
    )?;
    Ok(())
}

fn run_ffmpeg<'a>(
    layout: &crate::layout::AppLayout,
    arguments: impl IntoIterator<Item = &'a str>,
) -> CoreResult<()> {
    let mut command = Command::new(media_tools::resolve(layout, MediaTool::Ffmpeg)?);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let output = command
        .output()
        .map_err(|error| CoreError::Media(format!("ffmpeg could not start: {error}")))?;
    if !output.status.success() {
        return Err(CoreError::Media(format!(
            "ffmpeg recording finalization failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn run_ffmpeg_owned(layout: &crate::layout::AppLayout, arguments: &[String]) -> CoreResult<()> {
    let mut command = Command::new(media_tools::resolve(layout, MediaTool::Ffmpeg)?);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let output = command
        .output()
        .map_err(|error| CoreError::Media(format!("ffmpeg could not start: {error}")))?;
    if !output.status.success() {
        return Err(CoreError::Media(format!(
            "ffmpeg recording finalization failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn new_id() -> String {
    Uuid::now_v7().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(
        channels: u16,
        frames: u64,
        qpc_first: u64,
        qpc_last: u64,
        anchors: Vec<ClockAnchor>,
    ) -> CaptureSummary {
        CaptureSummary {
            kind: CaptureKind::Microphone,
            device_id: "device".into(),
            channels,
            segment_paths: Vec::new(),
            samples_written: frames * channels as u64,
            dropped_packets: 0,
            discontinuities: 0,
            qpc_first: Some(qpc_first),
            qpc_last: Some(qpc_last),
            clock_anchors: anchors,
        }
    }

    #[test]
    fn elapsed_time_excludes_current_and_completed_pauses() {
        let recording = ActiveRecording {
            id: "s".into(),
            meeting_id: "m".into(),
            state: RecordingState::Paused,
            started_at_ms: now_ms(),
            started_instant: Instant::now() - Duration::from_secs(10),
            paused_at: Some(Instant::now() - Duration::from_secs(2)),
            paused_total: Duration::from_secs(3),
            current_pause_id: None,
            tracks: Vec::new(),
            manifest_path: PathBuf::new(),
            monitor_stop: Arc::new(AtomicBool::new(false)),
            monitor_handle: None,
            caption_handle: None,
            qpc_start: None,
            qpc_frequency: None,
        };
        assert!((4_900..=5_100).contains(&elapsed_ms(&recording)));
    }

    #[test]
    fn clock_plan_corrects_two_hour_endpoint_drift_below_fifty_ms() {
        let frequency = 10_000_000_u64;
        let duration_seconds = 2 * 60 * 60_u64;
        let start = 50_000_000_u64;
        let end = start + duration_seconds * frequency;
        let mic_frames = duration_seconds * CAPTURE_SAMPLE_RATE as u64;
        let loopback_frames = mic_frames + mic_frames / 5_000; // 200 ppm fast.
        let mic = summary(
            1,
            mic_frames,
            start,
            end,
            vec![
                ClockAnchor {
                    qpc: start,
                    frame_index: 0,
                    discontinuity: false,
                    pause_boundary: false,
                    inserted_gap_frames: 0,
                },
                ClockAnchor {
                    qpc: end,
                    frame_index: mic_frames,
                    discontinuity: false,
                    pause_boundary: false,
                    inserted_gap_frames: 0,
                },
            ],
        );
        let loopback = summary(
            2,
            loopback_frames,
            start,
            end,
            vec![
                ClockAnchor {
                    qpc: start,
                    frame_index: 0,
                    discontinuity: false,
                    pause_boundary: false,
                    inserted_gap_frames: 0,
                },
                ClockAnchor {
                    qpc: end,
                    frame_index: loopback_frames,
                    discontinuity: false,
                    pause_boundary: false,
                    inserted_gap_frames: 0,
                },
            ],
        );
        let mic_plan = build_clock_plan(&mic, Some(start), Some(frequency));
        let loopback_plan = build_clock_plan(&loopback, Some(start), Some(frequency));
        let mic_corrected_ms =
            mic_frames as f64 / CAPTURE_SAMPLE_RATE as f64 * mic_plan.duration_scale * 1000.0;
        let loopback_corrected_ms = loopback_frames as f64 / CAPTURE_SAMPLE_RATE as f64
            * loopback_plan.duration_scale
            * 1000.0;
        assert!((mic_corrected_ms - loopback_corrected_ms).abs() < 50.0);
    }

    #[test]
    fn clock_plan_preserves_start_offset_and_classifies_gaps() {
        let frequency = 10_000_000_u64;
        let session_start = 100_000_000_u64;
        let first = session_start + frequency / 10;
        let anchors = vec![
            ClockAnchor {
                qpc: first,
                frame_index: 0,
                discontinuity: false,
                pause_boundary: false,
                inserted_gap_frames: 0,
            },
            ClockAnchor {
                qpc: first + 2 * frequency,
                frame_index: 2 * CAPTURE_SAMPLE_RATE as u64,
                discontinuity: true,
                pause_boundary: false,
                inserted_gap_frames: CAPTURE_SAMPLE_RATE as u64,
            },
        ];
        let recovered_gap = summary(
            1,
            2 * CAPTURE_SAMPLE_RATE as u64,
            first,
            first + 2 * frequency,
            anchors,
        );
        let plan = build_clock_plan(&recovered_gap, Some(session_start), Some(frequency));
        assert_eq!(plan.start_offset_ms, 100);
        assert_eq!(plan.materialized_gap_frames, CAPTURE_SAMPLE_RATE as u64);
        assert_eq!(plan.unrepaired_discontinuities, 0);

        let mut unrepaired = recovered_gap;
        unrepaired.clock_anchors[1].inserted_gap_frames = 0;
        assert_eq!(
            build_clock_plan(&unrepaired, Some(session_start), Some(frequency))
                .unrepaired_discontinuities,
            1
        );
    }

    #[test]
    fn room_microphone_uses_anonymous_live_speaker_hint() {
        assert_eq!(
            live_source_type(CaptureKind::Microphone, true),
            "microphone"
        );
        assert_eq!(live_source_type(CaptureKind::Microphone, false), "mixed");
        assert_eq!(live_source_type(CaptureKind::Loopback, false), "loopback");
    }

    #[test]
    fn recording_status_surfaces_capture_failure_and_liveness() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let shared = Arc::new(CaptureShared::default());
        shared.mark_live();
        let recording = ActiveRecording {
            id: "session".into(),
            meeting_id: "meeting".into(),
            state: RecordingState::Recording,
            started_at_ms: now_ms(),
            started_instant: Instant::now(),
            paused_at: None,
            paused_total: Duration::ZERO,
            current_pause_id: None,
            tracks: vec![ActiveTrack {
                track_id: "track".into(),
                kind: CaptureKind::Microphone,
                command_sender: sender,
                handle: None,
                shared: shared.clone(),
            }],
            manifest_path: PathBuf::new(),
            monitor_stop: Arc::new(AtomicBool::new(false)),
            monitor_handle: None,
            caption_handle: None,
            qpc_start: None,
            qpc_frequency: None,
        };
        let healthy = status_from_active(&recording);
        assert!(healthy.microphone_active);
        assert!(healthy.warning.is_none());

        shared.mark_failed("device removed");
        let failed = status_from_active(&recording);
        assert!(!failed.microphone_active);
        assert_eq!(failed.state, RecordingState::Failed);
        assert!(failed
            .warning
            .as_deref()
            .unwrap()
            .contains("device removed"));
    }

    #[test]
    fn failed_control_send_is_actionable() {
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(receiver);
        let shared = Arc::new(CaptureShared::default());
        shared.mark_live();
        let track = ActiveTrack {
            track_id: "track".into(),
            kind: CaptureKind::Microphone,
            command_sender: sender,
            handle: None,
            shared: shared.clone(),
        };

        let (_, failures) = send_capture_control(&[track], CaptureCommand::Pause, "pause");

        assert_eq!(failures.len(), 1);
        assert!(!shared.is_live());
        assert!(shared
            .failure()
            .as_deref()
            .unwrap()
            .contains("Could not pause microphone capture"));
    }

    #[test]
    fn interrupted_partial_recording_becomes_visible_and_processable_idempotently() {
        let temp = tempfile::tempdir().unwrap();
        let core = CoreService::open(temp.path()).unwrap();
        let meeting_id = new_id();
        let session_id = new_id();
        let track_id = new_id();
        let session_directory = core
            .layout()
            .recordings()
            .join(&meeting_id)
            .join(&session_id);
        let track_directory = session_directory.join("microphone");
        fs::create_dir_all(&track_directory).unwrap();
        let segment = track_directory.join("segment-00001.wav");
        let mut writer = hound::WavWriter::create(
            &segment,
            hound::WavSpec {
                channels: 1,
                sample_rate: CAPTURE_SAMPLE_RATE,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for _ in 0..4_800 {
            writer.write_sample(0_i16).unwrap();
        }
        writer.finalize().unwrap();
        let manifest = session_directory.join("recording-manifest.jsonl");
        let frequency = 10_000_000_u64;
        let start = 100_000_000_u64;
        append_manifest(
            &manifest,
            json!({
                "type":"capture_checkpoint",
                "tracks":[{
                    "source":"microphone",
                    "samplesWritten":4_800,
                    "droppedPackets":0,
                    "discontinuities":0,
                    "qpcFirst":start,
                    "qpcLast":start + frequency / 10,
                    "clockAnchors":[
                        {
                            "qpc":start,
                            "frameIndex":0,
                            "discontinuity":false,
                            "pauseBoundary":false,
                            "insertedGapFrames":0
                        },
                        {
                            "qpc":start + frequency / 10,
                            "frameIndex":4_800,
                            "discontinuity":false,
                            "pauseBoundary":false,
                            "insertedGapFrames":0
                        }
                    ]
                }]
            }),
        )
        .unwrap();
        let config = RecordingConfig {
            capture_system_audio: true,
            ..Default::default()
        };
        let now = now_ms();
        let connection = core.database().connect().unwrap();
        connection
            .execute(
                "INSERT INTO meetings(
                    id,title,source_kind,status,created_at_ms,started_at_ms
                 ) VALUES (?1,'Recovered','recording','recording',?2,?2)",
                params![meeting_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO recording_sessions(
                    id,meeting_id,state,config_json,manifest_relative_path,
                    qpc_frequency,qpc_start,started_at_ms
                 ) VALUES (?1,?2,'recording',?3,?4,?5,?6,?7)",
                params![
                    session_id,
                    meeting_id,
                    serde_json::to_string(&config).unwrap(),
                    core.layout().relative_to_root(&manifest).unwrap(),
                    frequency as i64,
                    start as i64,
                    now
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO recording_tracks(
                    id,session_id,source_kind,device_id,relative_directory,
                    sample_rate_hz,channels,created_at_ms
                 ) VALUES (?1,?2,'microphone','mic',?3,?4,1,?5)",
                params![
                    track_id,
                    session_id,
                    core.layout().relative_to_root(&track_directory).unwrap(),
                    CAPTURE_SAMPLE_RATE,
                    now
                ],
            )
            .unwrap();
        drop(connection);

        core.recover_interrupted_work().unwrap();
        let meeting = core.get_meeting_summary(&meeting_id).unwrap();
        assert_eq!(meeting.status, crate::models::MeetingStatus::Processing);
        assert!(meeting.needs_review);
        assert!(meeting
            .recovery_warning
            .as_deref()
            .unwrap()
            .contains("loopback"));
        let connection = core.database().connect().unwrap();
        let session_state: String = connection
            .query_row(
                "SELECT state FROM recording_sessions WHERE id=?1",
                [&session_id],
                |row| row.get(0),
            )
            .unwrap();
        let asset_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM media_assets WHERE meeting_id=?1",
                [&meeting_id],
                |row| row.get(0),
            )
            .unwrap();
        let job_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM processing_jobs WHERE meeting_id=?1 AND status='queued'",
                [&meeting_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(session_state, "failed");
        assert!(asset_count >= 1);
        assert_eq!(job_count, 1);
        drop(connection);

        core.recover_interrupted_work().unwrap();
        let connection = core.database().connect().unwrap();
        let counts: (i64, i64) = connection
            .query_row(
                "SELECT
                    (SELECT count(*) FROM media_assets WHERE meeting_id=?1),
                    (SELECT count(*) FROM processing_jobs WHERE meeting_id=?1)",
                [&meeting_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(counts, (asset_count, 1));
    }
}
