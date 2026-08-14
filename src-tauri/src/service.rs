use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, UNIX_EPOCH},
};

use parking_lot::{Condvar, Mutex};
use rusqlite::{backup::Backup, params, Connection, OptionalExtension, Row, TransactionBehavior};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::{
    db::{iso_from_ms, now_ms, Database, SCHEMA_VERSION},
    error::{CoreError, CoreResult},
    layout::{managed_directory_size, remove_managed_child_tree, AppLayout},
    media,
    media_tools::{self, MediaTool},
    models::*,
    performance::PerformanceSpan,
};

const DEFAULT_PROFILE_COLORS: &[&str] = &[
    "#6E8BFF", "#E9779D", "#37B4A3", "#E7A53C", "#A47BE8", "#5E9FDC",
];
const MAX_ASSET_CHUNK: u32 = 4 * 1024 * 1024;
const LIBRARY_SIZE_CACHE_TTL: Duration = Duration::from_secs(3);
#[cfg(target_arch = "aarch64")]
#[cfg(target_os = "macos")]
const MODEL_MANIFEST_JSON: &str = include_str!("../../worker/model-manifest.macos.json");
#[cfg(not(all(target_arch = "aarch64", target_os = "macos")))]
const MODEL_MANIFEST_JSON: &str = include_str!("../../worker/model-manifest.json");
pub(crate) const VOICE_PROFILE_MIN_SAMPLES: i64 = 1;
pub(crate) const VOICE_PROFILE_MIN_CLEAN_DURATION_MS: i64 = 10_000;

#[derive(Debug, Deserialize)]
struct BundledModelManifest {
    pipeline_version: String,
    models: Vec<BundledModelSpec>,
}

#[derive(Debug, Deserialize)]
struct BundledModelSpec {
    key: String,
    revision: String,
    files: Vec<BundledModelFile>,
}

#[derive(Debug, Deserialize)]
struct BundledModelFile {
    path: String,
    size: u64,
}

#[derive(Debug)]
pub struct CoreService {
    layout: AppLayout,
    database: Database,
    processing_queue: ProcessingQueueSignal,
    library_size_cache: Mutex<Option<(Instant, u64)>>,
}

#[derive(Debug, Default)]
struct ProcessingQueueSignal {
    generation: Mutex<u64>,
    changed: Condvar,
}

#[derive(Debug)]
pub(crate) struct StartupRecoveryScope {
    captured_at_ms: i64,
    recording_session_ids: BTreeSet<String>,
    recording_meeting_ids: BTreeSet<String>,
}

impl CoreService {
    pub fn open(root: impl Into<PathBuf>) -> CoreResult<Self> {
        let layout = AppLayout::create(root)?;
        let database = Database::open(layout.database())?;
        Ok(Self {
            layout,
            database,
            processing_queue: ProcessingQueueSignal::default(),
            library_size_cache: Mutex::new(None),
        })
    }

    pub fn open_with_runtime(
        root: impl Into<PathBuf>,
        bundled_runtime: PathBuf,
    ) -> CoreResult<Self> {
        let layout = AppLayout::create_with_runtime(root, Some(bundled_runtime))?;
        let database = Database::open(layout.database())?;
        Ok(Self {
            layout,
            database,
            processing_queue: ProcessingQueueSignal::default(),
            library_size_cache: Mutex::new(None),
        })
    }

    pub(crate) fn open_with_deferred_runtime(
        root: impl Into<PathBuf>,
        bundled_runtime: PathBuf,
    ) -> CoreResult<Self> {
        let layout = AppLayout::create_with_deferred_runtime(root, bundled_runtime)?;
        let database = Database::open(layout.database())?;
        Ok(Self {
            layout,
            database,
            processing_queue: ProcessingQueueSignal::default(),
            library_size_cache: Mutex::new(None),
        })
    }

    pub fn layout(&self) -> &AppLayout {
        &self.layout
    }

    pub fn database(&self) -> &Database {
        &self.database
    }

    pub(crate) fn processing_queue_generation(&self) -> u64 {
        *self.processing_queue.generation.lock()
    }

    pub(crate) fn wait_for_processing_queue_change(
        &self,
        observed_generation: u64,
        timeout: Duration,
    ) -> bool {
        let mut generation = self.processing_queue.generation.lock();
        if *generation == observed_generation {
            self.processing_queue
                .changed
                .wait_for(&mut generation, timeout);
        }
        *generation != observed_generation
    }

    pub(crate) fn notify_processing_queue(&self) {
        let mut generation = self.processing_queue.generation.lock();
        *generation = generation.wrapping_add(1);
        self.processing_queue.changed.notify_all();
    }

    pub(crate) fn invalidate_library_size_cache(&self) {
        self.library_size_cache.lock().take();
    }

    pub fn schema_version(&self) -> u32 {
        SCHEMA_VERSION
    }

    pub fn recover_interrupted_work(&self) -> CoreResult<()> {
        let recovery_scope = self.capture_startup_recovery_scope()?;
        self.recover_interrupted_processing_jobs()?;
        self.recover_recordings_and_cleanup(recovery_scope)
    }

    /// Snapshots crash-recovery ownership before the app becomes interactive.
    /// Background recovery may only mutate these sessions/meetings, preventing
    /// a newly started recording from being mistaken for stale work.
    pub(crate) fn capture_startup_recovery_scope(&self) -> CoreResult<StartupRecoveryScope> {
        let captured_at_ms = now_ms();
        let connection = self.database.connect()?;
        let mut statement = connection.prepare(
            "SELECT id,meeting_id FROM recording_sessions
             WHERE state IN ('starting','recording','paused','finalizing')",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(StartupRecoveryScope {
            captured_at_ms,
            recording_session_ids: rows.iter().map(|(id, _)| id.clone()).collect(),
            recording_meeting_ids: rows.into_iter().map(|(_, id)| id).collect(),
        })
    }

    /// Restores durable queue ownership before the coordinator starts. This is
    /// intentionally database-only so it remains safe on the first-paint path.
    pub(crate) fn recover_interrupted_processing_jobs(&self) -> CoreResult<()> {
        let _span = PerformanceSpan::new("startup_queue_recovery", "scope=database");
        let connection = self.database.connect()?;
        let now = now_ms();
        connection.execute(
            "UPDATE processing_jobs
             SET status = 'interrupted',
                 attempts = attempts + 1,
                 locked_at_ms = NULL,
                 worker_id = NULL,
                 error_code = 'WORKER_INTERRUPTED',
                 error_message = 'Application exited while this job was running',
                 updated_at_ms = ?1
             WHERE status IN ('running','cancel_requested')",
            [now],
        )?;
        connection.execute(
            "UPDATE processing_jobs
             SET status = CASE WHEN attempts >= max_attempts THEN 'failed' ELSE 'retry_wait' END,
                 error_message = CASE WHEN attempts >= max_attempts
                    THEN 'Processing was interrupted repeatedly'
                    ELSE 'Processing was interrupted and will be retried' END,
                 updated_at_ms = ?1
             WHERE status = 'interrupted'",
            [now],
        )?;
        self.notify_processing_queue();
        Ok(())
    }

    /// Performs filesystem scans, recording salvage, and stale partial cleanup
    /// after the UI can paint. Any recovered processing job wakes the queue.
    pub(crate) fn recover_recordings_and_cleanup(
        &self,
        recovery_scope: StartupRecoveryScope,
    ) -> CoreResult<()> {
        let _span = PerformanceSpan::new("startup_file_recovery", "scope=managed_library");
        self.repair_recording_partials(&recovery_scope)?;
        let recovered = crate::recording::recover_interrupted_recordings(
            self,
            &recovery_scope.recording_session_ids,
        )?;
        if recovered > 0 {
            log::warn!("recovered {recovered} interrupted recording session(s)");
        }
        self.remove_abandoned_import_partials(recovery_scope.captured_at_ms)?;
        let connection = self.database.connect()?;
        let now = now_ms();
        for session_id in &recovery_scope.recording_session_ids {
            connection.execute(
                "UPDATE recording_sessions
                 SET state='failed', ended_at_ms=?1,
                     error_message=COALESCE(error_message, 'Application exited during recording')
                 WHERE id=?2 AND state IN ('starting','recording','paused','finalizing')",
                params![now, session_id],
            )?;
        }
        for meeting_id in &recovery_scope.recording_meeting_ids {
            connection.execute(
                "UPDATE meetings
                 SET status='failed', ended_at_ms=COALESCE(ended_at_ms, ?1)
                 WHERE id=?2 AND status='recording'",
                params![now, meeting_id],
            )?;
        }
        self.invalidate_library_size_cache();
        self.notify_processing_queue();
        Ok(())
    }

    pub fn first_run(&self) -> CoreResult<bool> {
        let connection = self.database.connect()?;
        let meetings: i64 =
            connection.query_row("SELECT count(*) FROM meetings", [], |row| row.get(0))?;
        let setup_complete = connection
            .query_row(
                "SELECT value FROM app_meta WHERE key='setup_complete'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .as_deref()
            == Some("true");
        Ok(meetings == 0 && !setup_complete)
    }

    pub fn library_stats(&self) -> CoreResult<LibraryStats> {
        let connection = self.database.connect()?;
        let (meeting_count, recording_count, processing_count) = connection.query_row(
                "SELECT
                    (SELECT count(*) FROM meetings),
                    (SELECT count(*) FROM meetings WHERE source_kind='recording'),
                    (SELECT count(*) FROM processing_jobs
                     WHERE status IN ('queued','running','retry_wait','cancel_requested','interrupted'))",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)? as u64,
                        row.get::<_, i64>(1)? as u64,
                        row.get::<_, i64>(2)? as u64,
                    ))
                },
            )?;
        let storage_bytes = self.cached_library_size()?;
        Ok(LibraryStats {
            meeting_count,
            recording_count,
            processing_count,
            storage_bytes,
        })
    }

    fn cached_library_size(&self) -> CoreResult<u64> {
        if let Some((measured_at, bytes)) = *self.library_size_cache.lock() {
            if measured_at.elapsed() <= LIBRARY_SIZE_CACHE_TTL {
                return Ok(bytes);
            }
        }
        let bytes = managed_directory_size(self.layout.library())?;
        *self.library_size_cache.lock() = Some((Instant::now(), bytes));
        Ok(bytes)
    }

    pub fn import_media(
        &self,
        source_path: &Path,
        title: Option<String>,
    ) -> CoreResult<ImportMediaResult> {
        media::validate_source(source_path)?;
        let probe = media::probe(&self.layout, source_path)?;
        let meeting_id = new_id();
        let asset_id = new_id();
        let job_id = new_id();
        let original_name = source_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| CoreError::InvalidInput("media filename is not valid Unicode".into()))?;
        let destination_directory = self.layout.media().join(&meeting_id).join(&asset_id);
        let destination = destination_directory.join(original_name);
        let (size_bytes, sha256) = media::atomic_copy_with_hash(source_path, &destination)?;
        let relative = self.layout.relative_to_root(&destination)?;
        let now = now_ms();
        let title = title
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| {
                source_path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("Imported meeting")
                    .to_string()
            });
        let input_json = json!({
            "assetId": asset_id,
            "sourceKind": "managed_import",
            "relativePath": relative,
            "pipelineVersion": crate::worker::PIPELINE_VERSION,
        })
        .to_string();

        let write_result = (|| -> CoreResult<()> {
            let mut connection = self.database.connect()?;
            let transaction = connection.transaction()?;
            transaction.execute(
                "INSERT INTO meetings(
                    id,title,source_kind,status,created_at_ms,duration_ms,language
                 ) VALUES (?1,?2,'import','processing',?3,?4,'en')",
                params![meeting_id, title, now, probe.duration_ms],
            )?;
            transaction.execute(
                "INSERT INTO media_assets(
                    id,meeting_id,kind,display_name,relative_path,content_type,size_bytes,
                    sha256,duration_ms,codec,sample_rate_hz,channels,created_at_ms
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                params![
                    asset_id,
                    meeting_id,
                    probe.kind,
                    original_name,
                    relative,
                    probe.content_type,
                    size_bytes as i64,
                    sha256,
                    probe.duration_ms,
                    probe.codec,
                    probe.sample_rate_hz,
                    probe.channels,
                    now,
                ],
            )?;
            transaction.execute(
                "INSERT INTO processing_jobs(
                    id,meeting_id,stage,status,progress,input_json,created_at_ms,updated_at_ms
                 ) VALUES (?1,?2,'ingest','queued',0,?3,?4,?4)",
                params![job_id, meeting_id, input_json, now],
            )?;
            transaction.commit()?;
            Ok(())
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&destination);
            return Err(error);
        }
        self.invalidate_library_size_cache();
        self.notify_processing_queue();
        Ok(ImportMediaResult {
            meeting: self.get_meeting_summary(&meeting_id)?,
            asset: self.get_asset(&asset_id)?,
            job: self.get_job(&job_id)?,
        })
    }

    pub fn list_meetings(&self, request: MeetingListRequest) -> CoreResult<Vec<Meeting>> {
        let connection = self.database.connect()?;
        let status = request.status.map(status_to_db);
        let query = request
            .query
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let query_like = query.map(|value| format!("%{value}%"));
        let limit = request.limit.unwrap_or(100).clamp(1, 500) as i64;
        let offset = request.offset.unwrap_or(0) as i64;
        let mut statement = connection.prepare(&format!(
            "{} WHERE (?1 IS NULL OR m.status=?1)
                 AND (?2 IS NULL OR m.title LIKE ?2 ESCAPE '\\')
             ORDER BY m.created_at_ms DESC
             LIMIT ?3 OFFSET ?4",
            meeting_select_sql()
        ))?;
        let meetings = statement
            .query_map(params![status, query_like, limit, offset], map_meeting)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(meetings)
    }

    pub fn get_meeting_summary(&self, meeting_id: &str) -> CoreResult<Meeting> {
        validate_id(meeting_id, "meeting")?;
        let connection = self.database.connect()?;
        connection
            .query_row(
                &format!("{} WHERE m.id=?1", meeting_select_sql()),
                [meeting_id],
                map_meeting,
            )
            .optional()?
            .ok_or_else(|| CoreError::NotFound(format!("meeting {meeting_id}")))
    }

    pub fn rename_meeting(&self, meeting_id: &str, title: String) -> CoreResult<Meeting> {
        validate_id(meeting_id, "meeting")?;
        let title = title.trim();
        if title.is_empty() || title.chars().count() > 180 {
            return Err(CoreError::InvalidInput(
                "meeting title must be between 1 and 180 characters".into(),
            ));
        }
        let connection = self.database.connect()?;
        let updated = connection.execute(
            "UPDATE meetings SET title=?1 WHERE id=?2",
            params![title, meeting_id],
        )?;
        if updated == 0 {
            return Err(CoreError::NotFound(format!("meeting {meeting_id}")));
        }
        self.get_meeting_summary(meeting_id)
    }

    pub fn get_meeting(&self, meeting_id: &str) -> CoreResult<MeetingDetail> {
        let _span = PerformanceSpan::new("meeting_detail_load", "query=batched");
        let meeting = self.get_meeting_summary(meeting_id)?;
        let connection = self.database.connect()?;

        let mut asset_statement = connection.prepare(
            "SELECT id,meeting_id,kind,display_name,content_type,size_bytes,sha256,
                    duration_ms,codec,sample_rate_hz,channels,created_at_ms
             FROM media_assets WHERE meeting_id=?1 ORDER BY created_at_ms",
        )?;
        let assets = asset_statement
            .query_map([meeting_id], map_asset)?
            .collect::<Result<Vec<_>, _>>()?;

        let mut speaker_statement = connection.prepare(
            "SELECT id,meeting_id,cluster_label,display_name,profile_id,match_state,
                    needs_review,color
             FROM meeting_speakers WHERE meeting_id=?1 ORDER BY cluster_label",
        )?;
        let mut speakers = speaker_statement
            .query_map([meeting_id], map_speaker)?
            .collect::<Result<Vec<_>, _>>()?;
        normalize_unknown_speaker_names(&mut speakers);

        let mut turn_statement = connection.prepare(
            "SELECT id,meeting_id,speaker_id,start_ms,end_ms,model_text,edited_text,
                    revision,needs_review,is_draft,is_marked
             FROM transcript_turns WHERE meeting_id=?1 ORDER BY start_ms,id",
        )?;
        let mut turns = turn_statement
            .query_map([meeting_id], map_turn_without_words)?
            .collect::<Result<Vec<_>, _>>()?;
        let mut word_statement = connection.prepare(
            "SELECT w.id,w.turn_id,w.start_ms,w.end_ms,w.text,w.confidence,
                    w.speaker_id,w.is_overlap
             FROM words w
             JOIN transcript_turns t ON t.id=w.turn_id
             WHERE t.meeting_id=?1
             ORDER BY t.start_ms,t.id,w.sequence",
        )?;
        let mut words_by_turn = BTreeMap::<String, Vec<WordTiming>>::new();
        for word in word_statement
            .query_map([meeting_id], map_word)?
            .collect::<Result<Vec<_>, _>>()?
        {
            words_by_turn
                .entry(word.turn_id.clone())
                .or_default()
                .push(word);
        }
        for turn in &mut turns {
            turn.words = words_by_turn.remove(&turn.id).unwrap_or_default();
        }

        let mut marker_statement = connection.prepare(
            "SELECT id,meeting_id,offset_ms,label,created_at_ms
             FROM recording_markers WHERE meeting_id=?1 ORDER BY offset_ms",
        )?;
        let markers = marker_statement
            .query_map([meeting_id], |row| {
                Ok(RecordingMarker {
                    id: row.get(0)?,
                    meeting_id: row.get(1)?,
                    at_ms: row.get(2)?,
                    label: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    created_at_ms: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(MeetingDetail {
            meeting,
            assets,
            speakers,
            turns,
            markers,
        })
    }

    pub fn delete_meeting(&self, meeting_id: &str) -> CoreResult<()> {
        validate_id(meeting_id, "meeting")?;
        let mut connection = self.database.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists = transaction
            .query_row("SELECT 1 FROM meetings WHERE id=?1", [meeting_id], |_| {
                Ok(())
            })
            .optional()?
            .is_some();
        if !exists {
            return Err(CoreError::NotFound(format!("meeting {meeting_id}")));
        }
        let active_jobs: i64 = transaction.query_row(
            "SELECT count(*) FROM processing_jobs
             WHERE meeting_id=?1 AND status NOT IN ('completed','failed','cancelled')",
            [meeting_id],
            |row| row.get(0),
        )?;
        if active_jobs > 0 {
            return Err(CoreError::Conflict(
                "meeting has active or resumable processing; cancel it before deleting".into(),
            ));
        }
        let active_recordings: i64 = transaction.query_row(
            "SELECT count(*) FROM recording_sessions
             WHERE meeting_id=?1 AND state NOT IN ('stopped','failed')",
            [meeting_id],
            |row| row.get(0),
        )?;
        if active_recordings > 0 {
            return Err(CoreError::Conflict(
                "meeting has an active recording session".into(),
            ));
        }

        let mut paths = Vec::new();
        {
            let mut statement = transaction.prepare(
                "SELECT relative_path FROM media_assets WHERE meeting_id=?1
                 UNION ALL
                 SELECT relative_path FROM artifacts WHERE meeting_id=?1",
            )?;
            paths.extend(
                statement
                    .query_map([meeting_id], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?,
            );
        }
        let terminal_job_ids = {
            let mut statement = transaction.prepare(
                "SELECT id FROM processing_jobs
                 WHERE meeting_id=?1 AND status IN ('completed','failed','cancelled')",
            )?;
            let job_ids = statement
                .query_map([meeting_id], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            job_ids
        };
        transaction.execute("DELETE FROM meetings WHERE id=?1", [meeting_id])?;
        transaction.commit()?;

        for relative in paths {
            match self.layout.resolve_relative(&relative) {
                Ok(path) => {
                    if let Err(error) = fs::remove_file(path) {
                        if error.kind() != std::io::ErrorKind::NotFound {
                            log::warn!("failed to remove deleted meeting media: {error}");
                        }
                    }
                }
                Err(error) => log::error!("refused unsafe stored media path: {error}"),
            }
        }

        for (label, parent) in [
            ("media", self.layout.media()),
            ("recording", self.layout.recordings()),
            ("artifact", self.layout.artifacts()),
        ] {
            if let Err(error) = remove_managed_child_tree(self.layout.library(), parent, meeting_id)
            {
                log::error!(
                    "refused or failed to remove deleted meeting {label} directory: {error}"
                );
            }
        }
        for job_id in terminal_job_ids {
            if validate_id(&job_id, "processing job").is_err() {
                log::error!("refused invalid processing job id during meeting cleanup");
                continue;
            }
            if let Err(error) =
                remove_managed_child_tree(self.layout.library(), self.layout.work(), &job_id)
            {
                log::error!("refused or failed to remove terminal job workspace {job_id}: {error}");
            }
        }
        self.invalidate_library_size_cache();
        Ok(())
    }

    pub fn search_transcript(
        &self,
        request: TranscriptSearchRequest,
    ) -> CoreResult<Vec<TranscriptSearchHit>> {
        let query = fts_query(&request.query)?;
        let connection = self.database.connect()?;
        let limit = request.limit.unwrap_or(50).clamp(1, 200) as i64;
        let mut statement = connection.prepare(
            "SELECT f.meeting_id,f.turn_id,t.start_ms,s.display_name,
                    COALESCE(t.edited_text,t.model_text),
                    snippet(transcript_fts,2,'<mark>','</mark>','…',24)
             FROM transcript_fts f
             JOIN transcript_turns t ON t.id=f.turn_id
             LEFT JOIN meeting_speakers s ON s.id=t.speaker_id
             WHERE transcript_fts MATCH ?1
               AND (?2 IS NULL OR f.meeting_id=?2)
             ORDER BY rank
             LIMIT ?3",
        )?;
        let hits = statement
            .query_map(params![query, request.meeting_id, limit], |row| {
                Ok(TranscriptSearchHit {
                    meeting_id: row.get(0)?,
                    turn_id: row.get(1)?,
                    start_ms: row.get(2)?,
                    speaker_name: row.get(3)?,
                    text: row.get(4)?,
                    snippet: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(hits)
    }

    pub fn agent_context_turns(&self, meeting_id: &str) -> CoreResult<Vec<AgentContextTurn>> {
        validate_id(meeting_id, "meeting")?;
        let connection = self.database.connect()?;
        let exists = connection
            .query_row("SELECT 1 FROM meetings WHERE id=?1", [meeting_id], |_| {
                Ok(())
            })
            .optional()?
            .is_some();
        if !exists {
            return Err(CoreError::NotFound(format!("meeting {meeting_id}")));
        }
        let mut statement = connection.prepare(
            "SELECT t.id,t.start_ms,t.end_ms,
                    COALESCE(NULLIF(s.display_name,''),'Unknown speaker'),
                    COALESCE(t.edited_text,t.model_text)
             FROM transcript_turns t
             LEFT JOIN meeting_speakers s ON s.id=t.speaker_id
             WHERE t.meeting_id=?1 AND t.is_draft=0
             ORDER BY t.start_ms,t.id",
        )?;
        let turns = statement
            .query_map([meeting_id], |row| {
                Ok(AgentContextTurn {
                    turn_id: row.get(0)?,
                    start_ms: row.get(1)?,
                    end_ms: row.get(2)?,
                    speaker_name: row.get(3)?,
                    text: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(turns)
    }

    pub fn list_transcript_chat(
        &self,
        meeting_id: &str,
        limit: u32,
    ) -> CoreResult<Vec<TranscriptChatMessage>> {
        validate_id(meeting_id, "meeting")?;
        let connection = self.database.connect()?;
        let mut statement = connection.prepare(
            "SELECT id,meeting_id,role,content,citations_json,model,created_at_ms
             FROM transcript_chat_messages
             WHERE meeting_id=?1
             ORDER BY created_at_ms DESC,id DESC
             LIMIT ?2",
        )?;
        let mut messages = statement
            .query_map(params![meeting_id, limit.clamp(1, 200)], map_chat_message)?
            .collect::<Result<Vec<_>, _>>()?;
        messages.reverse();
        Ok(messages)
    }

    pub fn save_transcript_chat_message(
        &self,
        meeting_id: &str,
        role: &str,
        content: String,
        citations: Vec<TranscriptCitation>,
        model: Option<String>,
    ) -> CoreResult<TranscriptChatMessage> {
        validate_id(meeting_id, "meeting")?;
        if !matches!(role, "user" | "assistant") {
            return Err(CoreError::InvalidInput("invalid chat message role".into()));
        }
        let content = content.trim();
        if content.is_empty() || content.chars().count() > 8_000 {
            return Err(CoreError::InvalidInput(
                "chat message must be between 1 and 8000 characters".into(),
            ));
        }
        let id = Uuid::now_v7().to_string();
        let created_at_ms = now_ms();
        let citations_json = serde_json::to_string(&citations)?;
        let connection = self.database.connect()?;
        connection.execute(
            "INSERT INTO transcript_chat_messages(
                id,meeting_id,role,content,citations_json,model,created_at_ms
             ) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                id,
                meeting_id,
                role,
                content,
                citations_json,
                model,
                created_at_ms
            ],
        )?;
        Ok(TranscriptChatMessage {
            id,
            meeting_id: meeting_id.to_string(),
            role: role.to_string(),
            content: content.to_string(),
            citations,
            model,
            created_at_ms,
        })
    }

    pub fn clear_transcript_chat(&self, meeting_id: &str) -> CoreResult<()> {
        validate_id(meeting_id, "meeting")?;
        let connection = self.database.connect()?;
        connection.execute(
            "DELETE FROM transcript_chat_messages WHERE meeting_id=?1",
            [meeting_id],
        )?;
        Ok(())
    }

    pub fn update_transcript_turn(
        &self,
        turn_id: &str,
        edited_text: String,
        expected_revision: Option<u32>,
    ) -> CoreResult<TranscriptTurn> {
        validate_id(turn_id, "turn")?;
        let text = edited_text.trim().to_string();
        let mut connection = self.database.connect()?;
        let transaction = connection.transaction()?;
        let (meeting_id, prior_text, revision): (String, Option<String>, u32) = transaction
            .query_row(
                "SELECT meeting_id,edited_text,revision FROM transcript_turns WHERE id=?1",
                [turn_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| CoreError::NotFound(format!("transcript turn {turn_id}")))?;
        if let Some(expected) = expected_revision {
            if expected != revision {
                return Err(CoreError::Conflict(format!(
                    "turn was updated elsewhere; expected revision {expected}, found {revision}"
                )));
            }
        }
        let new_revision = revision.saturating_add(1);
        let next_text = if text.is_empty() { None } else { Some(text) };
        transaction.execute(
            "INSERT INTO transcript_turn_revisions(
                id,turn_id,revision,prior_edited_text,new_edited_text,created_at_ms
             ) VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                new_id(),
                turn_id,
                new_revision,
                prior_text,
                next_text,
                now_ms()
            ],
        )?;
        transaction.execute(
            "UPDATE transcript_turns
             SET edited_text=?1,revision=?2,updated_at_ms=?3
             WHERE id=?4",
            params![next_text, new_revision, now_ms(), turn_id],
        )?;
        transaction.execute(
            "UPDATE meetings SET has_user_edits=1 WHERE id=?1",
            [meeting_id],
        )?;
        transaction.commit()?;
        self.get_turn(turn_id)
    }

    pub fn set_transcript_turn_review(
        &self,
        turn_id: &str,
        needs_review: bool,
    ) -> CoreResult<TranscriptTurn> {
        validate_id(turn_id, "turn")?;
        let connection = self.database.connect()?;
        let updated = connection.execute(
            "UPDATE transcript_turns
             SET needs_review=?1,updated_at_ms=?2
             WHERE id=?3",
            params![i64::from(needs_review), now_ms(), turn_id],
        )?;
        if updated == 0 {
            return Err(CoreError::NotFound(format!("transcript turn {turn_id}")));
        }
        self.get_turn(turn_id)
    }

    pub fn set_transcript_turn_bookmark(
        &self,
        turn_id: &str,
        is_marked: bool,
    ) -> CoreResult<TranscriptTurn> {
        validate_id(turn_id, "turn")?;
        let connection = self.database.connect()?;
        let updated = connection.execute(
            "UPDATE transcript_turns
             SET is_marked=?1,updated_at_ms=?2
             WHERE id=?3",
            params![i64::from(is_marked), now_ms(), turn_id],
        )?;
        if updated == 0 {
            return Err(CoreError::NotFound(format!("transcript turn {turn_id}")));
        }
        self.get_turn(turn_id)
    }

    pub fn rename_speaker(
        &self,
        meeting_id: &str,
        speaker_id: &str,
        display_name: String,
    ) -> CoreResult<RenameSpeakerResult> {
        validate_id(meeting_id, "meeting")?;
        validate_id(speaker_id, "speaker")?;
        let name = nonempty_name(display_name, "speaker name")?;
        let now = now_ms();
        let mut connection = self.database.connect()?;
        let transaction = connection.transaction()?;
        let (match_state, current_profile_id): (String, Option<String>) = transaction
            .query_row(
                "SELECT match_state,profile_id FROM meeting_speakers
                 WHERE id=?1 AND meeting_id=?2",
                params![speaker_id, meeting_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| CoreError::NotFound(format!("speaker {speaker_id}")))?;

        let should_save_profile = match_state != "matched" || current_profile_id.is_none();
        let mut profile_id = current_profile_id;
        let mut profile_created = false;
        let mut sample_saved = false;

        if should_save_profile {
            profile_id = transaction
                .query_row(
                    "SELECT id FROM voice_profiles
                     WHERE trim(display_name)=?1 COLLATE NOCASE
                     ORDER BY created_at_ms,id LIMIT 1",
                    [&name],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if profile_id.is_none() {
                let id = new_id();
                transaction.execute(
                    "INSERT INTO voice_profiles(
                        id,display_name,color,created_at_ms,updated_at_ms,last_used_at_ms
                     ) VALUES (?1,?2,?3,?4,?4,?4)",
                    params![id, name, color_for(&id), now],
                )?;
                profile_id = Some(id);
                profile_created = true;
            }
            let saved_profile_id = profile_id
                .as_deref()
                .ok_or_else(|| CoreError::NotFound("automatic voice profile".into()))?;
            let candidate: Option<(i64, Vec<u8>)> = transaction
                .query_row(
                    "SELECT clean_duration_ms,encrypted_embedding
                     FROM voice_profile_candidates
                     WHERE meeting_id=?1 AND speaker_id=?2",
                    params![meeting_id, speaker_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((clean_duration_ms, protected)) = candidate {
                transaction.execute(
                    "INSERT INTO voice_profile_samples(
                        id,profile_id,meeting_id,speaker_id,clean_duration_ms,
                        encrypted_embedding,confirmed_at_ms
                     ) VALUES (?1,?2,?3,?4,?5,?6,?7)
                     ON CONFLICT(profile_id,meeting_id,speaker_id) DO UPDATE SET
                        clean_duration_ms=excluded.clean_duration_ms,
                        encrypted_embedding=excluded.encrypted_embedding,
                        confirmed_at_ms=excluded.confirmed_at_ms",
                    params![
                        new_id(),
                        saved_profile_id,
                        meeting_id,
                        speaker_id,
                        clean_duration_ms,
                        protected,
                        now
                    ],
                )?;
                transaction.execute(
                    "DELETE FROM voice_profile_candidates
                     WHERE meeting_id=?1 AND speaker_id=?2",
                    params![meeting_id, speaker_id],
                )?;
                sample_saved = true;
            }
            transaction.execute(
                "UPDATE voice_profiles SET updated_at_ms=?1,last_used_at_ms=?1 WHERE id=?2",
                params![now, saved_profile_id],
            )?;
            transaction.execute(
                "UPDATE meeting_speakers
                 SET display_name=?1,profile_id=?2,match_state='matched',needs_review=0
                 WHERE id=?3 AND meeting_id=?4",
                params![name, saved_profile_id, speaker_id, meeting_id],
            )?;
        } else {
            transaction.execute(
                "UPDATE meeting_speakers SET display_name=?1
                 WHERE id=?2 AND meeting_id=?3",
                params![name, speaker_id, meeting_id],
            )?;
        }
        transaction.commit()?;

        let speaker = self
            .get_meeting(meeting_id)?
            .speakers
            .into_iter()
            .find(|speaker| speaker.id == speaker_id)
            .ok_or_else(|| CoreError::NotFound(format!("speaker {speaker_id}")))?;
        let profile = if should_save_profile {
            let saved_profile_id = profile_id
                .as_deref()
                .ok_or_else(|| CoreError::NotFound("automatic voice profile".into()))?;
            self.list_voice_profiles()?
                .into_iter()
                .find(|profile| profile.id == saved_profile_id)
        } else {
            None
        };
        Ok(RenameSpeakerResult {
            speaker,
            profile,
            profile_created,
            sample_saved,
        })
    }

    pub fn merge_speakers(
        &self,
        meeting_id: &str,
        source_speaker_id: &str,
        target_speaker_id: &str,
    ) -> CoreResult<()> {
        if source_speaker_id == target_speaker_id {
            return Err(CoreError::InvalidInput(
                "source and target speakers must differ".into(),
            ));
        }
        let mut connection = self.database.connect()?;
        let transaction = connection.transaction()?;
        let count: i64 = transaction.query_row(
            "SELECT count(*) FROM meeting_speakers
             WHERE meeting_id=?1 AND id IN (?2,?3)",
            params![meeting_id, source_speaker_id, target_speaker_id],
            |row| row.get(0),
        )?;
        if count != 2 {
            return Err(CoreError::NotFound(
                "both speakers must belong to the meeting".into(),
            ));
        }
        let source_cluster: String = transaction.query_row(
            "SELECT cluster_label FROM meeting_speakers WHERE id=?1",
            [source_speaker_id],
            |row| row.get(0),
        )?;
        // Preserve this correction across deterministic model reprocessing.
        // Rules that previously targeted the source follow the merge chain.
        transaction.execute(
            "UPDATE speaker_merge_rules SET target_speaker_id=?1
             WHERE target_speaker_id=?2",
            params![target_speaker_id, source_speaker_id],
        )?;
        transaction.execute(
            "INSERT INTO speaker_merge_rules(
                meeting_id,source_cluster_label,target_speaker_id,created_at_ms
             ) VALUES (?1,?2,?3,?4)
             ON CONFLICT(meeting_id,source_cluster_label) DO UPDATE SET
                target_speaker_id=excluded.target_speaker_id,
                created_at_ms=excluded.created_at_ms",
            params![meeting_id, source_cluster, target_speaker_id, now_ms()],
        )?;
        transaction.execute(
            "UPDATE transcript_turns SET speaker_id=?1 WHERE speaker_id=?2",
            params![target_speaker_id, source_speaker_id],
        )?;
        transaction.execute(
            "UPDATE words SET speaker_id=?1 WHERE speaker_id=?2",
            params![target_speaker_id, source_speaker_id],
        )?;
        transaction.execute(
            "UPDATE speaker_clusters SET meeting_speaker_id=?1 WHERE meeting_speaker_id=?2",
            params![target_speaker_id, source_speaker_id],
        )?;
        let source_candidate_duration = transaction
            .query_row(
                "SELECT clean_duration_ms FROM voice_profile_candidates WHERE speaker_id=?1",
                [source_speaker_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let target_candidate_duration = transaction
            .query_row(
                "SELECT clean_duration_ms FROM voice_profile_candidates WHERE speaker_id=?1",
                [target_speaker_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if source_candidate_duration.is_some()
            && source_candidate_duration > target_candidate_duration
        {
            transaction.execute(
                "DELETE FROM voice_profile_candidates WHERE speaker_id=?1",
                [target_speaker_id],
            )?;
            transaction.execute(
                "UPDATE voice_profile_candidates SET speaker_id=?1 WHERE speaker_id=?2",
                params![target_speaker_id, source_speaker_id],
            )?;
        } else {
            transaction.execute(
                "DELETE FROM voice_profile_candidates WHERE speaker_id=?1",
                [source_speaker_id],
            )?;
        }
        transaction.execute(
            "DELETE FROM meeting_speakers WHERE id=?1",
            [source_speaker_id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn review_speaker_match(&self, speaker_id: &str, accepted: bool) -> CoreResult<()> {
        let connection = self.database.connect()?;
        let updated = if accepted {
            connection.execute(
                "UPDATE meeting_speakers
                 SET display_name=(
                         SELECT display_name FROM voice_profiles
                         WHERE voice_profiles.id=meeting_speakers.profile_id
                     ),
                     match_state='matched',needs_review=0
                 WHERE id=?1 AND profile_id IS NOT NULL",
                [speaker_id],
            )?
        } else {
            let cluster_label = connection
                .query_row(
                    "SELECT cluster_label FROM meeting_speakers WHERE id=?1",
                    [speaker_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .ok_or_else(|| CoreError::NotFound(format!("speaker {speaker_id}")))?;
            let default_name = default_unknown_speaker_name(&cluster_label);
            connection.execute(
                "UPDATE meeting_speakers
                 SET profile_id=NULL,display_name=?1,
                     match_state='unknown',needs_review=1
                 WHERE id=?2",
                params![default_name, speaker_id],
            )?
        };
        if updated == 0 {
            let exists: i64 = connection.query_row(
                "SELECT count(*) FROM meeting_speakers WHERE id=?1",
                [speaker_id],
                |row| row.get(0),
            )?;
            return if exists == 0 {
                Err(CoreError::NotFound(format!("speaker {speaker_id}")))
            } else {
                Err(CoreError::Conflict(
                    "a remembered identity must be explicitly confirmed before acceptance".into(),
                ))
            };
        }
        Ok(())
    }

    pub fn set_speaker_review(&self, speaker_id: &str, needs_review: bool) -> CoreResult<()> {
        let connection = self.database.connect()?;
        let updated = connection.execute(
            "UPDATE meeting_speakers SET needs_review=?1 WHERE id=?2",
            params![i64::from(needs_review), speaker_id],
        )?;
        if updated == 0 {
            return Err(CoreError::NotFound(format!("speaker {speaker_id}")));
        }
        Ok(())
    }

    pub fn list_voice_profiles(&self) -> CoreResult<Vec<VoiceProfile>> {
        let connection = self.database.connect()?;
        let mut statement = connection.prepare(
            "SELECT p.id,p.display_name,p.color,p.created_at_ms,p.updated_at_ms,p.last_used_at_ms,
                    count(s.id),COALESCE(sum(s.clean_duration_ms),0)
             FROM voice_profiles p
             LEFT JOIN voice_profile_samples s ON s.profile_id=p.id
             GROUP BY p.id
             ORDER BY lower(p.display_name)",
        )?;
        let profiles = statement
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let name: String = row.get(1)?;
                let color = row
                    .get::<_, Option<String>>(2)?
                    .unwrap_or_else(|| color_for(&id));
                let created: i64 = row.get(3)?;
                let updated: i64 = row.get(4)?;
                let last_used: Option<i64> = row.get(5)?;
                let sample_count = row.get::<_, i64>(6)? as u32;
                let duration: i64 = row.get(7)?;
                let ready = sample_count as i64 >= VOICE_PROFILE_MIN_SAMPLES
                    && duration >= VOICE_PROFILE_MIN_CLEAN_DURATION_MS;
                Ok(VoiceProfile {
                    id,
                    name: name.clone(),
                    display_name: name.clone(),
                    initials: initials(&name),
                    color,
                    created_at_ms: created,
                    updated_at_ms: updated,
                    last_used_at: iso_from_ms(last_used.unwrap_or(updated)),
                    sample_count,
                    sample_duration_ms: duration,
                    total_clean_duration_ms: duration,
                    ready_for_matching: ready,
                    status: if ready { "ready" } else { "needs_samples" }.into(),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(profiles)
    }

    pub fn create_voice_profile(&self, display_name: String) -> CoreResult<VoiceProfile> {
        let name = nonempty_name(display_name, "profile name")?;
        let id = new_id();
        let now = now_ms();
        let color = color_for(&id);
        let connection = self.database.connect()?;
        connection.execute(
            "INSERT INTO voice_profiles(
                id,display_name,color,created_at_ms,updated_at_ms
             ) VALUES (?1,?2,?3,?4,?4)",
            params![id, name, color, now],
        )?;
        self.list_voice_profiles()?
            .into_iter()
            .find(|profile| profile.id == id)
            .ok_or_else(|| CoreError::NotFound(format!("new profile {id}")))
    }

    pub fn delete_voice_profile(&self, profile_id: &str) -> CoreResult<()> {
        validate_id(profile_id, "voice profile")?;
        let mut connection = self.database.connect()?;
        let transaction = connection.transaction()?;
        let affected_speakers = {
            let mut statement = transaction
                .prepare("SELECT id,cluster_label FROM meeting_speakers WHERE profile_id=?1")?;
            let speakers = statement
                .query_map([profile_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            speakers
        };
        for (speaker_id, cluster_label) in affected_speakers {
            transaction.execute(
                "UPDATE meeting_speakers
                 SET profile_id=NULL,display_name=?1,match_state='unknown',needs_review=1
                 WHERE id=?2",
                params![default_unknown_speaker_name(&cluster_label), speaker_id],
            )?;
        }
        let deleted =
            transaction.execute("DELETE FROM voice_profiles WHERE id=?1", [profile_id])?;
        if deleted == 0 {
            return Err(CoreError::NotFound(format!("voice profile {profile_id}")));
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn confirm_voice_profile_sample(
        &self,
        request: ConfirmVoiceSampleRequest,
    ) -> CoreResult<VoiceProfile> {
        let now = now_ms();
        let mut connection = self.database.connect()?;
        let transaction = connection.transaction()?;
        let belongs: i64 = transaction.query_row(
            "SELECT count(*) FROM meeting_speakers
             WHERE id=?1 AND meeting_id=?2",
            params![request.speaker_id, request.meeting_id],
            |row| row.get(0),
        )?;
        if belongs != 1 {
            return Err(CoreError::InvalidInput(
                "confirmed speaker does not belong to the meeting".into(),
            ));
        }
        let candidate: Option<(i64, Vec<u8>)> = transaction
            .query_row(
                "SELECT clean_duration_ms,encrypted_embedding
                 FROM voice_profile_candidates
                 WHERE meeting_id=?1 AND speaker_id=?2",
                params![request.meeting_id, request.speaker_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (clean_duration_ms, protected) = candidate.ok_or_else(|| {
            CoreError::NotFound(
                "no encrypted clean-speech candidate is available for this speaker".into(),
            )
        })?;
        let profile_exists: i64 = transaction.query_row(
            "SELECT count(*) FROM voice_profiles WHERE id=?1",
            [&request.profile_id],
            |row| row.get(0),
        )?;
        if profile_exists != 1 {
            return Err(CoreError::NotFound(format!(
                "voice profile {}",
                request.profile_id
            )));
        }
        transaction.execute(
            "INSERT INTO voice_profile_samples(
                id,profile_id,meeting_id,speaker_id,clean_duration_ms,
                encrypted_embedding,confirmed_at_ms
             ) VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(profile_id,meeting_id,speaker_id) DO UPDATE SET
                clean_duration_ms=excluded.clean_duration_ms,
                encrypted_embedding=excluded.encrypted_embedding,
                confirmed_at_ms=excluded.confirmed_at_ms",
            params![
                new_id(),
                request.profile_id,
                request.meeting_id,
                request.speaker_id,
                clean_duration_ms,
                protected,
                now
            ],
        )?;
        transaction.execute(
            "DELETE FROM voice_profile_candidates
             WHERE meeting_id=?1 AND speaker_id=?2",
            params![request.meeting_id, request.speaker_id],
        )?;
        transaction.execute(
            "UPDATE meeting_speakers
             SET profile_id=?1,
                 display_name=(SELECT display_name FROM voice_profiles WHERE id=?1),
                 match_state='matched',needs_review=0
             WHERE id=?2",
            params![request.profile_id, request.speaker_id],
        )?;
        let updated = transaction.execute(
            "UPDATE voice_profiles SET updated_at_ms=?1,last_used_at_ms=?1 WHERE id=?2",
            params![now, request.profile_id],
        )?;
        if updated == 0 {
            return Err(CoreError::NotFound(format!(
                "voice profile {}",
                request.profile_id
            )));
        }
        transaction.commit()?;
        self.list_voice_profiles()?
            .into_iter()
            .find(|profile| profile.id == request.profile_id)
            .ok_or_else(|| CoreError::NotFound("confirmed voice profile".into()))
    }

    pub fn list_jobs(&self, meeting_id: Option<&str>) -> CoreResult<Vec<ProcessingJob>> {
        let connection = self.database.connect()?;
        let mut statement = connection.prepare(
            "SELECT id,meeting_id,stage,status,progress,attempts,max_attempts,checkpoint_ms,
                    error_code,error_message,created_at_ms,updated_at_ms
             FROM processing_jobs
             WHERE (?1 IS NULL OR meeting_id=?1)
             ORDER BY created_at_ms DESC",
        )?;
        let jobs = statement
            .query_map([meeting_id], map_job)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(jobs)
    }

    pub fn get_job(&self, job_id: &str) -> CoreResult<ProcessingJob> {
        let connection = self.database.connect()?;
        connection
            .query_row(
                "SELECT id,meeting_id,stage,status,progress,attempts,max_attempts,checkpoint_ms,
                        error_code,error_message,created_at_ms,updated_at_ms
                 FROM processing_jobs WHERE id=?1",
                [job_id],
                map_job,
            )
            .optional()?
            .ok_or_else(|| CoreError::NotFound(format!("processing job {job_id}")))
    }

    pub fn cancel_job(&self, job_id: &str) -> CoreResult<ProcessingJob> {
        let connection = self.database.connect()?;
        let changed = connection.execute(
            "UPDATE processing_jobs
             SET status=CASE WHEN status='running' THEN 'cancel_requested' ELSE 'cancelled' END,
                 locked_at_ms=CASE WHEN status='running' THEN locked_at_ms ELSE NULL END,
                 worker_id=CASE WHEN status='running' THEN worker_id ELSE NULL END,
                 updated_at_ms=?1
             WHERE id=?2 AND status IN ('queued','running','retry_wait','interrupted')",
            params![now_ms(), job_id],
        )?;
        if changed == 0 {
            return Err(CoreError::Conflict(
                "only queued or running jobs can be cancelled".into(),
            ));
        }
        let job = self.get_job(job_id)?;
        if job.status == "cancelled" {
            connection.execute(
                "UPDATE meetings SET status='failed'
                 WHERE id=?1 AND status='processing'",
                [&job.meeting_id],
            )?;
        }
        self.notify_processing_queue();
        Ok(job)
    }

    pub fn retry_job(&self, job_id: &str) -> CoreResult<ProcessingJob> {
        let connection = self.database.connect()?;
        let changed = connection.execute(
            "UPDATE processing_jobs
             SET status='queued',progress=0,attempts=0,error_code=NULL,error_message=NULL,
                 locked_at_ms=NULL,worker_id=NULL,updated_at_ms=?1
             WHERE id=?2 AND status IN ('failed','cancelled','interrupted','retry_wait')",
            params![now_ms(), job_id],
        )?;
        if changed == 0 {
            return Err(CoreError::Conflict(
                "only failed or cancelled jobs can be retried".into(),
            ));
        }
        self.notify_processing_queue();
        self.get_job(job_id)
    }

    pub fn enqueue_final_pipeline(&self, meeting_id: &str) -> CoreResult<ProcessingJob> {
        let id = new_id();
        let now = now_ms();
        let connection = self.database.connect()?;
        connection.execute(
            "INSERT INTO processing_jobs(
                id,meeting_id,stage,status,progress,input_json,created_at_ms,updated_at_ms
             ) VALUES (?1,?2,'normalize','queued',0,?3,?4,?4)",
            params![
                id,
                meeting_id,
                json!({
                    "meetingId": meeting_id,
                    "pipelineVersion": crate::worker::PIPELINE_VERSION
                })
                .to_string(),
                now
            ],
        )?;
        connection.execute(
            "UPDATE meetings SET status='processing' WHERE id=?1",
            [meeting_id],
        )?;
        self.notify_processing_queue();
        self.get_job(&id)
    }

    pub fn export_transcript(
        &self,
        meeting_id: &str,
        format: ExportFormat,
    ) -> CoreResult<(ExportResult, Vec<u8>)> {
        let detail = self.get_meeting(meeting_id)?;
        let bytes = render_export(&detail, &format)?;
        let extension = export_extension(&format);
        let file_name = format!(
            "{}-{}.{}",
            media::sanitize_file_stem(&detail.meeting.title),
            &meeting_id[..8],
            extension
        );
        let artifact_id = new_id();
        let destination = self.layout.exports().join(&artifact_id).join(&file_name);
        let (size_bytes, sha256) = media::atomic_write(&destination, &bytes)?;
        let relative = self.layout.relative_to_root(&destination)?;
        let now = now_ms();
        let connection = self.database.connect()?;
        connection.execute(
            "INSERT INTO artifacts(
                id,meeting_id,kind,format,relative_path,size_bytes,sha256,created_at_ms
             ) VALUES (?1,?2,'transcript',?3,?4,?5,?6,?7)",
            params![
                artifact_id,
                meeting_id,
                extension,
                relative,
                size_bytes as i64,
                sha256,
                now
            ],
        )?;
        Ok((
            ExportResult {
                artifact_id,
                file_name,
                format,
                size_bytes,
                created_at_ms: now,
            },
            bytes,
        ))
    }

    pub fn backup_library(&self, include_media: bool) -> CoreResult<BackupResult> {
        let backup_id = new_id();
        let created_at_ms = now_ms();
        let backup_directory = self.layout.backups().join(&backup_id);
        fs::create_dir_all(&backup_directory)?;
        let database_path = backup_directory.join("local-transcript.sqlite3");
        {
            let source = self.database.connect()?;
            source.execute_batch("PRAGMA wal_checkpoint(PASSIVE);")?;
            let mut destination = Connection::open(&database_path)?;
            let backup = Backup::new(&source, &mut destination)?;
            backup.run_to_completion(32, Duration::from_millis(20), None)?;
        }

        let connection = self.database.connect()?;
        let mut statement = connection
            .prepare("SELECT id,relative_path,size_bytes,sha256 FROM media_assets ORDER BY id")?;
        let asset_rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)? as u64,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let manifest = json!({
            "schemaVersion": 1,
            "backupId": backup_id,
            "createdAt": iso_from_ms(created_at_ms),
            "includesMedia": include_media,
            "assets": asset_rows.iter().map(|(id, path, size, sha)| json!({
                "id": id, "relativePath": path, "sizeBytes": size, "sha256": sha
            })).collect::<Vec<_>>()
        });
        media::atomic_write(
            &backup_directory.join("manifest.json"),
            &serde_json::to_vec_pretty(&manifest)?,
        )?;

        let mut file_count = 2_u64;
        let mut total_bytes = fs::metadata(&database_path)?.len()
            + fs::metadata(backup_directory.join("manifest.json"))?.len();
        if include_media {
            for (_, relative, _, _) in &asset_rows {
                let source = self.layout.resolve_relative(relative)?;
                let destination = backup_directory.join("files").join(relative);
                let (size, _) = media::atomic_copy_with_hash(&source, &destination)?;
                file_count += 1;
                total_bytes = total_bytes.saturating_add(size);
            }
        }
        Ok(BackupResult {
            backup_id,
            file_count,
            total_bytes,
            includes_media: include_media,
            created_at_ms,
        })
    }

    pub fn asset_descriptor(&self, asset_id: &str) -> CoreResult<AssetDescriptor> {
        let asset = self.get_asset(asset_id)?;
        Ok(AssetDescriptor {
            id: asset.id,
            display_name: asset.display_name,
            content_type: asset.content_type,
            size_bytes: asset.size_bytes,
            duration_ms: asset.duration_ms,
            url: None,
        })
    }

    pub(crate) fn asset_stream_info(
        &self,
        asset_id: &str,
    ) -> CoreResult<(PathBuf, AssetDescriptor)> {
        validate_id(asset_id, "asset")?;
        let connection = self.database.connect()?;
        let row: Option<(String, String, String, i64, Option<i64>)> = connection
            .query_row(
                "SELECT relative_path,display_name,content_type,size_bytes,duration_ms
                 FROM media_assets WHERE id=?1
                 UNION ALL
                 SELECT relative_path,relative_path,
                        CASE format
                            WHEN 'txt' THEN 'text/plain'
                            WHEN 'md' THEN 'text/markdown'
                            WHEN 'srt' THEN 'application/x-subrip'
                            WHEN 'vtt' THEN 'text/vtt'
                            WHEN 'json' THEN 'application/json'
                            ELSE 'application/octet-stream'
                        END,
                        size_bytes,NULL
                 FROM artifacts WHERE id=?1
                 LIMIT 1",
                [asset_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let (relative, display_name, content_type, size_bytes, duration_ms) =
            row.ok_or_else(|| CoreError::NotFound(format!("asset {asset_id}")))?;
        let path = self.layout.resolve_relative(&relative)?;
        Ok((
            path,
            AssetDescriptor {
                id: asset_id.into(),
                display_name,
                content_type,
                size_bytes: size_bytes.max(0) as u64,
                duration_ms,
                url: None,
            },
        ))
    }

    pub fn read_asset_chunk(&self, request: AssetChunkRequest) -> CoreResult<AssetChunk> {
        if request.length == 0 || request.length > MAX_ASSET_CHUNK {
            return Err(CoreError::InvalidInput(format!(
                "asset chunk length must be between 1 and {MAX_ASSET_CHUNK} bytes"
            )));
        }
        let connection = self.database.connect()?;
        let (relative, size): (String, i64) = connection
            .query_row(
                "SELECT relative_path,size_bytes FROM media_assets WHERE id=?1
                 UNION ALL
                 SELECT relative_path,size_bytes FROM artifacts WHERE id=?1
                 LIMIT 1",
                [&request.asset_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| CoreError::NotFound(format!("asset {}", request.asset_id)))?;
        let size = size.max(0) as u64;
        if request.offset > size {
            return Err(CoreError::InvalidInput(
                "asset chunk offset is beyond end of file".into(),
            ));
        }
        let path = self.layout.resolve_relative(&relative)?;
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(request.offset))?;
        let desired = (request.length as u64).min(size.saturating_sub(request.offset)) as usize;
        let mut bytes = vec![0_u8; desired];
        file.read_exact(&mut bytes)?;
        let end = request.offset.saturating_add(bytes.len() as u64);
        Ok(AssetChunk {
            asset_id: request.asset_id,
            offset: request.offset,
            bytes,
            end_of_file: end >= size,
        })
    }

    pub fn model_status(&self) -> ModelPackStatus {
        let worker_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("worker");
        let packaged_runtime_ready = self.layout.runtime_validated();
        let development_runtime_ready =
            cfg!(any(debug_assertions, feature = "development-runtime"))
                && development_python(&worker_root).is_file()
                && worker_root
                    .join("src")
                    .join("local_transcript_worker")
                    .join("__main__.py")
                    .is_file()
                && media_tools::resolve(&self.layout, MediaTool::Ffmpeg).is_ok()
                && media_tools::resolve(&self.layout, MediaTool::Ffprobe).is_ok();
        let runtime_ready = packaged_runtime_ready || development_runtime_ready;
        let manifest = bundled_model_manifest();
        let model_ready = |key: &str| {
            manifest
                .as_ref()
                .and_then(|manifest| manifest.models.iter().find(|model| model.key == key))
                .map(|model| {
                    let root = self.layout.models().join(&model.key).join(&model.revision);
                    model.files.iter().all(|file| {
                        fs::metadata(root.join(&file.path))
                            .map(|metadata| metadata.is_file() && metadata.len() == file.size)
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        };
        let live_ready = model_ready("live_asr_en");
        let final_ready = model_ready("final_asr_en")
            && (cfg!(all(target_os = "macos", target_arch = "aarch64"))
                || model_ready("alignment_en"));
        let diarization_ready = model_ready("diarization") && model_ready("speaker_embedding");
        let runtime_status = if runtime_ready {
            "ready"
        } else if self.layout.runtime_validation_pending() {
            "checking"
        } else {
            "missing"
        };
        ModelPackStatus {
            runtime: runtime_status.into(),
            live_model: ready_or_missing(live_ready),
            final_model: ready_or_missing(final_ready),
            diarization_model: ready_or_missing(diarization_ready),
            device: if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
                "Apple MLX (Metal) with PyTorch MPS/CPU speaker processing".into()
            } else {
                "Automatic (NVIDIA CUDA when available; CPU fallback)".into()
            },
            disk_required_gb: if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
                2.0
            } else {
                13.5
            },
            disk_available_gb: available_disk_gb(self.layout.root()).unwrap_or(0.0),
        }
    }

    pub fn model_revisions(&self) -> BTreeMap<String, String> {
        let mut revisions = BTreeMap::new();
        if let Some(manifest) = bundled_model_manifest() {
            revisions.insert("pipeline".into(), manifest.pipeline_version);
            for model in manifest.models {
                revisions.insert(model.key, model.revision);
            }
        }
        revisions
    }

    pub fn mark_setup_complete(&self) -> CoreResult<()> {
        let connection = self.database.connect()?;
        connection.execute(
            "INSERT INTO app_meta(key,value,updated_at_ms)
             VALUES ('setup_complete','true',?1)
             ON CONFLICT(key) DO UPDATE SET value='true',updated_at_ms=excluded.updated_at_ms",
            [now_ms()],
        )?;
        Ok(())
    }

    pub fn get_asset(&self, asset_id: &str) -> CoreResult<MediaAsset> {
        let connection = self.database.connect()?;
        connection
            .query_row(
                "SELECT id,meeting_id,kind,display_name,content_type,size_bytes,sha256,
                        duration_ms,codec,sample_rate_hz,channels,created_at_ms
                 FROM media_assets WHERE id=?1",
                [asset_id],
                map_asset,
            )
            .optional()?
            .ok_or_else(|| CoreError::NotFound(format!("media asset {asset_id}")))
    }

    pub fn get_turn(&self, turn_id: &str) -> CoreResult<TranscriptTurn> {
        let connection = self.database.connect()?;
        let mut turn = connection
            .query_row(
                "SELECT id,meeting_id,speaker_id,start_ms,end_ms,model_text,edited_text,
                        revision,needs_review,is_draft,is_marked
                 FROM transcript_turns WHERE id=?1",
                [turn_id],
                map_turn_without_words,
            )
            .optional()?
            .ok_or_else(|| CoreError::NotFound(format!("transcript turn {turn_id}")))?;
        let mut statement = connection.prepare(
            "SELECT id,turn_id,start_ms,end_ms,text,confidence,speaker_id,is_overlap
             FROM words WHERE turn_id=?1 ORDER BY sequence",
        )?;
        turn.words = statement
            .query_map([turn_id], map_word)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(turn)
    }

    fn repair_recording_partials(&self, recovery_scope: &StartupRecoveryScope) -> CoreResult<()> {
        for path in files_with_suffix(self.layout.recordings(), ".wav.partial")? {
            let belongs_to_startup_session = path.components().any(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .is_some_and(|value| recovery_scope.recording_session_ids.contains(value))
            });
            if !belongs_to_startup_session
                || !file_modified_before(&path, recovery_scope.captured_at_ms)
            {
                continue;
            }
            match repair_wav_header(&path) {
                Ok(()) => {
                    let final_path = PathBuf::from(
                        path.to_string_lossy()
                            .strip_suffix(".partial")
                            .unwrap_or_default(),
                    );
                    if let Err(error) = fs::rename(&path, final_path) {
                        log::warn!("could not finalize repaired recording segment: {error}");
                    }
                }
                Err(error) => log::warn!("could not repair recording segment {:?}: {error}", path),
            }
        }
        Ok(())
    }

    fn remove_abandoned_import_partials(&self, captured_at_ms: i64) -> CoreResult<()> {
        for path in files_with_suffix(self.layout.media(), ".partial")? {
            if !file_modified_before(&path, captured_at_ms) {
                continue;
            }
            if let Err(error) = fs::remove_file(&path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    log::warn!("could not remove abandoned import {:?}: {error}", path);
                }
            }
        }
        Ok(())
    }
}

fn meeting_select_sql() -> &'static str {
    "SELECT m.id,m.title,m.source_kind,m.status,m.created_at_ms,m.started_at_ms,m.ended_at_ms,
            COALESCE(m.duration_ms,0),m.language,m.has_user_edits,
            (SELECT count(*) FROM meeting_speakers s WHERE s.meeting_id=m.id),
            (SELECT count(*) FROM media_assets a WHERE a.meeting_id=m.id),
            (SELECT a.id FROM media_assets a WHERE a.meeting_id=m.id
             ORDER BY CASE WHEN a.kind='playback' THEN 0 ELSE 1 END,a.created_at_ms LIMIT 1),
            m.needs_review,m.recovery_warning
     FROM meetings m"
}

fn map_meeting(row: &Row<'_>) -> rusqlite::Result<Meeting> {
    let source_kind: String = row.get(2)?;
    let created_at_ms: i64 = row.get(4)?;
    Ok(Meeting {
        id: row.get(0)?,
        title: row.get(1)?,
        created_at: iso_from_ms(created_at_ms),
        source_type: source_kind.clone(),
        source_kind,
        status: status_from_db(row.get::<_, String>(3)?),
        created_at_ms,
        started_at_ms: row.get(5)?,
        ended_at_ms: row.get(6)?,
        duration_ms: row.get(7)?,
        language: row.get(8)?,
        has_user_edits: row.get::<_, i64>(9)? != 0,
        speaker_count: row.get::<_, i64>(10)? as u32,
        asset_count: row.get::<_, i64>(11)? as u32,
        asset_id: row.get(12)?,
        needs_review: row.get::<_, i64>(13)? != 0,
        recovery_warning: row.get(14)?,
    })
}

fn map_asset(row: &Row<'_>) -> rusqlite::Result<MediaAsset> {
    Ok(MediaAsset {
        id: row.get(0)?,
        meeting_id: row.get(1)?,
        kind: row.get(2)?,
        display_name: row.get(3)?,
        content_type: row.get(4)?,
        size_bytes: row.get::<_, i64>(5)?.max(0) as u64,
        sha256: row.get(6)?,
        duration_ms: row.get(7)?,
        codec: row.get(8)?,
        sample_rate_hz: row.get(9)?,
        channels: row.get(10)?,
        created_at_ms: row.get(11)?,
    })
}

fn map_speaker(row: &Row<'_>) -> rusqlite::Result<MeetingSpeaker> {
    let id: String = row.get(0)?;
    let label: String = row.get(2)?;
    let stored_name: String = row.get(3)?;
    let state = match row.get::<_, String>(5)?.as_str() {
        "matched" => SpeakerMatchState::Matched,
        "review" => SpeakerMatchState::Review,
        _ => SpeakerMatchState::Unknown,
    };
    let name = if matches!(
        state,
        SpeakerMatchState::Unknown | SpeakerMatchState::Review
    ) && matches!(stored_name.trim(), "Unknown" | "Unknown speaker")
    {
        default_unknown_speaker_name(&label)
    } else {
        stored_name
    };
    Ok(MeetingSpeaker {
        id: id.clone(),
        meeting_id: row.get(1)?,
        label,
        display_name: name.clone(),
        initials: initials(&name),
        profile_id: row.get(4)?,
        state: state.clone(),
        match_state: state,
        needs_review: row.get::<_, i64>(6)? != 0,
        color: Some(
            row.get::<_, Option<String>>(7)?
                .unwrap_or_else(|| color_for(&id)),
        ),
    })
}

fn normalize_unknown_speaker_names(speakers: &mut [MeetingSpeaker]) {
    let anonymous = speakers
        .iter()
        .map(|speaker| {
            matches!(
                speaker.state,
                SpeakerMatchState::Unknown | SpeakerMatchState::Review
            ) && (matches!(speaker.display_name.trim(), "Unknown" | "Unknown speaker")
                || numbered_unknown_name(&speaker.display_name).is_some())
        })
        .collect::<Vec<_>>();
    let mut used = speakers
        .iter()
        .zip(&anonymous)
        .filter_map(|(speaker, anonymous)| {
            (!anonymous).then(|| numbered_unknown_name(&speaker.display_name))?
        })
        .collect::<BTreeSet<_>>();
    let mut assignments = vec![None; speakers.len()];

    for (index, speaker) in speakers.iter().enumerate() {
        if !anonymous[index] {
            continue;
        }
        if let Some(preferred) = number_from_cluster_label(&speaker.label) {
            if used.insert(preferred) {
                assignments[index] = Some(preferred);
            }
        }
    }
    for (index, speaker) in speakers.iter().enumerate() {
        if !anonymous[index] || assignments[index].is_some() {
            continue;
        }
        if let Some(current) = numbered_unknown_name(&speaker.display_name) {
            if used.insert(current) {
                assignments[index] = Some(current);
            }
        }
    }

    let mut fallback = 1;
    for (index, speaker) in speakers.iter_mut().enumerate() {
        if !anonymous[index] {
            continue;
        }
        let number = assignments[index].unwrap_or_else(|| {
            while used.contains(&fallback) {
                fallback += 1;
            }
            let number = fallback;
            used.insert(number);
            number
        });
        speaker.display_name = format!("Speaker {number}");
        speaker.initials = format!("S{number}");
    }
}

fn numbered_unknown_name(display_name: &str) -> Option<u32> {
    display_name
        .trim()
        .strip_prefix("Speaker ")?
        .parse::<u32>()
        .ok()
        .filter(|number| *number > 0)
}

fn map_chat_message(row: &Row<'_>) -> rusqlite::Result<TranscriptChatMessage> {
    let citations_json: String = row.get(4)?;
    let citations = serde_json::from_str(&citations_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(TranscriptChatMessage {
        id: row.get(0)?,
        meeting_id: row.get(1)?,
        role: row.get(2)?,
        content: row.get(3)?,
        citations,
        model: row.get(5)?,
        created_at_ms: row.get(6)?,
    })
}

fn map_turn_without_words(row: &Row<'_>) -> rusqlite::Result<TranscriptTurn> {
    let model_text: String = row.get(5)?;
    let edited_text: Option<String> = row.get(6)?;
    Ok(TranscriptTurn {
        id: row.get(0)?,
        meeting_id: row.get(1)?,
        speaker_id: row.get(2)?,
        start_ms: row.get(3)?,
        end_ms: row.get(4)?,
        text: edited_text.clone().unwrap_or_else(|| model_text.clone()),
        model_text,
        edited_text,
        revision: row.get(7)?,
        needs_review: row.get::<_, i64>(8)? != 0,
        is_draft: row.get::<_, i64>(9)? != 0,
        is_marked: row.get::<_, i64>(10)? != 0,
        words: Vec::new(),
    })
}

fn map_word(row: &Row<'_>) -> rusqlite::Result<WordTiming> {
    Ok(WordTiming {
        id: row.get(0)?,
        turn_id: row.get(1)?,
        start_ms: row.get(2)?,
        end_ms: row.get(3)?,
        text: row.get(4)?,
        confidence: row.get(5)?,
        speaker_id: row.get(6)?,
        is_overlap: row.get::<_, i64>(7)? != 0,
    })
}

fn map_job(row: &Row<'_>) -> rusqlite::Result<ProcessingJob> {
    let status: String = row.get(3)?;
    let state = match status.as_str() {
        "retry_wait" | "interrupted" => "queued",
        "cancel_requested" => "running",
        value => value,
    }
    .to_string();
    Ok(ProcessingJob {
        id: row.get(0)?,
        meeting_id: row.get(1)?,
        stage: row.get(2)?,
        status,
        state,
        progress: row.get(4)?,
        attempts: row.get::<_, i64>(5)? as u32,
        max_attempts: row.get::<_, i64>(6)? as u32,
        checkpoint_ms: row.get(7)?,
        error_code: row.get(8)?,
        error_message: row.get(9)?,
        created_at_ms: row.get(10)?,
        updated_at_ms: row.get(11)?,
    })
}

fn render_export(detail: &MeetingDetail, format: &ExportFormat) -> CoreResult<Vec<u8>> {
    let speaker_names = detail
        .speakers
        .iter()
        .map(|speaker| (speaker.id.as_str(), speaker.display_name.as_str()))
        .collect::<BTreeMap<_, _>>();
    let speaker_for = |turn: &TranscriptTurn| {
        turn.speaker_id
            .as_deref()
            .and_then(|id| speaker_names.get(id).copied())
            .unwrap_or("Unknown")
    };
    let content = match format {
        ExportFormat::Txt => detail
            .turns
            .iter()
            .map(|turn| format!("{}: {}", speaker_for(turn), turn.text))
            .collect::<Vec<_>>()
            .join("\r\n\r\n"),
        ExportFormat::Markdown => {
            let mut output = format!(
                "# {}\n\n_Date: {}_\n\n",
                detail.meeting.title, detail.meeting.created_at
            );
            for turn in &detail.turns {
                output.push_str(&format!(
                    "**{}** `{}`\n\n{}\n\n",
                    speaker_for(turn),
                    format_clock(turn.start_ms),
                    turn.text
                ));
            }
            output
        }
        ExportFormat::Srt => detail
            .turns
            .iter()
            .enumerate()
            .map(|(index, turn)| {
                format!(
                    "{}\r\n{} --> {}\r\n{}: {}\r\n",
                    index + 1,
                    format_srt_time(turn.start_ms),
                    format_srt_time(turn.end_ms),
                    speaker_for(turn),
                    turn.text
                )
            })
            .collect::<Vec<_>>()
            .join("\r\n"),
        ExportFormat::WebVtt => {
            let body = detail
                .turns
                .iter()
                .map(|turn| {
                    format!(
                        "{} --> {}\r\n<v {}>{}\r\n",
                        format_vtt_time(turn.start_ms),
                        format_vtt_time(turn.end_ms),
                        speaker_for(turn),
                        turn.text
                    )
                })
                .collect::<Vec<_>>()
                .join("\r\n");
            format!("WEBVTT\r\n\r\n{body}")
        }
        ExportFormat::Json => serde_json::to_string_pretty(&json!({
            "schemaVersion": 1,
            "exportedAt": iso_from_ms(now_ms()),
            "meeting": detail.meeting,
            "speakers": detail.speakers,
            "turns": detail.turns,
            "markers": detail.markers,
        }))?,
    };
    Ok(content.into_bytes())
}

fn export_extension(format: &ExportFormat) -> &'static str {
    match format {
        ExportFormat::Txt => "txt",
        ExportFormat::Markdown => "md",
        ExportFormat::Srt => "srt",
        ExportFormat::WebVtt => "vtt",
        ExportFormat::Json => "json",
    }
}

fn format_clock(milliseconds: i64) -> String {
    let total_seconds = milliseconds.max(0) / 1000;
    format!(
        "{:02}:{:02}:{:02}",
        total_seconds / 3600,
        (total_seconds / 60) % 60,
        total_seconds % 60
    )
}

fn format_srt_time(milliseconds: i64) -> String {
    let milliseconds = milliseconds.max(0);
    let total_seconds = milliseconds / 1000;
    format!(
        "{:02}:{:02}:{:02},{:03}",
        total_seconds / 3600,
        (total_seconds / 60) % 60,
        total_seconds % 60,
        milliseconds % 1000
    )
}

fn format_vtt_time(milliseconds: i64) -> String {
    format_srt_time(milliseconds).replace(',', ".")
}

fn fts_query(value: &str) -> CoreResult<String> {
    let tokens = value
        .split_whitespace()
        .map(|token| {
            token
                .chars()
                .filter(|character| character.is_alphanumeric() || *character == '\'')
                .collect::<String>()
        })
        .filter(|token| !token.is_empty())
        .map(|token| format!("\"{}\"*", token.replace('"', "\"\"")))
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return Err(CoreError::InvalidInput(
            "transcript search requires at least one letter or number".into(),
        ));
    }
    Ok(tokens.join(" AND "))
}

fn status_to_db(status: MeetingStatus) -> &'static str {
    match status {
        MeetingStatus::Importing => "importing",
        MeetingStatus::Recording => "recording",
        MeetingStatus::Processing => "processing",
        MeetingStatus::Ready => "ready",
        MeetingStatus::Failed => "failed",
    }
}

fn status_from_db(value: String) -> MeetingStatus {
    match value.as_str() {
        "importing" => MeetingStatus::Importing,
        "recording" => MeetingStatus::Recording,
        "processing" => MeetingStatus::Processing,
        "ready" => MeetingStatus::Ready,
        _ => MeetingStatus::Failed,
    }
}

fn new_id() -> String {
    Uuid::now_v7().to_string()
}

fn validate_id(value: &str, label: &str) -> CoreResult<()> {
    Uuid::parse_str(value)
        .map(|_| ())
        .map_err(|_| CoreError::InvalidInput(format!("{label} id is invalid")))
}

fn nonempty_name(value: String, label: &str) -> CoreResult<String> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > 120 {
        return Err(CoreError::InvalidInput(format!(
            "{label} must be between 1 and 120 characters"
        )));
    }
    Ok(value.into())
}

fn default_unknown_speaker_name(cluster_label: &str) -> String {
    format!(
        "Speaker {}",
        number_from_cluster_label(cluster_label).unwrap_or(1)
    )
}

fn number_from_cluster_label(cluster_label: &str) -> Option<u32> {
    let trailing_digits = cluster_label
        .chars()
        .rev()
        .take_while(|character| character.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    trailing_digits
        .parse::<u32>()
        .ok()
        .and_then(|value| value.checked_add(1))
}

fn initials(name: &str) -> String {
    let result = name
        .split_whitespace()
        .filter_map(|word| word.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase();
    if result.is_empty() {
        "?".into()
    } else {
        result
    }
}

fn color_for(id: &str) -> String {
    let index = id
        .bytes()
        .fold(0_usize, |value, byte| value.wrapping_add(byte as usize))
        % DEFAULT_PROFILE_COLORS.len();
    DEFAULT_PROFILE_COLORS[index].into()
}

fn ready_or_missing(ready: bool) -> String {
    if ready { "ready" } else { "missing" }.into()
}

fn bundled_model_manifest() -> Option<BundledModelManifest> {
    serde_json::from_str(MODEL_MANIFEST_JSON).ok()
}

fn development_python(worker_root: &Path) -> PathBuf {
    if cfg!(windows) {
        worker_root.join(".venv").join("Scripts").join("python.exe")
    } else {
        worker_root.join(".venv").join("bin").join("python")
    }
}

fn files_with_suffix(root: &Path, suffix: &str) -> CoreResult<Vec<PathBuf>> {
    fn visit(directory: &Path, suffix: &str, output: &mut Vec<PathBuf>) -> CoreResult<()> {
        if !directory.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                visit(&entry.path(), suffix, output)?;
            } else if kind.is_file() && entry.file_name().to_string_lossy().ends_with(suffix) {
                output.push(entry.path());
            }
        }
        Ok(())
    }
    let mut output = Vec::new();
    visit(root, suffix, &mut output)?;
    Ok(output)
}

fn file_modified_before(path: &Path, captured_at_ms: i64) -> bool {
    let Ok(cutoff_ms) = u128::try_from(captured_at_ms) else {
        return false;
    };
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .is_some_and(|age| age.as_millis() < cutoff_ms)
}

fn repair_wav_header(path: &Path) -> CoreResult<()> {
    let mut file = fs::OpenOptions::new().read(true).write(true).open(path)?;
    let length = file.metadata()?.len();
    if length < 44 {
        return Err(CoreError::Media("recording segment is too short".into()));
    }
    let riff_size = (length - 8).min(u32::MAX as u64) as u32;
    let data_size = (length - 44).min(u32::MAX as u64) as u32;
    file.seek(SeekFrom::Start(4))?;
    file.write_all(&riff_size.to_le_bytes())?;
    file.seek(SeekFrom::Start(40))?;
    file.write_all(&data_size.to_le_bytes())?;
    file.sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn available_disk_gb(path: &Path) -> Option<f32> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            directory_name: *const u16,
            free_bytes_available: *mut u64,
            total_number_of_bytes: *mut u64,
            total_number_of_free_bytes: *mut u64,
        ) -> i32;
    }
    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut available = 0_u64;
    let result = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (result != 0).then_some(available as f32 / 1024.0 / 1024.0 / 1024.0)
}

#[cfg(unix)]
fn available_disk_gb(path: &Path) -> Option<f32> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let canonical = path.canonicalize().ok()?;
    let raw = CString::new(canonical.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let result = unsafe { libc::statvfs(raw.as_ptr(), stats.as_mut_ptr()) };
    if result != 0 {
        return None;
    }
    let stats = unsafe { stats.assume_init() };
    Some((stats.f_bavail as f64 * stats.f_frsize as f64 / 1024.0 / 1024.0 / 1024.0) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc};

    fn service() -> (tempfile::TempDir, CoreService) {
        let temp = tempfile::tempdir().unwrap();
        let service = CoreService::open(temp.path()).unwrap();
        (temp, service)
    }

    #[test]
    fn transcript_chat_round_trip_preserves_grounded_citations() {
        let (_temp, service) = service();
        let meeting_id = new_id();
        let now = now_ms();
        service
            .database
            .connect()
            .unwrap()
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Agent chat','import','ready',?2)",
                params![meeting_id, now],
            )
            .unwrap();

        service
            .save_transcript_chat_message(
                &meeting_id,
                "user",
                "What was decided?".into(),
                Vec::new(),
                Some("local-model".into()),
            )
            .unwrap();
        let citation = TranscriptCitation {
            turn_id: "turn-1".into(),
            start_ms: 3_000,
            end_ms: 4_000,
            speaker_name: "Maya".into(),
            snippet: "Ship on Friday.".into(),
        };
        service
            .save_transcript_chat_message(
                &meeting_id,
                "assistant",
                "The team decided to ship on Friday.".into(),
                vec![citation.clone()],
                Some("local-model".into()),
            )
            .unwrap();

        let messages = service.list_transcript_chat(&meeting_id, 100).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].citations, vec![citation]);
        assert_eq!(messages[1].model.as_deref(), Some("local-model"));

        service.clear_transcript_chat(&meeting_id).unwrap();
        assert!(service
            .list_transcript_chat(&meeting_id, 100)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn library_stats_uses_actual_managed_files_including_active_work() {
        let (_temp, service) = service();
        let meeting_id = new_id();
        let asset_id = new_id();
        let now = now_ms();
        let media = service.layout.media().join("source.bin");
        fs::write(&media, vec![0_u8; 7]).unwrap();
        let relative = service.layout.relative_to_root(&media).unwrap();
        let connection = service.database.connect().unwrap();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Storage','import','ready',?2)",
                params![meeting_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO media_assets(
                    id,meeting_id,kind,display_name,relative_path,content_type,size_bytes,
                    sha256,created_at_ms
                 ) VALUES (?1,?2,'original','source.bin',?3,'application/octet-stream',
                           999,'hash',?4)",
                params![asset_id, meeting_id, relative, now],
            )
            .unwrap();
        drop(connection);
        fs::write(
            service.layout.recordings().join("capture.bin"),
            vec![0_u8; 11],
        )
        .unwrap();
        fs::write(
            service.layout.artifacts().join("model.json"),
            vec![0_u8; 13],
        )
        .unwrap();
        fs::write(service.layout.work().join("checkpoint.bin"), vec![0_u8; 17]).unwrap();

        let stats = service.library_stats().unwrap();

        assert_eq!(stats.storage_bytes, 48);
        assert_ne!(stats.storage_bytes, 999);
    }

    #[test]
    fn library_size_cache_is_reused_and_explicitly_invalidated() {
        let (_temp, service) = service();
        fs::write(service.layout.media().join("first.bin"), vec![0_u8; 5]).unwrap();
        assert_eq!(service.library_stats().unwrap().storage_bytes, 5);

        fs::write(service.layout.media().join("second.bin"), vec![0_u8; 7]).unwrap();
        assert_eq!(service.library_stats().unwrap().storage_bytes, 5);

        service.invalidate_library_size_cache();
        assert_eq!(service.library_stats().unwrap().storage_bytes, 12);
    }

    #[test]
    fn processing_queue_notification_wakes_an_idle_waiter() {
        let temp = tempfile::tempdir().unwrap();
        let service = Arc::new(CoreService::open(temp.path()).unwrap());
        let observed = service.processing_queue_generation();
        let (ready_sender, ready_receiver) = mpsc::channel();
        let waiting_service = service.clone();
        let waiter = std::thread::spawn(move || {
            ready_sender.send(()).unwrap();
            waiting_service.wait_for_processing_queue_change(observed, Duration::from_secs(2))
        });
        ready_receiver.recv().unwrap();

        service.notify_processing_queue();

        assert!(waiter.join().unwrap());
    }

    #[test]
    fn background_recovery_never_touches_work_created_after_startup_snapshot() {
        let (_temp, service) = service();
        let recovery_scope = service.capture_startup_recovery_scope().unwrap();
        let meeting_id = new_id();
        let session_id = new_id();
        let now = now_ms();
        let recording_partial = service
            .layout
            .recordings()
            .join(&meeting_id)
            .join(&session_id)
            .join("segment.wav.partial");
        fs::create_dir_all(recording_partial.parent().unwrap()).unwrap();
        fs::write(&recording_partial, b"active recording bytes").unwrap();
        let import_partial = service.layout.media().join("active-import.partial");
        fs::write(&import_partial, b"active import bytes").unwrap();
        let connection = service.database.connect().unwrap();
        connection
            .execute(
                "INSERT INTO meetings(
                    id,title,source_kind,status,created_at_ms,started_at_ms
                 ) VALUES (?1,'New recording','recording','recording',?2,?2)",
                params![meeting_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO recording_sessions(
                    id,meeting_id,state,config_json,manifest_relative_path,started_at_ms
                 ) VALUES (?1,?2,'recording','{}','manifest.jsonl',?3)",
                params![session_id, meeting_id, now],
            )
            .unwrap();
        drop(connection);

        service
            .recover_recordings_and_cleanup(recovery_scope)
            .unwrap();

        let connection = service.database.connect().unwrap();
        let session_state: String = connection
            .query_row(
                "SELECT state FROM recording_sessions WHERE id=?1",
                [&session_id],
                |row| row.get(0),
            )
            .unwrap();
        let meeting_status: String = connection
            .query_row(
                "SELECT status FROM meetings WHERE id=?1",
                [&meeting_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(session_state, "recording");
        assert_eq!(meeting_status, "recording");
        assert!(recording_partial.is_file());
        assert!(import_partial.is_file());
    }

    #[test]
    fn meeting_detail_batches_words_and_preserves_per_turn_sequence() {
        let (_temp, service) = service();
        let meeting_id = new_id();
        let first_turn = new_id();
        let second_turn = new_id();
        let now = now_ms();
        let connection = service.database.connect().unwrap();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Batched words','import','ready',?2)",
                params![meeting_id, now],
            )
            .unwrap();
        for (turn_id, start_ms, text) in [
            (&first_turn, 0_i64, "hello world"),
            (&second_turn, 1_000_i64, "goodbye"),
        ] {
            connection
                .execute(
                    "INSERT INTO transcript_turns(
                        id,meeting_id,start_ms,end_ms,model_text,created_at_ms,updated_at_ms
                     ) VALUES (?1,?2,?3,?4,?5,?6,?6)",
                    params![turn_id, meeting_id, start_ms, start_ms + 900, text, now],
                )
                .unwrap();
        }
        for (turn_id, sequence, text, start_ms) in [
            (&first_turn, 1_i64, "world", 400_i64),
            (&first_turn, 0_i64, "hello", 0_i64),
            (&second_turn, 0_i64, "goodbye", 1_000_i64),
        ] {
            connection
                .execute(
                    "INSERT INTO words(
                        id,turn_id,sequence,start_ms,end_ms,text,confidence,is_overlap
                     ) VALUES (?1,?2,?3,?4,?5,?6,0.9,0)",
                    params![new_id(), turn_id, sequence, start_ms, start_ms + 300, text],
                )
                .unwrap();
        }
        drop(connection);

        let detail = service.get_meeting(&meeting_id).unwrap();

        assert_eq!(detail.turns.len(), 2);
        assert_eq!(
            detail.turns[0]
                .words
                .iter()
                .map(|word| word.text.as_str())
                .collect::<Vec<_>>(),
            vec!["hello", "world"]
        );
        assert_eq!(detail.turns[1].words[0].text, "goodbye");
    }

    #[test]
    fn delete_meeting_removes_recording_and_terminal_job_directories() {
        let (_temp, service) = service();
        let meeting_id = new_id();
        let job_id = new_id();
        let now = now_ms();
        let connection = service.database.connect().unwrap();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Delete','recording','ready',?2)",
                params![meeting_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO processing_jobs(
                    id,meeting_id,stage,status,progress,input_json,created_at_ms,updated_at_ms
                 ) VALUES (?1,?2,'finalize','completed',1,'{}',?3,?3)",
                params![job_id, meeting_id, now],
            )
            .unwrap();
        drop(connection);
        let recording_directory = service.layout.recordings().join(&meeting_id);
        let work_directory = service.layout.work().join(&job_id);
        fs::create_dir_all(&recording_directory).unwrap();
        fs::create_dir_all(&work_directory).unwrap();
        fs::write(recording_directory.join("track.flac"), b"recording").unwrap();
        fs::write(work_directory.join("scratch.bin"), b"scratch").unwrap();

        service.delete_meeting(&meeting_id).unwrap();

        assert!(!recording_directory.exists());
        assert!(!work_directory.exists());
        assert!(matches!(
            service.get_meeting_summary(&meeting_id),
            Err(CoreError::NotFound(_))
        ));
    }

    #[test]
    fn delete_meeting_refuses_active_job_and_preserves_resume_workspace() {
        let (_temp, service) = service();
        let meeting_id = new_id();
        let job_id = new_id();
        let now = now_ms();
        let connection = service.database.connect().unwrap();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Resume','import','processing',?2)",
                params![meeting_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO processing_jobs(
                    id,meeting_id,stage,status,progress,input_json,created_at_ms,updated_at_ms
                 ) VALUES (?1,?2,'transcribe','retry_wait',0.4,'{}',?3,?3)",
                params![job_id, meeting_id, now],
            )
            .unwrap();
        drop(connection);
        let workspace = service.layout.work().join(&job_id);
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("checkpoint.bin"), b"resume").unwrap();

        assert!(matches!(
            service.delete_meeting(&meeting_id),
            Err(CoreError::Conflict(_))
        ));
        assert!(service.get_meeting_summary(&meeting_id).is_ok());
        assert_eq!(
            fs::read(workspace.join("checkpoint.bin")).unwrap(),
            b"resume"
        );
    }

    #[test]
    fn edited_transcript_survives_and_exports_effective_text() {
        let (_temp, service) = service();
        let connection = service.database.connect().unwrap();
        let now = now_ms();
        let meeting_id = new_id();
        let turn_id = new_id();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Test','import','ready',?2)",
                params![meeting_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO transcript_turns(
                    id,meeting_id,start_ms,end_ms,model_text,created_at_ms,updated_at_ms
                 ) VALUES (?1,?2,0,1000,'machine text',?3,?3)",
                params![turn_id, meeting_id, now],
            )
            .unwrap();
        let edited = service
            .update_transcript_turn(&turn_id, "human correction".into(), Some(0))
            .unwrap();
        assert_eq!(edited.model_text, "machine text");
        assert_eq!(edited.text, "human correction");
        let (_, export) = service
            .export_transcript(&meeting_id, ExportFormat::Txt)
            .unwrap();
        assert!(String::from_utf8(export)
            .unwrap()
            .contains("human correction"));
    }

    #[test]
    fn meeting_rename_is_validated_and_persisted() {
        let (_temp, service) = service();
        let meeting_id = new_id();
        service
            .database
            .connect()
            .unwrap()
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Old title','import','ready',?2)",
                params![meeting_id, now_ms()],
            )
            .unwrap();

        let renamed = service
            .rename_meeting(&meeting_id, "  New title  ".into())
            .unwrap();

        assert_eq!(renamed.title, "New title");
        assert!(matches!(
            service.rename_meeting(&meeting_id, " ".into()),
            Err(CoreError::InvalidInput(_))
        ));
        assert!(matches!(
            service.rename_meeting(&meeting_id, "x".repeat(181)),
            Err(CoreError::InvalidInput(_))
        ));
    }

    #[test]
    fn transcript_turn_review_flag_is_persisted_and_returned() {
        let (_temp, service) = service();
        let meeting_id = new_id();
        let turn_id = new_id();
        let now = now_ms();
        let connection = service.database.connect().unwrap();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Review','import','ready',?2)",
                params![meeting_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO transcript_turns(
                    id,meeting_id,start_ms,end_ms,model_text,created_at_ms,updated_at_ms
                 ) VALUES (?1,?2,0,1000,'check me',?3,?3)",
                params![turn_id, meeting_id, now],
            )
            .unwrap();
        drop(connection);

        let reviewed = service.set_transcript_turn_review(&turn_id, true).unwrap();
        assert!(reviewed.needs_review);
        assert!(
            service
                .get_meeting(&meeting_id)
                .unwrap()
                .turns
                .first()
                .unwrap()
                .needs_review
        );
    }

    #[test]
    fn transcript_turn_bookmark_is_persisted_and_returned() {
        let (_temp, service) = service();
        let meeting_id = new_id();
        let turn_id = new_id();
        let now = now_ms();
        let connection = service.database.connect().unwrap();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Bookmark','import','ready',?2)",
                params![meeting_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO transcript_turns(
                    id,meeting_id,start_ms,end_ms,model_text,created_at_ms,updated_at_ms
                 ) VALUES (?1,?2,0,1000,'remember this',?3,?3)",
                params![turn_id, meeting_id, now],
            )
            .unwrap();
        drop(connection);

        let bookmarked = service
            .set_transcript_turn_bookmark(&turn_id, true)
            .unwrap();
        assert!(bookmarked.is_marked);
        assert!(
            service
                .get_meeting(&meeting_id)
                .unwrap()
                .turns
                .first()
                .unwrap()
                .is_marked
        );
    }

    #[test]
    fn stale_edit_revision_is_rejected() {
        let (_temp, service) = service();
        let connection = service.database.connect().unwrap();
        let now = now_ms();
        let meeting_id = new_id();
        let turn_id = new_id();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Test','import','ready',?2)",
                params![meeting_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO transcript_turns(
                    id,meeting_id,start_ms,end_ms,model_text,created_at_ms,updated_at_ms
                 ) VALUES (?1,?2,0,1,'a',?3,?3)",
                params![turn_id, meeting_id, now],
            )
            .unwrap();
        service
            .update_transcript_turn(&turn_id, "b".into(), Some(0))
            .unwrap();
        assert!(matches!(
            service.update_transcript_turn(&turn_id, "c".into(), Some(0)),
            Err(CoreError::Conflict(_))
        ));
    }

    #[test]
    fn export_time_formats_are_spec_compliant() {
        assert_eq!(format_srt_time(3_723_004), "01:02:03,004");
        assert_eq!(format_vtt_time(3_723_004), "01:02:03.004");
    }

    #[test]
    fn voice_confirmation_request_rejects_renderer_embeddings() {
        let request = json!({
            "profileId":new_id(),
            "meetingId":new_id(),
            "speakerId":new_id(),
            "cleanDurationMs":30_000,
            "embedding":[1,2,3,4]
        });
        assert!(serde_json::from_value::<ConfirmVoiceSampleRequest>(request).is_err());
    }

    #[test]
    fn renaming_an_unknown_speaker_automatically_saves_its_voice_profile() {
        let (_temp, service) = service();
        let connection = service.database.connect().unwrap();
        let now = now_ms();
        let meeting_id = new_id();
        let speaker_id = new_id();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Test','import','ready',?2)",
                params![meeting_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO meeting_speakers(
                    id,meeting_id,cluster_label,display_name,match_state,needs_review
                 ) VALUES (?1,?2,'SPEAKER_01','Speaker 2','unknown',1)",
                params![speaker_id, meeting_id],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO voice_profile_candidates(
                    id,meeting_id,speaker_id,cluster_label,clean_duration_ms,
                    encrypted_embedding,pipeline_version,created_at_ms
                 ) VALUES (?1,?2,?3,'SPEAKER_01',12000,?4,?5,?6)",
                params![
                    new_id(),
                    meeting_id,
                    speaker_id,
                    vec![1_u8, 2, 3, 4],
                    crate::worker::PIPELINE_VERSION,
                    now
                ],
            )
            .unwrap();
        drop(connection);

        let result = service
            .rename_speaker(&meeting_id, &speaker_id, "Jordan Lee".into())
            .unwrap();

        assert!(result.profile_created);
        assert!(result.sample_saved);
        assert_eq!(result.speaker.display_name, "Jordan Lee");
        assert_eq!(result.speaker.match_state, SpeakerMatchState::Matched);
        let profile = result.profile.unwrap();
        assert_eq!(profile.name, "Jordan Lee");
        assert_eq!(profile.sample_count, 1);
        assert_eq!(profile.sample_duration_ms, 12_000);
        assert!(profile.ready_for_matching);
    }

    #[test]
    fn legacy_unknown_labels_receive_unique_numbered_names() {
        let (_temp, service) = service();
        let connection = service.database.connect().unwrap();
        let now = now_ms();
        let meeting_id = new_id();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Legacy speakers','import','ready',?2)",
                params![meeting_id, now],
            )
            .unwrap();
        for (label, name) in [
            ("SPEAKER_00", "Unknown"),
            ("SPEAKER_01", "Unknown speaker"),
            ("unknown", "Unknown"),
        ] {
            connection
                .execute(
                    "INSERT INTO meeting_speakers(
                        id,meeting_id,cluster_label,display_name,match_state,needs_review
                     ) VALUES (?1,?2,?3,?4,'unknown',1)",
                    params![new_id(), meeting_id, label, name],
                )
                .unwrap();
        }
        drop(connection);

        let names = service
            .get_meeting(&meeting_id)
            .unwrap()
            .speakers
            .into_iter()
            .map(|speaker| speaker.display_name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["Speaker 1", "Speaker 2", "Speaker 3"]);
    }

    #[test]
    fn voice_confirmation_requires_an_encrypted_worker_candidate() {
        let (_temp, service) = service();
        let connection = service.database.connect().unwrap();
        let now = now_ms();
        let meeting_id = new_id();
        let speaker_id = new_id();
        let profile_id = new_id();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Test','import','ready',?2)",
                params![meeting_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO meeting_speakers(
                    id,meeting_id,cluster_label,display_name
                 ) VALUES (?1,?2,'speaker-0','Unknown')",
                params![speaker_id, meeting_id],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO voice_profiles(
                    id,display_name,created_at_ms,updated_at_ms
                 ) VALUES (?1,'Person',?2,?2)",
                params![profile_id, now],
            )
            .unwrap();
        let result = service.confirm_voice_profile_sample(ConfirmVoiceSampleRequest {
            profile_id,
            meeting_id,
            speaker_id,
        });
        assert!(matches!(result, Err(CoreError::NotFound(_))));
    }

    #[test]
    fn rejecting_a_speaker_match_clears_identity_and_survives_reload() {
        let (_temp, service) = service();
        let connection = service.database.connect().unwrap();
        let now = now_ms();
        let meeting_id = new_id();
        let speaker_id = new_id();
        let profile_id = new_id();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Test','import','ready',?2)",
                params![meeting_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO voice_profiles(
                    id,display_name,created_at_ms,updated_at_ms
                 ) VALUES (?1,'Remembered person',?2,?2)",
                params![profile_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO meeting_speakers(
                    id,meeting_id,cluster_label,display_name,profile_id,match_state,needs_review
                 ) VALUES (?1,?2,'speaker-0','Remembered person',?3,'matched',0)",
                params![speaker_id, meeting_id, profile_id],
            )
            .unwrap();

        service.review_speaker_match(&speaker_id, false).unwrap();

        let reloaded = service.get_meeting(&meeting_id).unwrap();
        let speaker = reloaded
            .speakers
            .into_iter()
            .find(|speaker| speaker.id == speaker_id)
            .unwrap();
        assert_eq!(speaker.display_name, "Speaker 1");
        assert_eq!(speaker.profile_id, None);
        assert_eq!(speaker.match_state, SpeakerMatchState::Unknown);
        assert!(speaker.needs_review);
    }

    #[test]
    fn deleting_a_voice_profile_clears_remembered_speaker_identity() {
        let (_temp, service) = service();
        let connection = service.database.connect().unwrap();
        let now = now_ms();
        let meeting_id = new_id();
        let speaker_id = new_id();
        let profile_id = new_id();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Test','import','ready',?2)",
                params![meeting_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO voice_profiles(
                    id,display_name,created_at_ms,updated_at_ms
                 ) VALUES (?1,'Remembered person',?2,?2)",
                params![profile_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO meeting_speakers(
                    id,meeting_id,cluster_label,display_name,profile_id,match_state,needs_review
                 ) VALUES (?1,?2,'speaker-0','Remembered person',?3,'matched',0)",
                params![speaker_id, meeting_id, profile_id],
            )
            .unwrap();
        drop(connection);

        service.delete_voice_profile(&profile_id).unwrap();

        let speaker = service
            .get_meeting(&meeting_id)
            .unwrap()
            .speakers
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(speaker.profile_id, None);
        assert_eq!(speaker.display_name, "Speaker 1");
        assert_eq!(speaker.match_state, SpeakerMatchState::Unknown);
        assert!(speaker.needs_review);
    }

    #[test]
    fn review_flag_command_does_not_change_speaker_identity() {
        let (_temp, service) = service();
        let connection = service.database.connect().unwrap();
        let now = now_ms();
        let meeting_id = new_id();
        let speaker_id = new_id();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Test','import','ready',?2)",
                params![meeting_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO meeting_speakers(
                    id,meeting_id,cluster_label,display_name,match_state,needs_review
                 ) VALUES (?1,?2,'speaker-0','Manual name','unknown',0)",
                params![speaker_id, meeting_id],
            )
            .unwrap();

        service.set_speaker_review(&speaker_id, true).unwrap();

        let (display_name, match_state, needs_review): (String, String, i64) = connection
            .query_row(
                "SELECT display_name,match_state,needs_review
                 FROM meeting_speakers WHERE id=?1",
                [&speaker_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(display_name, "Manual name");
        assert_eq!(match_state, "unknown");
        assert_eq!(needs_review, 1);
    }

    #[cfg(windows)]
    #[test]
    fn voice_confirmation_promotes_ciphertext_without_renderer_plaintext() {
        let (_temp, service) = service();
        let connection = service.database.connect().unwrap();
        let now = now_ms();
        let meeting_id = new_id();
        let speaker_id = new_id();
        let profile_id = new_id();
        let clear = [1.0_f32, 0.0, 0.0, 0.0]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();
        let protected = crate::crypto::protect_embedding(&clear).unwrap();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Test','import','ready',?2)",
                params![meeting_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO meeting_speakers(
                    id,meeting_id,cluster_label,display_name
                 ) VALUES (?1,?2,'speaker-0','Unknown')",
                params![speaker_id, meeting_id],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO voice_profiles(
                    id,display_name,created_at_ms,updated_at_ms
                 ) VALUES (?1,'Person',?2,?2)",
                params![profile_id, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO voice_profile_candidates(
                    id,meeting_id,speaker_id,cluster_label,clean_duration_ms,
                    encrypted_embedding,pipeline_version,created_at_ms
                 ) VALUES (?1,?2,?3,'speaker-0',12000,?4,?5,?6)",
                params![
                    new_id(),
                    meeting_id,
                    speaker_id,
                    protected,
                    crate::worker::PIPELINE_VERSION,
                    now
                ],
            )
            .unwrap();
        service
            .confirm_voice_profile_sample(ConfirmVoiceSampleRequest {
                profile_id: profile_id.clone(),
                meeting_id: meeting_id.clone(),
                speaker_id: speaker_id.clone(),
            })
            .unwrap();
        let (stored, candidates, display_name, stored_profile_id, state, needs_review): (
            Vec<u8>,
            i64,
            String,
            Option<String>,
            String,
            i64,
        ) = connection
            .query_row(
                "SELECT s.encrypted_embedding,
                        (SELECT count(*) FROM voice_profile_candidates
                         WHERE meeting_id=?1 AND speaker_id=?2),
                        ms.display_name,ms.profile_id,ms.match_state,ms.needs_review
                 FROM voice_profile_samples s
                 JOIN meeting_speakers ms ON ms.id=s.speaker_id
                 WHERE s.profile_id=?3 AND s.meeting_id=?1 AND s.speaker_id=?2",
                params![meeting_id, speaker_id, profile_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_ne!(stored, clear);
        assert_eq!(crate::crypto::unprotect_embedding(&stored).unwrap(), clear);
        assert_eq!(candidates, 0);
        assert_eq!(display_name, "Person");
        assert_eq!(stored_profile_id.as_deref(), Some(profile_id.as_str()));
        assert_eq!(state, "matched");
        assert_eq!(needs_review, 0);
    }
}
