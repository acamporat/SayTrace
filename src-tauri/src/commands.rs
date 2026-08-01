use std::{fs::OpenOptions, io::Write};

use tauri::{AppHandle, Emitter, State};
use tauri_plugin_dialog::DialogExt;
use zeroize::Zeroize;

use crate::{
    error::{ApiError, CommandResult, CoreError},
    models::*,
    AppState,
};

#[tauri::command]
pub fn get_app_status(state: State<'_, AppState>) -> CommandResult<AppStatus> {
    let model = state.core.model_status();
    let first_run = state.core.first_run().map_err(ApiError::from)?;
    let worker = state.worker.status();
    let model_ready = model.runtime == "ready"
        && model.live_model == "ready"
        && model.final_model == "ready"
        && model.diarization_model == "ready";
    let worker_ready = worker.state == "ready"
        && worker.protocol_version.to_string() == crate::worker::PROTOCOL_VERSION
        && worker.pipeline_version == crate::worker::PIPELINE_VERSION;
    Ok(AppStatus {
        app_version: env!("CARGO_PKG_VERSION").into(),
        schema_version: state.core.schema_version(),
        first_run,
        model_ready,
        // File presence is enough to guide first-run setup, but offline-ready
        // additionally requires a live, protocol-compatible worker handshake.
        // Model hashes are verified during setup and again by the worker before
        // use; this inexpensive status call intentionally does not rehash GiB
        // of model data on every renderer launch.
        offline_ready: model_ready && worker_ready,
        active_recording: state.recording.status(),
        worker,
        model_revisions: state.core.model_revisions(),
        capabilities: [
            ("platform".into(), serde_json::json!("windows")),
            ("language".into(), serde_json::json!("en")),
            ("wasapiLoopback".into(), serde_json::json!(cfg!(windows))),
            (
                "voiceProfilesDpapi".into(),
                serde_json::json!(cfg!(windows)),
            ),
        ]
        .into_iter()
        .collect(),
    })
}

#[tauri::command]
pub fn get_library_stats(state: State<'_, AppState>) -> CommandResult<LibraryStats> {
    state.core.library_stats().map_err(ApiError::from)
}

#[tauri::command]
pub fn import_media(app: AppHandle, state: State<'_, AppState>) -> CommandResult<Option<Meeting>> {
    let selected = app
        .dialog()
        .file()
        .add_filter(
            "Audio and video",
            &[
                "aac", "aif", "aiff", "avi", "flac", "m4a", "m4v", "mkv", "mov", "mp3", "mp4",
                "mpeg", "mpg", "oga", "ogg", "opus", "wav", "webm", "wma", "wmv",
            ],
        )
        .blocking_pick_file();
    let Some(selected) = selected else {
        return Ok(None);
    };
    let path = selected
        .into_path()
        .map_err(|error| ApiError::new("invalid_file_selection", error.to_string()))?;
    let result = state
        .core
        .import_media(&path, None)
        .map_err(ApiError::from)?;
    Ok(Some(result.meeting))
}

#[tauri::command]
pub fn list_meetings(state: State<'_, AppState>) -> CommandResult<Vec<Meeting>> {
    state
        .core
        .list_meetings(MeetingListRequest::default())
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn get_meeting(meeting_id: String, state: State<'_, AppState>) -> CommandResult<MeetingDetail> {
    state.core.get_meeting(&meeting_id).map_err(ApiError::from)
}

#[tauri::command]
pub fn rename_meeting(
    meeting_id: String,
    title: String,
    state: State<'_, AppState>,
) -> CommandResult<Meeting> {
    state
        .core
        .rename_meeting(&meeting_id, title)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn delete_meeting(meeting_id: String, state: State<'_, AppState>) -> CommandResult<()> {
    state
        .core
        .delete_meeting(&meeting_id)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn search_transcript(
    request: TranscriptSearchRequest,
    state: State<'_, AppState>,
) -> CommandResult<Vec<TranscriptSearchHit>> {
    state
        .core
        .search_transcript(request)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn update_transcript_turn(
    turn_id: String,
    edited_text: String,
    expected_revision: Option<u32>,
    state: State<'_, AppState>,
) -> CommandResult<TranscriptTurn> {
    state
        .core
        .update_transcript_turn(&turn_id, edited_text, expected_revision)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn set_transcript_turn_review(
    turn_id: String,
    needs_review: bool,
    state: State<'_, AppState>,
) -> CommandResult<TranscriptTurn> {
    state
        .core
        .set_transcript_turn_review(&turn_id, needs_review)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn set_transcript_turn_bookmark(
    turn_id: String,
    is_marked: bool,
    state: State<'_, AppState>,
) -> CommandResult<TranscriptTurn> {
    state
        .core
        .set_transcript_turn_bookmark(&turn_id, is_marked)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn rename_speaker(
    meeting_id: String,
    speaker_id: String,
    display_name: String,
    state: State<'_, AppState>,
) -> CommandResult<()> {
    ensure_speaker_in_meeting(&state, &meeting_id, &speaker_id)?;
    state
        .core
        .rename_speaker(&speaker_id, display_name)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn merge_speakers(
    meeting_id: String,
    source_speaker_id: String,
    target_speaker_id: String,
    state: State<'_, AppState>,
) -> CommandResult<()> {
    state
        .core
        .merge_speakers(&meeting_id, &source_speaker_id, &target_speaker_id)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn review_speaker(
    meeting_id: String,
    speaker_id: String,
    accepted: bool,
    state: State<'_, AppState>,
) -> CommandResult<()> {
    ensure_speaker_in_meeting(&state, &meeting_id, &speaker_id)?;
    state
        .core
        .review_speaker_match(&speaker_id, accepted)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn set_speaker_review(
    request: SetSpeakerReviewRequest,
    state: State<'_, AppState>,
) -> CommandResult<()> {
    state
        .core
        .set_speaker_review(&request.speaker_id, request.needs_review)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn list_voice_profiles(state: State<'_, AppState>) -> CommandResult<Vec<VoiceProfile>> {
    state.core.list_voice_profiles().map_err(ApiError::from)
}

#[tauri::command]
pub fn create_voice_profile(
    name: String,
    state: State<'_, AppState>,
) -> CommandResult<VoiceProfile> {
    state
        .core
        .create_voice_profile(name)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn delete_voice_profile(profile_id: String, state: State<'_, AppState>) -> CommandResult<()> {
    state
        .core
        .delete_voice_profile(&profile_id)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn confirm_voice_profile_sample(
    request: ConfirmVoiceSampleRequest,
    state: State<'_, AppState>,
) -> CommandResult<VoiceProfile> {
    state
        .core
        .confirm_voice_profile_sample(request)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn list_processing_jobs(
    meeting_id: Option<String>,
    state: State<'_, AppState>,
) -> CommandResult<Vec<ProcessingJob>> {
    state
        .core
        .list_jobs(meeting_id.as_deref())
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn cancel_processing_job(
    job_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CommandResult<ProcessingJob> {
    let job = state.core.cancel_job(&job_id).map_err(ApiError::from)?;
    let _ = app.emit(
        "meeting://changed",
        serde_json::json!({
            "meetingId":job.meeting_id,
            "reason":"processing_status_changed"
        }),
    );
    Ok(job)
}

#[tauri::command]
pub fn retry_processing_job(
    job_id: String,
    state: State<'_, AppState>,
) -> CommandResult<ProcessingJob> {
    state.core.retry_job(&job_id).map_err(ApiError::from)
}

#[tauri::command]
pub fn export_transcript(
    meeting_id: String,
    format: ExportFormat,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CommandResult<ExportResult> {
    let (result, bytes) = state
        .core
        .export_transcript(&meeting_id, format.clone())
        .map_err(ApiError::from)?;
    let extension = match format {
        ExportFormat::Txt => "txt",
        ExportFormat::Markdown => "md",
        ExportFormat::Srt => "srt",
        ExportFormat::WebVtt => "vtt",
        ExportFormat::Json => "json",
    };
    if let Some(selected) = app
        .dialog()
        .file()
        .set_file_name(&result.file_name)
        .add_filter("Transcript", &[extension])
        .blocking_save_file()
    {
        let path = selected
            .into_path()
            .map_err(|error| ApiError::new("invalid_file_selection", error.to_string()))?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .map_err(CoreError::Io)
            .map_err(ApiError::from)?;
        file.write_all(&bytes)
            .map_err(CoreError::Io)
            .map_err(ApiError::from)?;
        file.sync_all()
            .map_err(CoreError::Io)
            .map_err(ApiError::from)?;
    }
    Ok(result)
}

#[tauri::command]
pub fn backup_library(
    request: BackupRequest,
    state: State<'_, AppState>,
) -> CommandResult<BackupResult> {
    state
        .core
        .backup_library(request.include_media)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn create_backup(state: State<'_, AppState>) -> CommandResult<BackupResult> {
    state.core.backup_library(true).map_err(ApiError::from)
}

#[tauri::command]
pub fn get_asset_descriptor(
    asset_id: String,
    state: State<'_, AppState>,
) -> CommandResult<AssetDescriptor> {
    let mut descriptor = state
        .core
        .asset_descriptor(&asset_id)
        .map_err(ApiError::from)?;
    descriptor.url = Some(if cfg!(windows) {
        format!("http://localtranscript.localhost/asset/{asset_id}")
    } else {
        format!("localtranscript://asset/{asset_id}")
    });
    Ok(descriptor)
}

#[tauri::command]
pub fn read_asset_chunk(
    request: AssetChunkRequest,
    state: State<'_, AppState>,
) -> CommandResult<AssetChunk> {
    state.core.read_asset_chunk(request).map_err(ApiError::from)
}

#[tauri::command]
pub fn list_audio_devices(state: State<'_, AppState>) -> CommandResult<Vec<AudioDevice>> {
    let devices = state.recording.list_devices().map_err(ApiError::from)?;
    Ok(devices
        .microphones
        .into_iter()
        .chain(devices.outputs)
        .collect())
}

#[tauri::command]
pub fn get_recording_status(state: State<'_, AppState>) -> RecordingStatus {
    state.recording.status()
}

#[tauri::command]
pub fn start_recording(
    title: String,
    config: RecordingConfig,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CommandResult<RecordingSession> {
    state
        .recording
        .start(title, config, app)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn pause_recording(
    session_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CommandResult<RecordingSession> {
    state
        .recording
        .pause(&session_id, app)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn resume_recording(
    session_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CommandResult<RecordingSession> {
    state
        .recording
        .resume(&session_id, app)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn add_recording_marker(
    request: AddMarkerRequest,
    state: State<'_, AppState>,
) -> CommandResult<RecordingMarker> {
    let status = state.recording.status();
    let meeting_id = status
        .meeting_id
        .ok_or_else(|| ApiError::new("conflict", "no recording is active"))?;
    state
        .recording
        .add_marker(
            &meeting_id,
            status.elapsed_ms,
            request.label.unwrap_or_default(),
        )
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn add_marker(
    meeting_id: String,
    at_ms: i64,
    label: String,
    state: State<'_, AppState>,
) -> CommandResult<RecordingMarker> {
    state
        .recording
        .add_marker(&meeting_id, at_ms, label)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn stop_recording(
    session_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CommandResult<Meeting> {
    state
        .recording
        .stop(&session_id, app)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn get_worker_status(state: State<'_, AppState>) -> WorkerStatus {
    state.worker.status()
}

#[tauri::command]
pub fn restart_worker(state: State<'_, AppState>) -> CommandResult<WorkerStatus> {
    state.worker.restart().map_err(ApiError::from)
}

#[tauri::command]
pub fn get_model_status(state: State<'_, AppState>) -> ModelPackStatus {
    state.core.model_status()
}

#[tauri::command]
pub fn install_model_pack(
    mut hugging_face_token: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CommandResult<ModelPackStatus> {
    let result = state
        .worker
        .install_model_pack(&hugging_face_token, |progress| {
            if let Err(error) = app.emit("model://setup-progress", progress) {
                log::warn!("could not emit model setup progress: {error}");
            }
        });
    hugging_face_token.zeroize();
    result.map_err(ApiError::from)?;
    let status = state.core.model_status();
    if status.runtime != "ready"
        || status.live_model != "ready"
        || status.final_model != "ready"
        || status.diarization_model != "ready"
    {
        return Err(ApiError::new(
            "model_setup_incomplete",
            "one or more pinned model files are missing or failed verification",
        ));
    }
    state.core.mark_setup_complete().map_err(ApiError::from)?;
    Ok(status)
}

fn ensure_speaker_in_meeting(
    state: &State<'_, AppState>,
    meeting_id: &str,
    speaker_id: &str,
) -> CommandResult<()> {
    let detail = state.core.get_meeting(meeting_id).map_err(ApiError::from)?;
    if detail
        .speakers
        .iter()
        .any(|speaker| speaker.id == speaker_id)
    {
        Ok(())
    } else {
        Err(ApiError::new(
            "not_found",
            "speaker does not belong to the meeting",
        ))
    }
}
