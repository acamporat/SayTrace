use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use parking_lot::Mutex;
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use uuid::Uuid;
use zeroize::Zeroize;

use crate::{
    crypto,
    db::now_ms,
    error::{CoreError, CoreResult},
    layout::{managed_child_directory, remove_managed_child_tree},
    media,
    service::CoreService,
    worker::{worker_compatible_path, WorkerRequest, WorkerSupervisor, PIPELINE_VERSION},
};

const IDLE_POLL: Duration = Duration::from_millis(750);
const ACTIVE_POLL: Duration = Duration::from_millis(150);
const RETRY_DELAY_MS: i64 = 5_000;
const MAX_CANONICAL_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;

/// Owns the durable host-side queue. The Python process also serializes GPU
/// execution, but Rust remains authoritative for job state and SQLite writes.
pub struct JobCoordinator {
    stop: Arc<AtomicBool>,
    handle: Mutex<Option<thread::JoinHandle<()>>>,
}

impl JobCoordinator {
    pub fn start(
        core: Arc<CoreService>,
        worker: Arc<WorkerSupervisor>,
        app: AppHandle,
    ) -> CoreResult<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let handle = thread::Builder::new()
            .name("durable-processing-queue".into())
            .spawn(move || coordinator_loop(core, worker, app, thread_stop))
            .map_err(CoreError::Io)?;
        Ok(Self {
            stop,
            handle: Mutex::new(Some(handle)),
        })
    }
}

impl Drop for JobCoordinator {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // A pipeline can be inside a native ML call for a while. Detach here so
        // application shutdown is never held hostage; startup recovery changes
        // the still-running DB row to retry_wait.
        let _ = self.handle.lock().take();
    }
}

#[derive(Debug)]
struct ClaimedJob {
    id: String,
    meeting_id: String,
    attempts: u32,
    max_attempts: u32,
    output: Value,
}

#[derive(Debug)]
struct SourceAsset {
    id: String,
    kind: String,
    path: PathBuf,
}

struct PendingSpeakerCandidate {
    cluster_label: String,
    clean_duration_ms: i64,
    encrypted_embedding: Vec<u8>,
}

struct PreparedImportPlayback {
    id: String,
    relative_path: String,
    size_bytes: u64,
    sha256: String,
    duration_ms: i64,
    sample_rate_hz: u32,
    channels: u16,
}

fn coordinator_loop(
    core: Arc<CoreService>,
    worker: Arc<WorkerSupervisor>,
    app: AppHandle,
    stop: Arc<AtomicBool>,
) {
    let worker_id = format!("desktop-{}", Uuid::now_v7());
    while !stop.load(Ordering::Relaxed) {
        match claim_next_job(&core, &worker_id) {
            Ok(Some(job)) => {
                emit_job(&core, &app, &job.id);
                if let Err(error) = run_claimed_job(&core, &worker, &app, &stop, &job) {
                    let _ = fail_job(&core, &job, "HOST_PIPELINE_ERROR", &error.to_string(), true);
                    emit_job(&core, &app, &job.id);
                    let _ = app.emit(
                        "meeting://changed",
                        json!({"meetingId":job.meeting_id,"reason":"processing_status_changed"}),
                    );
                }
            }
            Ok(None) => thread::sleep(IDLE_POLL),
            Err(error) => {
                log::error!("processing queue poll failed: {error}");
                thread::sleep(IDLE_POLL);
            }
        }
    }
}

fn claim_next_job(core: &CoreService, worker_id: &str) -> CoreResult<Option<ClaimedJob>> {
    let now = now_ms();
    let retry_before = now.saturating_sub(RETRY_DELAY_MS);
    let mut connection = core.database().connect()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let row = transaction
        .query_row(
            "SELECT id,meeting_id,attempts,max_attempts,output_json
             FROM processing_jobs
             WHERE status='queued'
                OR (status='retry_wait' AND updated_at_ms<=?1)
             ORDER BY priority DESC,created_at_ms
             LIMIT 1",
            [retry_before],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((id, meeting_id, attempts, max_attempts, output_json)) = row else {
        transaction.commit()?;
        return Ok(None);
    };
    let changed = transaction.execute(
        "UPDATE processing_jobs
         SET status='running',attempts=attempts+1,locked_at_ms=?1,worker_id=?2,
             error_code=NULL,error_message=NULL,updated_at_ms=?1
         WHERE id=?3 AND status IN ('queued','retry_wait')",
        params![now, worker_id, id],
    )?;
    if changed != 1 {
        transaction.commit()?;
        return Ok(None);
    }
    transaction.commit()?;
    let output = output_json
        .as_deref()
        .and_then(|value| serde_json::from_str(value).ok())
        .unwrap_or_else(|| json!({}));
    Ok(Some(ClaimedJob {
        id,
        meeting_id,
        attempts: attempts.saturating_add(1) as u32,
        max_attempts: max_attempts as u32,
        output,
    }))
}

fn run_claimed_job(
    core: &CoreService,
    worker: &WorkerSupervisor,
    app: &AppHandle,
    stop: &AtomicBool,
    job: &ClaimedJob,
) -> CoreResult<()> {
    let payload = build_pipeline_payload(core, job)?;
    let mut request = WorkerRequest::new("pipeline.run", payload);
    request.job_id = Some(job.id.clone());
    request.pipeline_version = Some(PIPELINE_VERSION.into());
    worker.request(request)?;

    let mut cancel_sent = false;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        if !cancel_sent && job_status(core, &job.id)?.as_deref() == Some("cancel_requested") {
            let mut request = WorkerRequest::new("pipeline.cancel", json!({"job_id":job.id}));
            request.job_id = Some(job.id.clone());
            let _ = worker.request(request);
            cancel_sent = true;
        }

        for mut event in worker.drain_job_events(&job.id) {
            match event.event.as_deref() {
                Some("job_started") => {
                    update_job_progress(
                        core,
                        job,
                        &json!({
                            "stage":"normalize",
                            "completed_batches":0,
                            "total_batches":1,
                            "resume":job.output.get("resume").cloned().unwrap_or_else(|| json!({}))
                        }),
                    )?;
                    emit_job(core, app, &job.id);
                }
                Some("job_progress") => {
                    update_job_progress(core, job, &event.payload)?;
                    emit_job(core, app, &job.id);
                }
                Some("pipeline_batch") => {
                    // Batches are intentionally not written into canonical
                    // transcript tables. The final artifact is validated and
                    // committed in one SQLite transaction on job_complete.
                }
                Some("job_complete") => {
                    let result = event.payload.get_mut("result").ok_or_else(|| {
                        CoreError::Worker("worker completion did not contain a result".into())
                    })?;
                    commit_canonical_result(core, job, result)?;
                    emit_job(core, app, &job.id);
                    let _ = app.emit(
                        "meeting://changed",
                        json!({"meetingId":job.meeting_id,"reason":"transcript_finalized"}),
                    );
                    return Ok(());
                }
                Some("job_error") => {
                    let error = event.payload.get("error").cloned().unwrap_or_default();
                    let code = error
                        .get("code")
                        .and_then(Value::as_str)
                        .unwrap_or("WORKER_ERROR");
                    let message = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("local pipeline failed");
                    let retryable = error
                        .get("retryable")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if code == "CANCELLED" || cancel_sent {
                        mark_cancelled(core, job, message)?;
                    } else {
                        fail_job(core, job, code, message, retryable)?;
                    }
                    emit_job(core, app, &job.id);
                    let _ = app.emit(
                        "meeting://changed",
                        json!({
                            "meetingId":job.meeting_id,
                            "reason":"processing_status_changed"
                        }),
                    );
                    return Ok(());
                }
                _ => {}
            }
        }

        let status = worker.status();
        if status.process_id.is_none() {
            return Err(CoreError::Worker(
                status
                    .error
                    .unwrap_or_else(|| "worker exited during final processing".into()),
            ));
        }
        thread::sleep(ACTIVE_POLL);
    }
}

fn build_pipeline_payload(core: &CoreService, job: &ClaimedJob) -> CoreResult<Value> {
    validate_job_id(&job.id)?;
    let workspace =
        managed_child_directory(core.layout().library(), core.layout().work(), &job.id)?;

    let connection = core.database().connect()?;
    let mut statement = connection.prepare(
        "SELECT id,kind,relative_path
         FROM media_assets
         WHERE meeting_id=?1
         ORDER BY created_at_ms,id",
    )?;
    let mut assets = statement
        .query_map([&job.meeting_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .map(|row| {
            let (id, kind, relative) = row?;
            Ok(SourceAsset {
                id,
                kind,
                path: core.layout().resolve_relative(&relative)?,
            })
        })
        .collect::<CoreResult<Vec<_>>>()?;
    if assets.is_empty() {
        return Err(CoreError::NotFound(format!(
            "meeting {} has no source media",
            job.meeting_id
        )));
    }

    let has_authoritative_recording_track = assets
        .iter()
        .any(|asset| matches!(asset.kind.as_str(), "microphone" | "loopback"));
    let microphone_is_personal = if has_authoritative_recording_track {
        recording_microphone_is_personal(&connection, &job.meeting_id)?
    } else {
        false
    };
    if has_authoritative_recording_track {
        if microphone_is_personal {
            assets.retain(|asset| matches!(asset.kind.as_str(), "microphone" | "loopback"));
        } else {
            let has_mixed_playback = assets.iter().any(|asset| asset.kind == "playback");
            assets.retain(|asset| {
                matches!(asset.kind.as_str(), "microphone" | "loopback")
                    || (has_mixed_playback && asset.kind == "playback")
            });
        }
    } else {
        assets.retain(|asset| asset.kind != "playback");
    }
    if assets.is_empty() {
        return Err(CoreError::InvalidInput(
            "meeting has no authoritative transcription source".into(),
        ));
    }
    let diarization_asset_id = if has_authoritative_recording_track && !microphone_is_personal {
        assets
            .iter()
            .find(|asset| asset.kind == "playback")
            .or_else(|| assets.iter().find(|asset| asset.kind == "microphone"))
            .or_else(|| assets.iter().find(|asset| asset.kind == "loopback"))
            .map(|asset| asset.id.clone())
            .unwrap_or_else(|| assets[0].id.clone())
    } else {
        assets
            .iter()
            .find(|asset| asset.kind == "loopback")
            .map(|asset| asset.id.clone())
            .unwrap_or_else(|| assets[0].id.clone())
    };

    let sources = assets
        .iter()
        .filter_map(|asset| {
            let source_type = match asset.kind.as_str() {
                "microphone" => "microphone",
                "loopback" => "loopback",
                "playback" if has_authoritative_recording_track && !microphone_is_personal => {
                    "mixed"
                }
                _ if !has_authoritative_recording_track => "import",
                _ => return None,
            };
            let mut value = json!({
                "asset_id":asset.id,
                "path":worker_compatible_path(&asset.path).to_string_lossy(),
                "source_type":source_type,
                "priority":match source_type {
                    "loopback" => 30,
                    "microphone" => 10,
                    // The mixed file provides the complete speaker timeline,
                    // but the authoritative endpoint tracks should win bleed
                    // deduplication whenever their words overlap it.
                    "mixed" => 0,
                    _ => 20,
                }
            });
            if source_type == "microphone" && microphone_is_personal {
                value["isolated_speaker"] = Value::String("You".into());
            }
            Some(value)
        })
        .collect::<Vec<_>>();
    if sources.is_empty() {
        return Err(CoreError::InvalidInput(
            "meeting has no authoritative transcription source".into(),
        ));
    }
    if !sources
        .iter()
        .any(|source| source.get("asset_id").and_then(Value::as_str) == Some(&diarization_asset_id))
    {
        return Err(CoreError::InvalidInput(
            "diarization media is not an approved transcription source".into(),
        ));
    }

    let profiles = load_confirmed_profiles(core)?;
    let match_policy = (!profiles.is_empty()).then(|| {
        json!({
            "calibration_id":"local-transcript-uncalibrated-conservative-en-v1",
            "accept_similarity":0.93,
            "accept_margin":0.10,
            "review_similarity":0.82
        })
    });
    let resume = prepare_resume_for_worker(
        job.output
            .get("resume")
            .cloned()
            .unwrap_or_else(|| json!({})),
    );
    Ok(json!({
        "workspace_path":worker_compatible_path(&workspace).to_string_lossy(),
        "sources":sources,
        "diarization_asset_id":diarization_asset_id,
        "profiles":profiles,
        "match_policy":match_policy,
        "resume":resume
    }))
}

fn prepare_resume_for_worker(resume: Value) -> Value {
    if contains_windows_verbatim_path(&resume) {
        // Releases before the native-path interop fix persisted verbatim Windows
        // paths both in the checkpoint and inside its stage artifacts. Mixing
        // either spelling with a repaired workspace causes correct containment
        // checks to reject the resume. Start from saved source media instead.
        json!({})
    } else {
        resume
    }
}

fn contains_windows_verbatim_path(value: &Value) -> bool {
    match value {
        Value::String(raw) => raw.starts_with(r"\\?\"),
        Value::Array(values) => values.iter().any(contains_windows_verbatim_path),
        Value::Object(values) => values.values().any(contains_windows_verbatim_path),
        _ => false,
    }
}

fn recording_microphone_is_personal(
    connection: &rusqlite::Connection,
    meeting_id: &str,
) -> CoreResult<bool> {
    let config_json = connection
        .query_row(
            "SELECT config_json
             FROM recording_sessions
             WHERE meeting_id=?1
             ORDER BY started_at_ms DESC
             LIMIT 1",
            [meeting_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(config_json) = config_json else {
        // Recordings made before this setting existed used a personal
        // microphone by definition. Keep that compatible and conservative.
        return Ok(true);
    };
    Ok(
        serde_json::from_str::<crate::models::RecordingConfig>(&config_json)
            .map_err(|error| {
                CoreError::InvalidInput(format!("recording configuration is invalid: {error}"))
            })?
            .microphone_is_personal,
    )
}

fn load_confirmed_profiles(core: &CoreService) -> CoreResult<Vec<Value>> {
    let connection = core.database().connect()?;
    let mut statement = connection.prepare(
        "SELECT p.id,p.display_name,s.clean_duration_ms,s.encrypted_embedding
         FROM voice_profiles p
         JOIN voice_profile_samples s ON s.profile_id=p.id
         WHERE p.id IN (
            SELECT profile_id FROM voice_profile_samples
            GROUP BY profile_id
            HAVING count(*)>=3 AND sum(clean_duration_ms)>=30000
         )
         ORDER BY p.id,s.confirmed_at_ms",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut grouped: BTreeMap<String, (String, Vec<i64>, Vec<Vec<f32>>)> = BTreeMap::new();
    for (profile_id, name, duration, encrypted) in rows {
        let mut clear = crypto::unprotect_embedding(&encrypted)?;
        let decoded = decode_embedding(&clear);
        clear.zeroize();
        let embedding = decoded?;
        let entry = grouped
            .entry(profile_id)
            .or_insert_with(|| (name, Vec::new(), Vec::new()));
        entry.1.push(duration);
        entry.2.push(embedding);
    }
    Ok(grouped
        .into_iter()
        .map(|(profile_id, (name, durations, embeddings))| {
            json!({
                "profile_id":profile_id,
                "name":name,
                "embeddings":embeddings,
                "sample_durations_ms":durations,
                "explicitly_confirmed":true
            })
        })
        .collect())
}

fn decode_embedding(bytes: &[u8]) -> CoreResult<Vec<f32>> {
    let vector = if bytes.first() == Some(&b'[') {
        serde_json::from_slice::<Vec<f32>>(bytes)
            .map_err(|_| CoreError::Security("stored voice embedding JSON is invalid".into()))?
    } else if !bytes.is_empty() && bytes.len() % 4 == 0 {
        bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect()
    } else {
        return Err(CoreError::Security(
            "stored voice embedding has an invalid binary shape".into(),
        ));
    };
    if vector.is_empty() || vector.len() > 2_048 || vector.iter().any(|value| !value.is_finite()) {
        return Err(CoreError::Security(
            "stored voice embedding has invalid values".into(),
        ));
    }
    Ok(vector)
}

fn update_job_progress(core: &CoreService, job: &ClaimedJob, payload: &Value) -> CoreResult<()> {
    let worker_stage = payload
        .get("stage")
        .and_then(Value::as_str)
        .unwrap_or("normalize");
    let stage = database_stage(worker_stage);
    let completed = payload
        .get("completed_batches")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = payload
        .get("total_batches")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1);
    let progress = global_progress(worker_stage, completed as f64 / total as f64);
    let output = json!({
        "resume":payload.get("resume").cloned().unwrap_or_else(|| json!({})),
        "last_progress":payload
    });
    let connection = core.database().connect()?;
    connection.execute(
        "UPDATE processing_jobs
         SET stage=?1,progress=?2,output_json=?3,checkpoint_ms=?4,updated_at_ms=?4
         WHERE id=?5 AND status IN ('running','cancel_requested')",
        params![stage, progress, output.to_string(), now_ms(), job.id],
    )?;
    Ok(())
}

fn commit_canonical_result(
    core: &CoreService,
    job: &ClaimedJob,
    result: &mut Value,
) -> CoreResult<()> {
    let pending_candidates = take_speaker_candidates(result)?;
    let raw_path = result
        .get("canonical_artifact_path")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            CoreError::Worker("pipeline result did not name a canonical artifact".into())
        })?;
    let artifact_path = Path::new(raw_path).canonicalize()?;
    validate_job_id(&job.id)?;
    let workspace =
        managed_child_directory(core.layout().library(), core.layout().work(), &job.id)?;
    if !artifact_path.starts_with(&workspace) {
        return Err(CoreError::Security(
            "worker canonical artifact is outside its approved job workspace".into(),
        ));
    }
    let metadata = fs::metadata(&artifact_path)?;
    if !metadata.is_file() || metadata.len() > MAX_CANONICAL_ARTIFACT_BYTES {
        return Err(CoreError::Security(
            "worker canonical artifact has an invalid size".into(),
        ));
    }
    let raw = fs::read_to_string(&artifact_path)?;
    let artifact: Value = serde_json::from_str(&raw)?;
    if artifact.get("pipeline_version").and_then(Value::as_str) != Some(PIPELINE_VERSION) {
        return Err(CoreError::Worker(
            "canonical artifact pipeline version is incompatible".into(),
        ));
    }
    let turns = artifact
        .get("turns")
        .and_then(Value::as_array)
        .ok_or_else(|| CoreError::Worker("canonical artifact has no turns array".into()))?;
    let empty_matches = serde_json::Map::new();
    let speaker_matches = artifact
        .get("speaker_matches")
        .and_then(Value::as_object)
        .unwrap_or(&empty_matches);
    let now = now_ms();
    let import_playback = prepare_import_playback(core, job, &workspace, result)?;
    let model_output_id = Uuid::now_v7().to_string();
    let artifact_id = Uuid::now_v7().to_string();
    let durable_path = core
        .layout()
        .artifacts()
        .join(&job.meeting_id)
        .join(format!("{model_output_id}.json"));
    let (artifact_size, artifact_sha256) =
        media::atomic_copy_with_hash(&artifact_path, &durable_path)?;
    let artifact_relative = core.layout().relative_to_root(&durable_path)?;
    let word_count = turns
        .iter()
        .filter_map(|turn| turn.get("words").and_then(Value::as_array))
        .map(Vec::len)
        .sum::<usize>();
    let raw_summary = json!({
        "schema_version":artifact.get("schema_version"),
        "pipeline_version":PIPELINE_VERSION,
        "immutable_model_output":true,
        "draft_independent":artifact.get("draft_independent"),
        "speaker_confidence_kind":"categorical",
        "speaker_matches":speaker_matches,
        "warnings":artifact.get("warnings").cloned().unwrap_or_else(|| json!([])),
        "turn_count":turns.len(),
        "word_count":word_count,
        "canonical_artifact_id":artifact_id
    });
    let mut connection = core.database().connect()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute(
        "UPDATE model_outputs SET is_canonical=0
         WHERE meeting_id=?1 AND is_canonical=1",
        [&job.meeting_id],
    )?;
    if let Some(playback) = &import_playback {
        transaction.execute(
            "INSERT OR IGNORE INTO media_assets(
                id,meeting_id,kind,display_name,relative_path,content_type,size_bytes,sha256,
                duration_ms,codec,sample_rate_hz,channels,created_at_ms
             ) VALUES (?1,?2,'playback','playback.wav',?3,'audio/wav',?4,?5,?6,
                       'pcm_s16le',?7,?8,?9)",
            params![
                playback.id,
                job.meeting_id,
                playback.relative_path,
                playback.size_bytes as i64,
                playback.sha256,
                playback.duration_ms,
                playback.sample_rate_hz,
                playback.channels,
                now
            ],
        )?;
    }
    transaction.execute(
        "INSERT INTO model_outputs(
            id,meeting_id,pipeline_version,model_revisions_json,raw_result_json,
            is_canonical,created_at_ms
         ) VALUES (?1,?2,?3,?4,?5,1,?6)",
        params![
            model_output_id,
            job.meeting_id,
            PIPELINE_VERSION,
            json!({"pipeline":PIPELINE_VERSION}).to_string(),
            raw_summary.to_string(),
            now
        ],
    )?;
    transaction.execute(
        "INSERT INTO artifacts(
            id,meeting_id,kind,format,relative_path,size_bytes,sha256,created_at_ms
         ) VALUES (?1,?2,'model_output','json',?3,?4,?5,?6)",
        params![
            artifact_id,
            job.meeting_id,
            artifact_relative,
            artifact_size as i64,
            artifact_sha256,
            now
        ],
    )?;

    for turn in turns {
        import_turn(
            &transaction,
            &job.meeting_id,
            &model_output_id,
            speaker_matches,
            turn,
            now,
        )?;
    }
    for candidate in pending_candidates {
        persist_speaker_candidate(&transaction, &job.meeting_id, candidate, now)?;
    }
    // Exact deterministic IDs retain edited_text and revisions on reprocess.
    // Unmatched edited turns are kept and flagged instead of silently losing a
    // correction; unmatched untouched model turns are safely regenerated away.
    transaction.execute(
        "DELETE FROM transcript_turns
         WHERE meeting_id=?1 AND model_output_id<>?2 AND edited_text IS NULL",
        params![job.meeting_id, model_output_id],
    )?;
    transaction.execute(
        "UPDATE transcript_turns SET needs_review=1,updated_at_ms=?1
         WHERE meeting_id=?2 AND model_output_id<>?3 AND edited_text IS NOT NULL",
        params![now, job.meeting_id, model_output_id],
    )?;
    transaction.execute(
        "UPDATE processing_jobs
         SET stage='finalize',status='completed',progress=1,output_json=?1,
             checkpoint_ms=?2,locked_at_ms=NULL,worker_id=NULL,
             error_code=NULL,error_message=NULL,updated_at_ms=?2
         WHERE id=?3",
        params![json!({"result":result}).to_string(), now, job.id],
    )?;
    transaction.execute(
        "UPDATE meetings SET status='ready',model_revision=?1 WHERE id=?2",
        params![PIPELINE_VERSION, job.meeting_id],
    )?;
    transaction.commit()?;
    if let Err(error) =
        remove_managed_child_tree(core.layout().library(), core.layout().work(), &job.id)
    {
        // The durable media/artifact rows and canonical transcript are already
        // committed. Scratch cleanup is best effort and must never downgrade a
        // successfully completed job.
        log::warn!(
            "completed job {} but could not remove its regenerable workspace: {error}",
            job.id
        );
    }
    Ok(())
}

fn prepare_import_playback(
    core: &CoreService,
    job: &ClaimedJob,
    workspace: &Path,
    result: &Value,
) -> CoreResult<Option<PreparedImportPlayback>> {
    let connection = core.database().connect()?;
    let source_kind = connection
        .query_row(
            "SELECT source_kind FROM meetings WHERE id=?1",
            [&job.meeting_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if source_kind.as_deref() != Some("import") {
        return Ok(None);
    }
    let existing_playback: i64 = connection.query_row(
        "SELECT count(*) FROM media_assets WHERE meeting_id=?1 AND kind='playback'",
        [&job.meeting_id],
        |row| row.get(0),
    )?;
    if existing_playback > 0 {
        return Ok(None);
    }
    let source_asset_id = connection
        .query_row(
            "SELECT id FROM media_assets
             WHERE meeting_id=?1 AND kind<>'playback'
             ORDER BY created_at_ms,id LIMIT 1",
            [&job.meeting_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| {
            CoreError::NotFound(format!(
                "imported meeting {} has no source media",
                job.meeting_id
            ))
        })?;
    drop(connection);
    let normalized = result
        .get("playback_artifact_path")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            workspace
                .join("normalized")
                .join(format!("{source_asset_id}.wav"))
        })
        .canonicalize()?;
    if !normalized.starts_with(workspace) || !normalized.is_file() {
        return Err(CoreError::Security(
            "worker normalized playback is outside its approved job workspace".into(),
        ));
    }
    let destination = core
        .layout()
        .media()
        .join(&job.meeting_id)
        .join(format!("playback-{}.wav", job.id));
    let (size_bytes, sha256) = if destination.is_file() {
        (
            fs::metadata(&destination)?.len(),
            media::sha256_file(&destination)?,
        )
    } else {
        let partial = PathBuf::from(format!("{}.partial", destination.to_string_lossy()));
        let _ = fs::remove_file(partial);
        media::atomic_copy_with_hash(&normalized, &destination)?
    };
    let reader = hound::WavReader::open(&destination).map_err(|error| {
        CoreError::Media(format!("normalized playback WAV is invalid: {error}"))
    })?;
    let spec = reader.spec();
    if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample != 16 {
        return Err(CoreError::Media(
            "normalized playback is not signed 16-bit PCM".into(),
        ));
    }
    let duration_ms = reader.duration() as i64 * 1000 / i64::from(spec.sample_rate.max(1));
    Ok(Some(PreparedImportPlayback {
        id: namespaced_id(&job.meeting_id, "asset:playback"),
        relative_path: core.layout().relative_to_root(&destination)?,
        size_bytes,
        sha256,
        duration_ms,
        sample_rate_hz: spec.sample_rate,
        channels: spec.channels,
    }))
}

fn take_speaker_candidates(result: &mut Value) -> CoreResult<Vec<PendingSpeakerCandidate>> {
    let mut raw_candidates = result
        .as_object_mut()
        .and_then(|object| object.remove("speaker_candidates"))
        .unwrap_or_else(|| json!([]));
    let result = (|| -> CoreResult<Vec<PendingSpeakerCandidate>> {
        let candidates = raw_candidates.as_array_mut().ok_or_else(|| {
            CoreError::Worker("worker speaker_candidates must be an array".into())
        })?;
        if candidates.len() > 256 {
            return Err(CoreError::Security(
                "worker returned too many speaker candidates".into(),
            ));
        }
        let mut protected = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let object = candidate.as_object_mut().ok_or_else(|| {
                CoreError::Worker("worker speaker candidate must be an object".into())
            })?;
            let cluster_label = object
                .get("cluster_label")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty() && value.len() <= 256)
                .ok_or_else(|| {
                    CoreError::Worker(
                        "worker speaker candidate has an invalid cluster label".into(),
                    )
                })?
                .to_string();
            let clean_duration_ms = object
                .get("clean_duration_ms")
                .and_then(Value::as_i64)
                .filter(|value| (1..=86_400_000).contains(value))
                .ok_or_else(|| {
                    CoreError::Worker("worker speaker candidate has an invalid duration".into())
                })?;
            let mut encoded = match object.remove("embedding_base64") {
                Some(Value::String(value)) if value.len() <= 16_384 => value,
                _ => {
                    return Err(CoreError::Worker(
                        "worker speaker candidate has no bounded embedding".into(),
                    ))
                }
            };
            let decoded = BASE64_STANDARD.decode(encoded.as_bytes()).map_err(|_| {
                CoreError::Worker("worker speaker candidate embedding is not valid base64".into())
            });
            encoded.zeroize();
            let mut clear = decoded?;
            if clear.is_empty() || clear.len() > 8_192 || clear.len() % 4 != 0 {
                clear.zeroize();
                return Err(CoreError::Security(
                    "worker speaker candidate embedding has an invalid dimension".into(),
                ));
            }
            let mut magnitude_squared = 0.0_f64;
            for chunk in clear.chunks_exact(4) {
                let value = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                if !value.is_finite() {
                    clear.zeroize();
                    return Err(CoreError::Security(
                        "worker speaker candidate embedding contains invalid values".into(),
                    ));
                }
                magnitude_squared += f64::from(value) * f64::from(value);
            }
            if !magnitude_squared.is_finite() || magnitude_squared <= 1e-12 {
                clear.zeroize();
                return Err(CoreError::Security(
                    "worker speaker candidate embedding has zero magnitude".into(),
                ));
            }
            let encrypted_result = crypto::protect_embedding(&clear);
            clear.zeroize();
            protected.push(PendingSpeakerCandidate {
                cluster_label,
                clean_duration_ms,
                encrypted_embedding: encrypted_result?,
            });
        }
        Ok(protected)
    })();
    scrub_candidate_embeddings(&mut raw_candidates);
    result
}

fn scrub_candidate_embeddings(value: &mut Value) {
    let Some(candidates) = value.as_array_mut() else {
        return;
    };
    for candidate in candidates {
        if let Some(object) = candidate.as_object_mut() {
            if let Some(Value::String(mut encoded)) = object.remove("embedding_base64") {
                encoded.zeroize();
            }
        }
    }
}

fn persist_speaker_candidate(
    transaction: &rusqlite::Transaction<'_>,
    meeting_id: &str,
    candidate: PendingSpeakerCandidate,
    now: i64,
) -> CoreResult<()> {
    let deterministic_speaker_id =
        namespaced_id(meeting_id, &format!("speaker:{}", candidate.cluster_label));
    let speaker_id = transaction
        .query_row(
            "SELECT target_speaker_id FROM speaker_merge_rules
             WHERE meeting_id=?1 AND source_cluster_label=?2",
            params![meeting_id, candidate.cluster_label],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .unwrap_or(deterministic_speaker_id);
    let belongs: i64 = transaction.query_row(
        "SELECT count(*) FROM meeting_speakers WHERE meeting_id=?1 AND id=?2",
        params![meeting_id, speaker_id],
        |row| row.get(0),
    )?;
    if belongs != 1 {
        // Source-aware merging can intentionally replace a raw diarization
        // cluster (for example SPEAKER_00) with the isolated microphone
        // speaker (`You`). An embedding for the discarded raw cluster is not a
        // legitimate profile-learning candidate, so ignore it without
        // rolling back the canonical transcript transaction.
        log::debug!(
            "ignoring unmapped voice candidate {} for meeting {}",
            candidate.cluster_label,
            meeting_id
        );
        return Ok(());
    }
    transaction.execute(
        "INSERT INTO voice_profile_candidates(
            id,meeting_id,speaker_id,cluster_label,clean_duration_ms,
            encrypted_embedding,pipeline_version,created_at_ms
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(meeting_id,speaker_id) DO UPDATE SET
            id=excluded.id,
            cluster_label=excluded.cluster_label,
            clean_duration_ms=excluded.clean_duration_ms,
            encrypted_embedding=excluded.encrypted_embedding,
            pipeline_version=excluded.pipeline_version,
            created_at_ms=excluded.created_at_ms
         WHERE excluded.clean_duration_ms>=voice_profile_candidates.clean_duration_ms",
        params![
            Uuid::now_v7().to_string(),
            meeting_id,
            speaker_id,
            candidate.cluster_label,
            candidate.clean_duration_ms,
            candidate.encrypted_embedding,
            PIPELINE_VERSION,
            now
        ],
    )?;
    Ok(())
}

fn import_turn(
    transaction: &rusqlite::Transaction<'_>,
    meeting_id: &str,
    model_output_id: &str,
    speaker_matches: &serde_json::Map<String, Value>,
    turn: &Value,
    now: i64,
) -> CoreResult<()> {
    let worker_turn_id = required_str(turn, "turn_id")?;
    let cluster = required_str(turn, "speaker_cluster_id")?;
    let deterministic_speaker_id = namespaced_id(meeting_id, &format!("speaker:{cluster}"));
    let match_result = speaker_matches.get(cluster);
    let proposed_speaker_name = match_result
        .and_then(|value| value.get("name"))
        .and_then(Value::as_str)
        .or_else(|| turn.get("speaker_name").and_then(Value::as_str))
        .unwrap_or("Unknown");
    let state = match match_result
        .and_then(|value| value.get("state"))
        .and_then(Value::as_str)
        .or_else(|| turn.get("speaker_state").and_then(Value::as_str))
        .unwrap_or("Unknown")
        .to_ascii_lowercase()
        .as_str()
    {
        "matched" => "matched",
        "review" => "review",
        _ => "unknown",
    };
    // A Review candidate is only a suggestion. Do not display a remembered
    // person's name until the user explicitly accepts it.
    let speaker_name = if state == "review" {
        "Unknown speaker"
    } else {
        proposed_speaker_name
    };
    let proposed_profile_id = match_result
        .and_then(|value| value.get("profile_id"))
        .and_then(Value::as_str);
    let profile_id = if matches!(state, "matched" | "review") {
        proposed_profile_id.and_then(|profile_id| {
            transaction
                .query_row(
                    "SELECT id FROM voice_profiles WHERE id=?1",
                    [profile_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .ok()
                .flatten()
        })
    } else {
        None
    };
    let merged_target = transaction
        .query_row(
            "SELECT target_speaker_id FROM speaker_merge_rules
             WHERE meeting_id=?1 AND source_cluster_label=?2",
            params![meeting_id, cluster],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let speaker_id = merged_target
        .clone()
        .unwrap_or_else(|| deterministic_speaker_id.clone());
    let needs_review = turn
        .get("needs_review")
        .and_then(Value::as_bool)
        .unwrap_or(state != "matched");
    if merged_target.is_none() {
        transaction.execute(
            "INSERT INTO meeting_speakers(
                id,meeting_id,cluster_label,display_name,profile_id,match_state,needs_review
             ) VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(meeting_id,cluster_label) DO UPDATE SET
                display_name=CASE
                    WHEN meeting_speakers.display_name='Unknown'
                     AND meeting_speakers.match_state='unknown'
                    THEN excluded.display_name ELSE meeting_speakers.display_name END,
                profile_id=CASE
                    WHEN meeting_speakers.display_name='Unknown'
                     AND meeting_speakers.match_state='unknown'
                    THEN excluded.profile_id ELSE meeting_speakers.profile_id END,
                match_state=CASE
                    WHEN meeting_speakers.display_name='Unknown'
                     AND meeting_speakers.match_state='unknown'
                    THEN excluded.match_state ELSE meeting_speakers.match_state END,
                needs_review=CASE
                    WHEN meeting_speakers.display_name='Unknown'
                     AND meeting_speakers.match_state='unknown'
                    THEN excluded.needs_review ELSE meeting_speakers.needs_review END",
            params![
                deterministic_speaker_id,
                meeting_id,
                cluster,
                speaker_name,
                profile_id,
                state,
                i64::from(needs_review)
            ],
        )?;
        if state == "matched" {
            if let Some(profile_id) = profile_id {
                transaction.execute(
                    "UPDATE voice_profiles SET last_used_at_ms=?1 WHERE id=?2",
                    params![now, profile_id],
                )?;
            }
        }
    }
    let start_ms = required_i64(turn, "start_ms")?;
    let end_ms = required_i64(turn, "end_ms")?;
    let model_text = required_str(turn, "model_text")?;
    if start_ms < 0 || end_ms < start_ms {
        return Err(CoreError::Worker(
            "canonical turn contains invalid timing".into(),
        ));
    }
    // Worker turn IDs are intentionally job-scoped. Reattach a prior human
    // correction to a strongly overlapping turn owned by the same durable
    // speaker so routine reprocessing does not duplicate or discard edits.
    let corrected_turn_id = transaction
        .query_row(
            "SELECT id FROM transcript_turns
             WHERE meeting_id=?1
               AND speaker_id=?2
               AND edited_text IS NOT NULL
               AND COALESCE(model_output_id,'')<>?3
               AND start_ms<?5 AND end_ms>?4
               AND (min(end_ms,?5)-max(start_ms,?4))*2
                   >= min(end_ms-start_ms,?5-?4)
             ORDER BY
               (min(end_ms,?5)-max(start_ms,?4))*1.0
                   / max(1,max(end_ms,?5)-min(start_ms,?4)) DESC,
               abs(start_ms-?4)
             LIMIT 1",
            params![meeting_id, speaker_id, model_output_id, start_ms, end_ms],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let turn_id = corrected_turn_id
        .unwrap_or_else(|| namespaced_id(meeting_id, &format!("turn:{worker_turn_id}")));
    transaction.execute(
        "INSERT INTO transcript_turns(
            id,meeting_id,model_output_id,speaker_id,start_ms,end_ms,model_text,
            needs_review,is_draft,created_at_ms,updated_at_ms
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,0,?9,?9)
         ON CONFLICT(id) DO UPDATE SET
            model_output_id=excluded.model_output_id,
            speaker_id=excluded.speaker_id,
            start_ms=excluded.start_ms,
            end_ms=excluded.end_ms,
            model_text=excluded.model_text,
            needs_review=CASE WHEN transcript_turns.revision>0
                THEN transcript_turns.needs_review ELSE excluded.needs_review END,
            is_draft=0,
            updated_at_ms=excluded.updated_at_ms",
        params![
            turn_id,
            meeting_id,
            model_output_id,
            speaker_id,
            start_ms,
            end_ms,
            model_text,
            i64::from(needs_review),
            now
        ],
    )?;
    transaction.execute("DELETE FROM words WHERE turn_id=?1", [&turn_id])?;
    let words = turn
        .get("words")
        .and_then(Value::as_array)
        .ok_or_else(|| CoreError::Worker("canonical turn has no words array".into()))?;
    for (sequence, word) in words.iter().enumerate() {
        let worker_word_id = required_str(word, "word_id")?;
        let word_id = namespaced_id(
            meeting_id,
            &format!("word:{worker_turn_id}:{worker_word_id}"),
        );
        let word_start = required_i64(word, "start_ms")?;
        let word_end = required_i64(word, "end_ms")?;
        if word_start < 0 || word_end < word_start {
            return Err(CoreError::Worker(
                "canonical word contains invalid timing".into(),
            ));
        }
        transaction.execute(
            "INSERT INTO words(
                id,turn_id,speaker_id,sequence,start_ms,end_ms,text,confidence,is_overlap
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                word_id,
                turn_id,
                speaker_id,
                sequence as i64,
                word_start,
                word_end,
                required_str(word, "text")?,
                word.get("confidence").and_then(Value::as_f64),
                i64::from(
                    word.get("overlap")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                )
            ],
        )?;
    }
    Ok(())
}

fn fail_job(
    core: &CoreService,
    job: &ClaimedJob,
    code: &str,
    message: &str,
    retryable: bool,
) -> CoreResult<()> {
    let retry = retryable && job.attempts < job.max_attempts;
    let status = if retry { "retry_wait" } else { "failed" };
    let now = now_ms();
    let mut connection = core.database().connect()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute(
        "UPDATE processing_jobs
         SET status=?1,locked_at_ms=NULL,worker_id=NULL,error_code=?2,error_message=?3,
             updated_at_ms=?4
         WHERE id=?5 AND status IN ('running','cancel_requested')",
        params![status, code, message, now, job.id],
    )?;
    if !retry {
        transaction.execute(
            "UPDATE meetings SET status='failed' WHERE id=?1",
            [&job.meeting_id],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

fn mark_cancelled(core: &CoreService, job: &ClaimedJob, message: &str) -> CoreResult<()> {
    let now = now_ms();
    let connection = core.database().connect()?;
    connection.execute(
        "UPDATE processing_jobs
         SET status='cancelled',locked_at_ms=NULL,worker_id=NULL,
             error_code='CANCELLED',error_message=?1,updated_at_ms=?2
         WHERE id=?3",
        params![message, now, job.id],
    )?;
    connection.execute(
        "UPDATE meetings SET status='failed' WHERE id=?1 AND status='processing'",
        [&job.meeting_id],
    )?;
    Ok(())
}

fn job_status(core: &CoreService, job_id: &str) -> CoreResult<Option<String>> {
    let connection = core.database().connect()?;
    Ok(connection
        .query_row(
            "SELECT status FROM processing_jobs WHERE id=?1",
            [job_id],
            |row| row.get(0),
        )
        .optional()?)
}

fn emit_job(core: &CoreService, app: &AppHandle, job_id: &str) {
    if let Ok(job) = core.get_job(job_id) {
        let _ = app.emit("job://progress", json!({"job":job}));
    }
}

fn database_stage(worker_stage: &str) -> &'static str {
    match worker_stage {
        "transcribe" => "transcribe",
        "align" => "align",
        "diarize" => "diarize",
        "merge" | "identify" => "identify",
        "index" => "index",
        "finalize" => "finalize",
        _ => "normalize",
    }
}

fn global_progress(stage: &str, stage_progress: f64) -> f64 {
    let index = match stage {
        "normalize" => 0,
        "transcribe" => 1,
        "align" => 2,
        "diarize" => 3,
        "merge" => 4,
        "identify" => 5,
        "index" => 6,
        "finalize" => 7,
        _ => 0,
    };
    ((index as f64 + stage_progress.clamp(0.0, 1.0)) / 8.0).clamp(0.0, 1.0)
}

fn required_str<'a>(value: &'a Value, key: &str) -> CoreResult<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| CoreError::Worker(format!("canonical artifact is missing {key}")))
}

fn required_i64(value: &Value, key: &str) -> CoreResult<i64> {
    value
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| CoreError::Worker(format!("canonical artifact is missing {key}")))
}

fn validate_job_id(job_id: &str) -> CoreResult<()> {
    Uuid::parse_str(job_id)
        .map(|_| ())
        .map_err(|_| CoreError::Security("processing job id is not a valid UUID".into()))
}

fn namespaced_id(meeting_id: &str, value: &str) -> String {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("{meeting_id}:{value}").as_bytes(),
    )
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_verbatim_resume_is_discarded_before_native_worker_retry() {
        let resume = json!({
            "pipeline_version":"2026.07.28.1",
            "completed_batches":["normalize:asset-1"],
            "stage_results":{
                "normalize:asset-1":r"\\?\C:\Local Transcript\work\normalize.json"
            }
        });

        let prepared = prepare_resume_for_worker(resume);

        assert_eq!(prepared, json!({}));
    }

    #[test]
    fn ordinary_resume_is_preserved_for_idempotent_retry() {
        let resume = json!({
            "pipeline_version":"2026.07.28.1",
            "completed_batches":["normalize:asset-1"],
            "stage_results":{
                "normalize:asset-1":r"C:\Local Transcript\work\normalize.json"
            }
        });

        assert_eq!(prepare_resume_for_worker(resume.clone()), resume);
    }

    fn microphone_payload(microphone_is_personal: bool) -> Value {
        recording_payload(microphone_is_personal, &["microphone"])
    }

    fn recording_payload(microphone_is_personal: bool, kinds: &[&str]) -> Value {
        let temp = tempfile::tempdir().unwrap();
        let core = CoreService::open(temp.path()).unwrap();
        let meeting_id = Uuid::now_v7().to_string();
        let session_id = Uuid::now_v7().to_string();
        let config = crate::models::RecordingConfig {
            capture_microphone: kinds.contains(&"microphone"),
            capture_system_audio: kinds.contains(&"loopback"),
            microphone_is_personal,
            ..Default::default()
        };
        let now = now_ms();
        let connection = core.database().connect().unwrap();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Mic','recording','processing',?2)",
                params![meeting_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO recording_sessions(
                    id,meeting_id,state,config_json,manifest_relative_path,started_at_ms
                 ) VALUES (?1,?2,'stopped',?3,'manifest.jsonl',?4)",
                params![
                    session_id,
                    meeting_id,
                    serde_json::to_string(&config).unwrap(),
                    now
                ],
            )
            .unwrap();
        for kind in kinds {
            let asset_id = Uuid::now_v7().to_string();
            let media_path = core
                .layout()
                .recordings()
                .join(&meeting_id)
                .join(format!("{kind}.flac"));
            fs::create_dir_all(media_path.parent().unwrap()).unwrap();
            fs::write(&media_path, b"audio").unwrap();
            let relative = core.layout().relative_to_root(&media_path).unwrap();
            connection
                .execute(
                    "INSERT INTO media_assets(
                        id,meeting_id,kind,display_name,relative_path,content_type,size_bytes,
                        sha256,created_at_ms
                     ) VALUES (?1,?2,?3,?4,?5,'audio/flac',5,?6,?7)",
                    params![
                        asset_id,
                        meeting_id,
                        kind,
                        format!("{kind}.flac"),
                        relative,
                        format!("hash-{kind}"),
                        now
                    ],
                )
                .unwrap();
        }
        drop(connection);
        build_pipeline_payload(
            &core,
            &ClaimedJob {
                id: Uuid::now_v7().to_string(),
                meeting_id,
                attempts: 0,
                max_attempts: 3,
                output: json!({}),
            },
        )
        .unwrap()
    }

    #[test]
    fn worker_merge_stage_maps_to_valid_database_stage() {
        assert_eq!(database_stage("merge"), "identify");
        assert!(global_progress("merge", 0.5) > global_progress("diarize", 1.0));
    }

    #[test]
    fn personal_microphone_is_isolated_as_you() {
        let payload = microphone_payload(true);
        assert_eq!(
            payload["sources"][0]["isolated_speaker"].as_str(),
            Some("You")
        );
    }

    #[test]
    fn room_microphone_is_sent_through_diarization() {
        let payload = microphone_payload(false);
        assert!(payload["sources"][0].get("isolated_speaker").is_none());
        assert_eq!(
            payload["diarization_asset_id"],
            payload["sources"][0]["asset_id"]
        );
    }

    #[test]
    fn room_recording_uses_mixed_playback_for_complete_diarization() {
        let payload = recording_payload(false, &["microphone", "loopback", "playback"]);
        let sources = payload["sources"].as_array().unwrap();
        let diarization_id = payload["diarization_asset_id"].as_str().unwrap();
        let mixed = sources
            .iter()
            .find(|source| source["source_type"] == "mixed")
            .unwrap();
        assert_eq!(mixed["asset_id"], diarization_id);
        assert_eq!(mixed["priority"], 0);
        assert_eq!(sources.len(), 3);
        assert!(sources
            .iter()
            .find(|source| source["source_type"] == "microphone")
            .unwrap()
            .get("isolated_speaker")
            .is_none());
    }

    #[test]
    fn personal_recording_excludes_playback_and_diarizes_remote_track() {
        let payload = recording_payload(true, &["microphone", "loopback", "playback"]);
        let sources = payload["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 2);
        assert!(sources
            .iter()
            .all(|source| source["source_type"] != "mixed"));
        assert_eq!(
            sources
                .iter()
                .find(|source| source["asset_id"] == payload["diarization_asset_id"])
                .unwrap()["source_type"],
            "loopback"
        );
        assert_eq!(
            sources
                .iter()
                .find(|source| source["source_type"] == "microphone")
                .unwrap()["isolated_speaker"],
            "You"
        );
    }

    #[test]
    fn room_recording_without_playback_falls_back_to_microphone_diarization() {
        let payload = recording_payload(false, &["microphone", "loopback"]);
        let sources = payload["sources"].as_array().unwrap();
        assert_eq!(
            sources
                .iter()
                .find(|source| source["asset_id"] == payload["diarization_asset_id"])
                .unwrap()["source_type"],
            "microphone"
        );
    }

    #[test]
    fn imported_ids_are_namespaced_by_meeting() {
        assert_ne!(
            namespaced_id("meeting-a", "turn:worker-id"),
            namespaced_id("meeting-b", "turn:worker-id")
        );
        assert_eq!(
            namespaced_id("meeting-a", "turn:worker-id"),
            namespaced_id("meeting-a", "turn:worker-id")
        );
    }

    #[test]
    fn invalid_worker_candidate_is_removed_from_terminal_result() {
        let mut result = json!({
            "speaker_candidates":[{
                "cluster_label":"speaker-0",
                "clean_duration_ms":10_000,
                "embedding_base64":"not base64"
            }]
        });
        assert!(take_speaker_candidates(&mut result).is_err());
        assert!(result.get("speaker_candidates").is_none());
    }

    #[test]
    fn review_match_keeps_proposed_profile_for_explicit_acceptance() {
        let temp = tempfile::tempdir().unwrap();
        let core = CoreService::open(temp.path()).unwrap();
        let meeting_id = Uuid::now_v7().to_string();
        let profile_id = Uuid::now_v7().to_string();
        let model_output_id = Uuid::now_v7().to_string();
        let now = now_ms();
        let connection = core.database().connect().unwrap();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Review','import','processing',?2)",
                params![meeting_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO voice_profiles(id,display_name,created_at_ms,updated_at_ms)
                 VALUES (?1,'Candidate',?2,?2)",
                params![profile_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO model_outputs(
                    id,meeting_id,pipeline_version,model_revisions_json,
                    raw_result_json,created_at_ms
                 ) VALUES (?1,?2,?3,'{}','{}',?4)",
                params![model_output_id, meeting_id, PIPELINE_VERSION, now],
            )
            .unwrap();
        drop(connection);
        let mut matches = serde_json::Map::new();
        matches.insert(
            "SPEAKER_00".into(),
            json!({
                "profile_id":profile_id,
                "name":"Candidate",
                "state":"Review"
            }),
        );
        let mut connection = core.database().connect().unwrap();
        let transaction = connection.transaction().unwrap();
        import_turn(
            &transaction,
            &meeting_id,
            &model_output_id,
            &matches,
            &json!({
                "turn_id":"turn-1",
                "speaker_cluster_id":"SPEAKER_00",
                "start_ms":0,
                "end_ms":500,
                "model_text":"hello",
                "words":[]
            }),
            now,
        )
        .unwrap();
        transaction.commit().unwrap();

        let speaker = core
            .get_meeting(&meeting_id)
            .unwrap()
            .speakers
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(speaker.profile_id.as_deref(), Some(profile_id.as_str()));
        assert_eq!(
            speaker.match_state,
            crate::models::SpeakerMatchState::Review
        );
        assert_eq!(speaker.display_name, "Unknown speaker");
        assert!(speaker.needs_review);

        core.review_speaker_match(&speaker.id, true).unwrap();
        let accepted = core
            .get_meeting(&meeting_id)
            .unwrap()
            .speakers
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(
            accepted.match_state,
            crate::models::SpeakerMatchState::Matched
        );
        assert_eq!(accepted.profile_id.as_deref(), Some(profile_id.as_str()));
        assert_eq!(accepted.display_name, "Candidate");
        assert!(!accepted.needs_review);
    }

    #[test]
    fn completed_import_registers_worker_normalized_wav_as_playback() {
        let temp = tempfile::tempdir().unwrap();
        let core = CoreService::open(temp.path()).unwrap();
        let meeting_id = Uuid::now_v7().to_string();
        let source_asset_id = Uuid::now_v7().to_string();
        let job_id = Uuid::now_v7().to_string();
        let now = now_ms();
        let source = core.layout().media().join("original.mkv");
        fs::write(&source, b"original").unwrap();
        let source_relative = core.layout().relative_to_root(&source).unwrap();
        let connection = core.database().connect().unwrap();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Video','import','processing',?2)",
                params![meeting_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO media_assets(
                    id,meeting_id,kind,display_name,relative_path,content_type,size_bytes,
                    sha256,created_at_ms
                 ) VALUES (?1,?2,'original','original.mkv',?3,'video/x-matroska',8,'hash',?4)",
                params![source_asset_id, meeting_id, source_relative, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO processing_jobs(
                    id,meeting_id,stage,status,progress,input_json,created_at_ms,updated_at_ms
                 ) VALUES (?1,?2,'finalize','running',0.9,'{}',?3,?3)",
                params![job_id, meeting_id, now],
            )
            .unwrap();
        drop(connection);

        let workspace = core.layout().work().join(&job_id);
        let normalized = workspace
            .join("normalized")
            .join(format!("{source_asset_id}.wav"));
        fs::create_dir_all(normalized.parent().unwrap()).unwrap();
        let mut wav = hound::WavWriter::create(
            &normalized,
            hound::WavSpec {
                channels: 1,
                sample_rate: 16_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for _ in 0..16_000 {
            wav.write_sample(0_i16).unwrap();
        }
        wav.finalize().unwrap();
        let canonical = workspace.join("canonical.json");
        fs::write(
            &canonical,
            json!({
                "schema_version":1,
                "pipeline_version":PIPELINE_VERSION,
                "draft_independent":true,
                "speaker_matches":{},
                "turns":[]
            })
            .to_string(),
        )
        .unwrap();
        let job = ClaimedJob {
            id: job_id,
            meeting_id: meeting_id.clone(),
            attempts: 1,
            max_attempts: 3,
            output: json!({}),
        };
        let mut result = json!({
            "canonical_artifact_path":canonical.to_string_lossy(),
            "playback_artifact_path":normalized.to_string_lossy()
        });

        commit_canonical_result(&core, &job, &mut result).unwrap();

        let connection = core.database().connect().unwrap();
        let (kind, content_type, duration_ms, relative): (String, String, i64, String) = connection
            .query_row(
                "SELECT kind,content_type,duration_ms,relative_path
                 FROM media_assets WHERE meeting_id=?1 AND kind='playback'",
                [&meeting_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(kind, "playback");
        assert_eq!(content_type, "audio/wav");
        assert_eq!(duration_ms, 1_000);
        assert!(core.layout().resolve_relative(&relative).unwrap().is_file());
        assert!(
            !workspace.exists(),
            "completed job scratch workspace should be removed"
        );
    }

    #[test]
    fn retryable_failure_preserves_workspace_for_resume() {
        let temp = tempfile::tempdir().unwrap();
        let core = CoreService::open(temp.path()).unwrap();
        let meeting_id = Uuid::now_v7().to_string();
        let job_id = Uuid::now_v7().to_string();
        let now = now_ms();
        let connection = core.database().connect().unwrap();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Retry','import','processing',?2)",
                params![meeting_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO processing_jobs(
                    id,meeting_id,stage,status,progress,attempts,max_attempts,input_json,
                    created_at_ms,updated_at_ms
                 ) VALUES (?1,?2,'transcribe','running',0.4,1,4,'{}',?3,?3)",
                params![job_id, meeting_id, now],
            )
            .unwrap();
        drop(connection);
        let workspace = core.layout().work().join(&job_id);
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("checkpoint.bin"), b"resume").unwrap();
        let job = ClaimedJob {
            id: job_id.clone(),
            meeting_id,
            attempts: 1,
            max_attempts: 4,
            output: json!({}),
        };

        fail_job(&core, &job, "GPU_OOM", "retry with smaller batch", true).unwrap();

        assert_eq!(
            job_status(&core, &job_id).unwrap().as_deref(),
            Some("retry_wait")
        );
        assert_eq!(
            fs::read(workspace.join("checkpoint.bin")).unwrap(),
            b"resume"
        );
    }

    #[test]
    fn unmapped_source_candidate_does_not_rollback_mic_only_transcript() {
        let temp = tempfile::tempdir().unwrap();
        let core = CoreService::open(temp.path()).unwrap();
        let meeting_id = Uuid::now_v7().to_string();
        let speaker_id = namespaced_id(&meeting_id, "speaker:isolated:You");
        let now = now_ms();
        let connection = core.database().connect().unwrap();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Mic only','recording','processing',?2)",
                params![meeting_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO meeting_speakers(
                    id,meeting_id,cluster_label,display_name
                 ) VALUES (?1,?2,'isolated:You','You')",
                params![speaker_id, meeting_id],
            )
            .unwrap();
        drop(connection);

        let mut connection = core.database().connect().unwrap();
        let transaction = connection.transaction().unwrap();
        persist_speaker_candidate(
            &transaction,
            &meeting_id,
            PendingSpeakerCandidate {
                cluster_label: "SPEAKER_00".into(),
                clean_duration_ms: 12_000,
                encrypted_embedding: vec![1, 2, 3, 4],
            },
            now,
        )
        .unwrap();
        transaction.commit().unwrap();

        let connection = core.database().connect().unwrap();
        let candidate_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM voice_profile_candidates WHERE meeting_id=?1",
                [&meeting_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(candidate_count, 0);
    }

    #[test]
    fn canonical_import_honors_persisted_manual_speaker_merge() {
        let temp = tempfile::tempdir().unwrap();
        let core = CoreService::open(temp.path()).unwrap();
        let meeting_id = Uuid::now_v7().to_string();
        let source_id = Uuid::now_v7().to_string();
        let target_id = Uuid::now_v7().to_string();
        let model_output_id = Uuid::now_v7().to_string();
        let now = now_ms();
        let connection = core.database().connect().unwrap();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Test','import','processing',?2)",
                params![meeting_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO meeting_speakers(
                    id,meeting_id,cluster_label,display_name
                 ) VALUES (?1,?3,'source-cluster','Source'),
                          (?2,?3,'target-cluster','Target')",
                params![source_id, target_id, meeting_id],
            )
            .unwrap();
        core.merge_speakers(&meeting_id, &source_id, &target_id)
            .unwrap();
        connection
            .execute(
                "INSERT INTO model_outputs(
                    id,meeting_id,pipeline_version,model_revisions_json,
                    raw_result_json,is_canonical,created_at_ms
                 ) VALUES (?1,?2,?3,'{}','{}',1,?4)",
                params![model_output_id, meeting_id, PIPELINE_VERSION, now],
            )
            .unwrap();
        drop(connection);
        let mut connection = core.database().connect().unwrap();
        let transaction = connection.transaction().unwrap();
        import_turn(
            &transaction,
            &meeting_id,
            &model_output_id,
            &serde_json::Map::new(),
            &json!({
                "turn_id":"worker-turn",
                "speaker_cluster_id":"source-cluster",
                "speaker_name":"Unknown",
                "speaker_state":"Unknown",
                "start_ms":0,
                "end_ms":500,
                "model_text":"hello",
                "words":[{
                    "word_id":"worker-word",
                    "text":"hello",
                    "start_ms":0,
                    "end_ms":500,
                    "overlap":false
                }]
            }),
            now,
        )
        .unwrap();
        transaction.commit().unwrap();
        let connection = core.database().connect().unwrap();
        let imported_speaker: String = connection
            .query_row(
                "SELECT speaker_id FROM transcript_turns WHERE meeting_id=?1",
                [&meeting_id],
                |row| row.get(0),
            )
            .unwrap();
        let recreated_source: i64 = connection
            .query_row(
                "SELECT count(*) FROM meeting_speakers
                 WHERE meeting_id=?1 AND cluster_label='source-cluster'",
                [&meeting_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(imported_speaker, target_id);
        assert_eq!(recreated_source, 0);
    }

    #[test]
    fn reprocessing_reattaches_overlapping_human_edit_to_new_model_turn() {
        let temp = tempfile::tempdir().unwrap();
        let core = CoreService::open(temp.path()).unwrap();
        let meeting_id = Uuid::now_v7().to_string();
        let speaker_id = namespaced_id(&meeting_id, "speaker:speaker-0");
        let old_output = Uuid::now_v7().to_string();
        let new_output = Uuid::now_v7().to_string();
        let old_turn = Uuid::now_v7().to_string();
        let now = now_ms();
        let connection = core.database().connect().unwrap();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Test','import','processing',?2)",
                params![meeting_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO meeting_speakers(
                    id,meeting_id,cluster_label,display_name
                 ) VALUES (?1,?2,'speaker-0','Alex')",
                params![speaker_id, meeting_id],
            )
            .unwrap();
        for output in [&old_output, &new_output] {
            connection
                .execute(
                    "INSERT INTO model_outputs(
                        id,meeting_id,pipeline_version,model_revisions_json,
                        raw_result_json,created_at_ms
                     ) VALUES (?1,?2,?3,'{}','{}',?4)",
                    params![output, meeting_id, PIPELINE_VERSION, now],
                )
                .unwrap();
        }
        connection
            .execute(
                "INSERT INTO transcript_turns(
                    id,meeting_id,model_output_id,speaker_id,start_ms,end_ms,
                    model_text,edited_text,revision,created_at_ms,updated_at_ms
                 ) VALUES (?1,?2,?3,?4,1000,5000,'old model','human correction',2,?5,?5)",
                params![old_turn, meeting_id, old_output, speaker_id, now],
            )
            .unwrap();
        drop(connection);
        let mut connection = core.database().connect().unwrap();
        let transaction = connection.transaction().unwrap();
        import_turn(
            &transaction,
            &meeting_id,
            &new_output,
            &serde_json::Map::new(),
            &json!({
                "turn_id":"different-job-turn-id",
                "speaker_cluster_id":"speaker-0",
                "speaker_name":"Unknown",
                "speaker_state":"Unknown",
                "start_ms":1100,
                "end_ms":5100,
                "model_text":"new model",
                "words":[{
                    "word_id":"new-word",
                    "text":"new",
                    "start_ms":1100,
                    "end_ms":1600
                }]
            }),
            now,
        )
        .unwrap();
        transaction.commit().unwrap();
        let connection = core.database().connect().unwrap();
        let (count, turn_id, edited, model): (i64, String, String, String) = connection
            .query_row(
                "SELECT count(*),id,edited_text,model_text
                 FROM transcript_turns WHERE meeting_id=?1",
                [&meeting_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(turn_id, old_turn);
        assert_eq!(edited, "human correction");
        assert_eq!(model, "new model");
    }
}
