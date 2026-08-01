use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::{SecondsFormat, TimeZone, Utc};
use rusqlite::{Connection, TransactionBehavior};

use crate::error::CoreResult;

pub const SCHEMA_VERSION: u32 = 4;

const MIGRATION_1: &str = r#"
CREATE TABLE IF NOT EXISTS app_meta (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY NOT NULL,
    value_json TEXT NOT NULL CHECK(json_valid(value_json)),
    updated_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS meetings (
    id TEXT PRIMARY KEY NOT NULL,
    title TEXT NOT NULL,
    source_kind TEXT NOT NULL CHECK(source_kind IN ('import', 'recording')),
    status TEXT NOT NULL CHECK(status IN ('importing', 'recording', 'processing', 'ready', 'failed')),
    created_at_ms INTEGER NOT NULL,
    started_at_ms INTEGER,
    ended_at_ms INTEGER,
    duration_ms INTEGER,
    language TEXT NOT NULL DEFAULT 'en',
    has_user_edits INTEGER NOT NULL DEFAULT 0 CHECK(has_user_edits IN (0, 1)),
    model_revision TEXT
) STRICT;

CREATE INDEX IF NOT EXISTS idx_meetings_created ON meetings(created_at_ms DESC);
CREATE INDEX IF NOT EXISTS idx_meetings_status ON meetings(status, created_at_ms DESC);

CREATE TABLE IF NOT EXISTS media_assets (
    id TEXT PRIMARY KEY NOT NULL,
    meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    display_name TEXT NOT NULL,
    relative_path TEXT NOT NULL UNIQUE,
    content_type TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    duration_ms INTEGER,
    codec TEXT,
    sample_rate_hz INTEGER,
    channels INTEGER,
    created_at_ms INTEGER NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS idx_assets_meeting ON media_assets(meeting_id, created_at_ms);

CREATE TABLE IF NOT EXISTS recording_sessions (
    id TEXT PRIMARY KEY NOT NULL,
    meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    state TEXT NOT NULL CHECK(state IN ('starting', 'recording', 'paused', 'finalizing', 'stopped', 'failed')),
    config_json TEXT NOT NULL CHECK(json_valid(config_json)),
    manifest_relative_path TEXT NOT NULL,
    qpc_frequency INTEGER,
    qpc_start INTEGER,
    started_at_ms INTEGER NOT NULL,
    ended_at_ms INTEGER,
    paused_duration_ms INTEGER NOT NULL DEFAULT 0,
    error_message TEXT
) STRICT;

CREATE TABLE IF NOT EXISTS recording_tracks (
    id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL REFERENCES recording_sessions(id) ON DELETE CASCADE,
    source_kind TEXT NOT NULL CHECK(source_kind IN ('microphone', 'loopback', 'mixed')),
    device_id TEXT,
    relative_directory TEXT NOT NULL,
    final_asset_id TEXT REFERENCES media_assets(id) ON DELETE SET NULL,
    sample_rate_hz INTEGER NOT NULL,
    channels INTEGER NOT NULL,
    dropped_packets INTEGER NOT NULL DEFAULT 0,
    discontinuities INTEGER NOT NULL DEFAULT 0,
    qpc_first INTEGER,
    qpc_last INTEGER,
    created_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS recording_pauses (
    id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL REFERENCES recording_sessions(id) ON DELETE CASCADE,
    started_offset_ms INTEGER NOT NULL,
    ended_offset_ms INTEGER
) STRICT;

CREATE TABLE IF NOT EXISTS recording_markers (
    id TEXT PRIMARY KEY NOT NULL,
    meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    offset_ms INTEGER NOT NULL,
    label TEXT,
    created_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS model_outputs (
    id TEXT PRIMARY KEY NOT NULL,
    meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    pipeline_version TEXT NOT NULL,
    model_revisions_json TEXT NOT NULL CHECK(json_valid(model_revisions_json)),
    raw_result_json TEXT NOT NULL CHECK(json_valid(raw_result_json)),
    is_canonical INTEGER NOT NULL DEFAULT 0 CHECK(is_canonical IN (0, 1)),
    created_at_ms INTEGER NOT NULL
) STRICT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_one_canonical_model_output
ON model_outputs(meeting_id) WHERE is_canonical = 1;

CREATE TABLE IF NOT EXISTS voice_profiles (
    id TEXT PRIMARY KEY NOT NULL,
    display_name TEXT NOT NULL,
    color TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    last_used_at_ms INTEGER
) STRICT;

CREATE TABLE IF NOT EXISTS meeting_speakers (
    id TEXT PRIMARY KEY NOT NULL,
    meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    cluster_label TEXT NOT NULL,
    display_name TEXT NOT NULL,
    profile_id TEXT REFERENCES voice_profiles(id) ON DELETE SET NULL,
    match_state TEXT NOT NULL DEFAULT 'unknown' CHECK(match_state IN ('matched', 'review', 'unknown')),
    needs_review INTEGER NOT NULL DEFAULT 0 CHECK(needs_review IN (0, 1)),
    color TEXT,
    UNIQUE(meeting_id, cluster_label)
) STRICT;

CREATE TABLE IF NOT EXISTS speaker_clusters (
    id TEXT PRIMARY KEY NOT NULL,
    model_output_id TEXT NOT NULL REFERENCES model_outputs(id) ON DELETE CASCADE,
    meeting_speaker_id TEXT REFERENCES meeting_speakers(id) ON DELETE SET NULL,
    cluster_label TEXT NOT NULL,
    total_speech_ms INTEGER NOT NULL,
    clean_speech_ms INTEGER NOT NULL,
    encrypted_embedding BLOB,
    overlap_json TEXT CHECK(overlap_json IS NULL OR json_valid(overlap_json))
) STRICT;

CREATE TABLE IF NOT EXISTS voice_profile_samples (
    id TEXT PRIMARY KEY NOT NULL,
    profile_id TEXT NOT NULL REFERENCES voice_profiles(id) ON DELETE CASCADE,
    meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    speaker_id TEXT NOT NULL REFERENCES meeting_speakers(id) ON DELETE CASCADE,
    clean_duration_ms INTEGER NOT NULL CHECK(clean_duration_ms > 0),
    encrypted_embedding BLOB NOT NULL,
    confirmed_at_ms INTEGER NOT NULL,
    UNIQUE(profile_id, meeting_id, speaker_id)
) STRICT;

CREATE TABLE IF NOT EXISTS transcript_turns (
    id TEXT PRIMARY KEY NOT NULL,
    meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    model_output_id TEXT REFERENCES model_outputs(id) ON DELETE SET NULL,
    speaker_id TEXT REFERENCES meeting_speakers(id) ON DELETE SET NULL,
    start_ms INTEGER NOT NULL,
    end_ms INTEGER NOT NULL,
    model_text TEXT NOT NULL,
    edited_text TEXT,
    revision INTEGER NOT NULL DEFAULT 0,
    needs_review INTEGER NOT NULL DEFAULT 0 CHECK(needs_review IN (0, 1)),
    is_draft INTEGER NOT NULL DEFAULT 0 CHECK(is_draft IN (0, 1)),
    is_marked INTEGER NOT NULL DEFAULT 0 CHECK(is_marked IN (0, 1)),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK(end_ms >= start_ms)
) STRICT;

CREATE INDEX IF NOT EXISTS idx_turns_meeting_time
ON transcript_turns(meeting_id, start_ms, end_ms);

CREATE TABLE IF NOT EXISTS transcript_turn_revisions (
    id TEXT PRIMARY KEY NOT NULL,
    turn_id TEXT NOT NULL REFERENCES transcript_turns(id) ON DELETE CASCADE,
    revision INTEGER NOT NULL,
    prior_edited_text TEXT,
    new_edited_text TEXT,
    created_at_ms INTEGER NOT NULL,
    UNIQUE(turn_id, revision)
) STRICT;

CREATE TABLE IF NOT EXISTS words (
    id TEXT PRIMARY KEY NOT NULL,
    turn_id TEXT NOT NULL REFERENCES transcript_turns(id) ON DELETE CASCADE,
    speaker_id TEXT REFERENCES meeting_speakers(id) ON DELETE SET NULL,
    sequence INTEGER NOT NULL,
    start_ms INTEGER NOT NULL,
    end_ms INTEGER NOT NULL,
    text TEXT NOT NULL,
    confidence REAL,
    is_overlap INTEGER NOT NULL DEFAULT 0 CHECK(is_overlap IN (0, 1)),
    UNIQUE(turn_id, sequence)
) STRICT;

CREATE VIRTUAL TABLE IF NOT EXISTS transcript_fts USING fts5(
    turn_id UNINDEXED,
    meeting_id UNINDEXED,
    content,
    tokenize = 'unicode61 remove_diacritics 2'
);

CREATE TRIGGER IF NOT EXISTS transcript_turns_fts_insert AFTER INSERT ON transcript_turns BEGIN
    INSERT INTO transcript_fts(turn_id, meeting_id, content)
    VALUES (NEW.id, NEW.meeting_id, COALESCE(NEW.edited_text, NEW.model_text));
END;

CREATE TRIGGER IF NOT EXISTS transcript_turns_fts_update AFTER UPDATE OF edited_text, model_text ON transcript_turns BEGIN
    DELETE FROM transcript_fts WHERE turn_id = OLD.id;
    INSERT INTO transcript_fts(turn_id, meeting_id, content)
    VALUES (NEW.id, NEW.meeting_id, COALESCE(NEW.edited_text, NEW.model_text));
END;

CREATE TRIGGER IF NOT EXISTS transcript_turns_fts_delete AFTER DELETE ON transcript_turns BEGIN
    DELETE FROM transcript_fts WHERE turn_id = OLD.id;
END;

CREATE TABLE IF NOT EXISTS processing_jobs (
    id TEXT PRIMARY KEY NOT NULL,
    meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    stage TEXT NOT NULL CHECK(stage IN ('ingest', 'normalize', 'transcribe', 'align', 'diarize', 'identify', 'index', 'finalize')),
    status TEXT NOT NULL CHECK(status IN (
        'queued', 'running', 'retry_wait', 'cancel_requested', 'interrupted',
        'completed', 'failed', 'cancelled'
    )),
    progress REAL NOT NULL DEFAULT 0 CHECK(progress >= 0 AND progress <= 1),
    attempts INTEGER NOT NULL DEFAULT 0,
    max_attempts INTEGER NOT NULL DEFAULT 4,
    priority INTEGER NOT NULL DEFAULT 0,
    input_json TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(input_json)),
    output_json TEXT CHECK(output_json IS NULL OR json_valid(output_json)),
    checkpoint_ms INTEGER,
    locked_at_ms INTEGER,
    worker_id TEXT,
    error_code TEXT,
    error_message TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS idx_jobs_queue
ON processing_jobs(status, priority DESC, created_at_ms);
CREATE INDEX IF NOT EXISTS idx_jobs_meeting
ON processing_jobs(meeting_id, created_at_ms);

CREATE TABLE IF NOT EXISTS artifacts (
    id TEXT PRIMARY KEY NOT NULL,
    meeting_id TEXT REFERENCES meetings(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    format TEXT NOT NULL,
    relative_path TEXT NOT NULL UNIQUE,
    size_bytes INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
) STRICT;
"#;

const MIGRATION_2: &str = r#"
CREATE TABLE IF NOT EXISTS speaker_merge_rules (
    meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    source_cluster_label TEXT NOT NULL,
    target_speaker_id TEXT NOT NULL REFERENCES meeting_speakers(id) ON DELETE CASCADE,
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY(meeting_id, source_cluster_label)
) STRICT;

CREATE INDEX IF NOT EXISTS idx_speaker_merge_target
ON speaker_merge_rules(target_speaker_id);
"#;

const MIGRATION_3: &str = r#"
CREATE TABLE IF NOT EXISTS voice_profile_candidates (
    id TEXT PRIMARY KEY NOT NULL,
    meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    speaker_id TEXT NOT NULL REFERENCES meeting_speakers(id) ON DELETE CASCADE,
    cluster_label TEXT NOT NULL,
    clean_duration_ms INTEGER NOT NULL CHECK(clean_duration_ms > 0),
    encrypted_embedding BLOB NOT NULL,
    pipeline_version TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    UNIQUE(meeting_id, speaker_id)
) STRICT;

CREATE INDEX IF NOT EXISTS idx_profile_candidates_speaker
ON voice_profile_candidates(meeting_id, speaker_id);
"#;

const MIGRATION_4: &str = r#"
ALTER TABLE recording_tracks ADD COLUMN frames_written INTEGER NOT NULL DEFAULT 0;
ALTER TABLE recording_tracks ADD COLUMN clock_anchors_json TEXT NOT NULL DEFAULT '[]'
    CHECK(json_valid(clock_anchors_json));
ALTER TABLE recording_tracks ADD COLUMN start_offset_ms INTEGER NOT NULL DEFAULT 0;
ALTER TABLE recording_tracks ADD COLUMN clock_scale REAL NOT NULL DEFAULT 1.0;
ALTER TABLE meetings ADD COLUMN needs_review INTEGER NOT NULL DEFAULT 0
    CHECK(needs_review IN (0, 1));
ALTER TABLE meetings ADD COLUMN recovery_warning TEXT;
"#;

#[derive(Debug, Clone)]
pub struct Database {
    path: PathBuf,
}

impl Database {
    pub fn open(path: impl Into<PathBuf>) -> CoreResult<Self> {
        let database = Self { path: path.into() };
        let mut connection = database.connect()?;
        database.migrate(&mut connection)?;
        Ok(database)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn connect(&self) -> CoreResult<Connection> {
        let connection = Connection::open(&self.path)?;
        connection.busy_timeout(std::time::Duration::from_secs(10))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA temp_store = MEMORY;",
        )?;
        Ok(connection)
    }

    fn migrate(&self, connection: &mut Connection) -> CoreResult<()> {
        let current: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if current < 1 {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(MIGRATION_1)?;
            transaction.pragma_update(None, "user_version", 1)?;
            let now = now_ms();
            transaction.execute(
                "INSERT OR IGNORE INTO app_meta(key, value, updated_at_ms)
                 VALUES ('created_at_ms', ?1, ?2)",
                (now.to_string(), now),
            )?;
            transaction.commit()?;
        }
        if current < 2 {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(MIGRATION_2)?;
            transaction.pragma_update(None, "user_version", 2)?;
            transaction.commit()?;
        }
        if current < 3 {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(MIGRATION_3)?;
            transaction.pragma_update(None, "user_version", 3)?;
            transaction.commit()?;
        }
        if current < 4 {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(MIGRATION_4)?;
            transaction.pragma_update(None, "user_version", 4)?;
            transaction.commit()?;
        }
        Ok(())
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

pub fn iso_from_ms(value: i64) -> String {
    Utc.timestamp_millis_opt(value)
        .single()
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_is_idempotent_and_enables_required_pragmas() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("library.sqlite3");
        let first = Database::open(&path).unwrap();
        let second = Database::open(&path).unwrap();
        let connection = second.connect().unwrap();
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        let foreign_keys: u32 = connection
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap();
        let journal_mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(foreign_keys, 1);
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
        assert_eq!(first.path(), second.path());
    }

    #[test]
    fn fts_tracks_edits_without_overwriting_model_text() {
        let temp = tempfile::tempdir().unwrap();
        let database = Database::open(temp.path().join("library.sqlite3")).unwrap();
        let connection = database.connect().unwrap();
        let now = now_ms();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES ('m','Meeting','import','ready',?1)",
                [now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO transcript_turns(
                    id,meeting_id,start_ms,end_ms,model_text,created_at_ms,updated_at_ms
                 ) VALUES ('t','m',0,1000,'model words',?1,?1)",
                [now],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE transcript_turns SET edited_text='corrected phrase' WHERE id='t'",
                [],
            )
            .unwrap();
        let model: String = connection
            .query_row(
                "SELECT model_text FROM transcript_turns WHERE id='t'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let hits: i64 = connection
            .query_row(
                "SELECT count(*) FROM transcript_fts WHERE transcript_fts MATCH 'corrected'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(model, "model words");
        assert_eq!(hits, 1);
    }
}
