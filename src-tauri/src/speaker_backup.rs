use crate::{
    db::now_ms,
    error::{CoreError, CoreResult},
    service::CoreService,
};
use rusqlite::{params, params_from_iter, types::Value as SqlValue, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
};
use zeroize::Zeroize;

// Explicit column lists are the versioned format: schema additions cannot silently
// change the file contract. Voice vectors remain protected by Windows DPAPI.
const TABLES: &[(&str, &str)] = &[
    ("voice_profiles", "id,display_name,color,created_at_ms,updated_at_ms,last_used_at_ms"),
    ("meeting_speakers", "id,meeting_id,cluster_label,display_name,profile_id,match_state,needs_review,color,attribution_source,attribution_confidence"),
    ("voice_profile_samples", "id,profile_id,meeting_id,speaker_id,clean_duration_ms,encrypted_embedding,confirmed_at_ms"),
    ("speaker_merge_rules", "meeting_id,source_cluster_label,target_speaker_id,created_at_ms"),
];
#[derive(Serialize, Deserialize)]
struct SpeakerBackup {
    format: String,
    version: u32,
    created_at_ms: i64,
    tables: Vec<Vec<Vec<serde_json::Value>>>,
}
#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeakerBackupResult {
    pub path: Option<String>,
    pub profiles: usize,
    pub samples: usize,
    pub speakers: usize,
    pub skipped: usize,
}
fn invalid(message: &str) -> CoreError {
    CoreError::InvalidInput(message.into())
}

impl CoreService {
    pub fn speaker_backup_bytes(&self) -> CoreResult<(SpeakerBackupResult, Vec<u8>)> {
        let mut connection = self.database().connect()?;
        let transaction = connection.transaction()?;
        let mut tables = Vec::new();
        for (table, columns) in TABLES {
            let mut statement = transaction.prepare(&format!("SELECT {columns} FROM {table}"))?;
            let width = statement.column_count();
            let rows = statement
                .query_map([], |row| {
                    (0..width)
                        .map(|index| {
                            let value: SqlValue = row.get(index)?;
                            Ok(match value {
                                SqlValue::Null => serde_json::Value::Null,
                                SqlValue::Integer(n) => n.into(),
                                SqlValue::Real(n) => n.into(),
                                SqlValue::Text(s) => s.into(),
                                SqlValue::Blob(b) => serde_json::json!(b),
                            })
                        })
                        .collect::<rusqlite::Result<Vec<_>>>()
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            tables.push(rows);
        }
        let result = SpeakerBackupResult {
            profiles: tables[0].len(),
            speakers: tables[1].len(),
            samples: tables[2].len(),
            ..Default::default()
        };
        let bytes = serde_json::to_vec_pretty(&SpeakerBackup {
            format: "saytrace-speakers".into(),
            version: 1,
            created_at_ms: now_ms(),
            tables,
        })?;
        Ok((result, bytes))
    }

    pub fn backup_speakers_best_effort(&self) {
        if let Err(error) = self.backup_speakers_automatically() {
            log::warn!("could not save speaker recovery snapshot: {error}");
        }
    }

    pub fn backup_speakers_automatically(&self) -> CoreResult<()> {
        let (counts, bytes) = self.speaker_backup_bytes()?;
        if counts.profiles + counts.speakers == 0 {
            return Ok(());
        }
        let directory = self.layout().backups().join("speakers");
        fs::create_dir_all(&directory)?;
        // Keep a rolling history of speaker-only snapshots, never entire media/DB.
        let path = directory.join(format!(
            "speakers-{}-{}.json",
            now_ms(),
            uuid::Uuid::now_v7()
        ));
        write_backup(&path, &bytes)?;
        let mut files = fs::read_dir(&directory)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.starts_with("speakers-") && s.ends_with(".json"))
            })
            .collect::<Vec<_>>();
        files.sort();
        let excess = files.len().saturating_sub(50);
        for old in files.into_iter().take(excess) {
            fs::remove_file(old)?;
        }
        Ok(())
    }

    pub fn restore_speaker_backup(&self, bytes: &[u8]) -> CoreResult<SpeakerBackupResult> {
        if bytes.len() > 64 * 1024 * 1024 {
            return Err(invalid("speaker backup exceeds 64 MB"));
        }
        let backup: SpeakerBackup = serde_json::from_slice(bytes)?;
        if backup.format != "saytrace-speakers"
            || backup.version != 1
            || backup.tables.len() != TABLES.len()
        {
            return Err(invalid("unsupported speaker backup format or version"));
        }
        // Validate all rows and DPAPI ownership before any persistent change.
        let mut decoded = Vec::new();
        for (table_index, rows) in backup.tables.iter().enumerate() {
            if rows.len() > 100_000 {
                return Err(invalid("too many speaker backup rows"));
            }
            let width = TABLES[table_index].1.split(',').count();
            let mut converted = Vec::new();
            for row in rows {
                if row.len() != width {
                    return Err(invalid("invalid speaker backup row"));
                }
                let mut values = Vec::new();
                for (column, value) in row.iter().enumerate() {
                    if table_index == 2 && column == 5 && !value.is_array() {
                        return Err(invalid(
                            "encrypted voice sample must be a protected byte array",
                        ));
                    }
                    let v = match value {
                        serde_json::Value::Null => SqlValue::Null,
                        serde_json::Value::String(s) if s.len() <= 4096 => {
                            SqlValue::Text(s.clone())
                        }
                        serde_json::Value::Number(n) if n.as_i64().is_some() => {
                            SqlValue::Integer(n.as_i64().unwrap())
                        }
                        serde_json::Value::Number(n) if n.as_f64().is_some_and(f64::is_finite) => {
                            SqlValue::Real(n.as_f64().unwrap())
                        }
                        serde_json::Value::Array(_) if table_index == 2 && column == 5 => {
                            let blob: Vec<u8> = serde_json::from_value(value.clone())?;
                            if blob.len() > 65536 {
                                return Err(invalid("invalid encrypted voice sample"));
                            }
                            let mut clear = crate::crypto::unprotect_embedding(&blob)?;
                            let decoded = crate::job_coordinator::decode_embedding(&clear);
                            clear.zeroize();
                            let mut vector = decoded?;
                            vector.zeroize();
                            SqlValue::Blob(blob)
                        }
                        _ => return Err(invalid("invalid speaker backup value")),
                    };
                    values.push(v);
                }
                converted.push(values);
            }
            decoded.push(converted);
        }
        self.backup_speakers_automatically()?;
        let mut connection = self.database().connect()?;
        let transaction = connection.transaction()?;
        let mut result = SpeakerBackupResult::default();
        for (index, rows) in decoded.iter().enumerate() {
            let (table, columns) = TABLES[index];
            let column_names = columns.split(',').collect::<Vec<_>>();
            let placeholders = vec!["?"; column_names.len()].join(",");
            let assignments = column_names
                .iter()
                .map(|c| format!("{c}=excluded.{c}"))
                .collect::<Vec<_>>()
                .join(",");
            for row in rows {
                let valid = match index {
                    1 => exists(&transaction, "meetings", &row[1])?,
                    2 => {
                        exists(&transaction, "meetings", &row[2])?
                            && exists(&transaction, "meeting_speakers", &row[3])?
                    }
                    3 => {
                        exists(&transaction, "meetings", &row[0])?
                            && exists(&transaction, "meeting_speakers", &row[2])?
                    }
                    _ => true,
                };
                if !valid {
                    result.skipped += 1;
                    continue;
                }
                if index == 1 {
                    let conflicting: bool = transaction.query_row(
                        "SELECT EXISTS(SELECT 1 FROM meeting_speakers WHERE (id=?1 AND (meeting_id<>?2 OR cluster_label<>?3)) OR (meeting_id=?2 AND cluster_label=?3 AND id<>?1))",
                        params![row[0],row[1],row[2]], |r| r.get(0))?;
                    if conflicting {
                        return Err(invalid(
                            "speaker backup IDs conflict with the existing meeting library",
                        ));
                    }
                }
                if index == 2 && !belongs_to(&transaction, &row[3], &row[2])? {
                    return Err(invalid(
                        "voice sample speaker belongs to a different meeting",
                    ));
                }
                if index == 3 && !belongs_to(&transaction, &row[2], &row[0])? {
                    return Err(invalid(
                        "speaker merge target belongs to a different meeting",
                    ));
                }
                transaction.execute(&format!("INSERT INTO {table}({columns}) VALUES({placeholders}) ON CONFLICT DO UPDATE SET {assignments}"), params_from_iter(row.iter()))?;
                match index {
                    0 => result.profiles += 1,
                    1 => result.speakers += 1,
                    2 => result.samples += 1,
                    _ => (),
                }
            }
        }
        // Apply saved merges to the existing visual transcript immediately.
        for row in &decoded[3] {
            if !exists(&transaction, "meetings", &row[0])?
                || !exists(&transaction, "meeting_speakers", &row[2])?
            {
                continue;
            }
            let source: Option<String> = transaction
                .query_row(
                    "SELECT id FROM meeting_speakers WHERE meeting_id=?1 AND cluster_label=?2",
                    params![row[0], row[1]],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(source) = source else {
                continue;
            };
            if SqlValue::Text(source.clone()) == row[2] {
                return Err(invalid("speaker backup contains a self merge"));
            }
            transaction.execute(
                "UPDATE transcript_turns SET speaker_id=?1 WHERE speaker_id=?2",
                params![row[2], source],
            )?;
            transaction.execute(
                "UPDATE words SET speaker_id=?1 WHERE speaker_id=?2",
                params![row[2], source],
            )?;
            transaction.execute(
                "UPDATE speaker_clusters SET meeting_speaker_id=?1 WHERE meeting_speaker_id=?2",
                params![row[2], source],
            )?;
            transaction.execute(
                "UPDATE visual_context_events SET speaker_id=?1 WHERE speaker_id=?2",
                params![row[2], source],
            )?;
            transaction.execute("DELETE FROM voice_profile_samples WHERE speaker_id=?1 AND profile_id IN (SELECT profile_id FROM voice_profile_samples WHERE speaker_id=?2)",params![source,row[2]])?;
            transaction.execute(
                "UPDATE voice_profile_samples SET speaker_id=?1 WHERE speaker_id=?2",
                params![row[2], source],
            )?;
            transaction.execute(
                "UPDATE speaker_merge_rules SET target_speaker_id=?1 WHERE target_speaker_id=?2",
                params![row[2], source],
            )?;
            transaction.execute("DELETE FROM meeting_speakers WHERE id=?1", [source])?;
        }
        transaction.commit()?;
        self.backup_speakers_best_effort();
        Ok(result)
    }
}
fn belongs_to(connection: &Connection, speaker: &SqlValue, meeting: &SqlValue) -> CoreResult<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM meeting_speakers WHERE id=?1 AND meeting_id=?2)",
        params![speaker, meeting],
        |r| r.get(0),
    )?)
}
fn exists(connection: &Connection, table: &str, id: &SqlValue) -> CoreResult<bool> {
    Ok(connection.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE id=?1)"),
        [id],
        |r| r.get(0),
    )?)
}
pub fn write_backup(path: &std::path::Path, bytes: &[u8]) -> CoreResult<()> {
    if path.exists() {
        return Err(invalid(
            "choose a new filename to preserve the existing speaker backup",
        ));
    }
    let temporary = path.with_extension(format!("{}.partial", uuid::Uuid::now_v7()));
    let result = (|| -> CoreResult<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if fs::read(&temporary)? != bytes {
            return Err(invalid("speaker backup verification failed"));
        }
        if path.exists() {
            return Err(invalid("speaker backup filename already exists"));
        }
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    fn fixture() -> (tempfile::TempDir, CoreService) {
        let directory = tempfile::tempdir().unwrap();
        let core = CoreService::open(directory.path()).unwrap();
        let db = core.database().connect().unwrap();
        db.execute("INSERT INTO meetings(id,title,source_kind,status,created_at_ms) VALUES ('m','Meeting','import','ready',1)", []).unwrap();
        db.execute("INSERT INTO voice_profiles(id,display_name,created_at_ms,updated_at_ms) VALUES ('p','Jordan',1,1)", []).unwrap();
        db.execute("INSERT INTO meeting_speakers(id,meeting_id,cluster_label,display_name,profile_id,match_state,attribution_source) VALUES ('s','m','speaker-0','Jordan','p','matched','manual')", []).unwrap();
        db.execute("INSERT INTO speaker_merge_rules(meeting_id,source_cluster_label,target_speaker_id,created_at_ms) VALUES ('m','speaker-1','s',1)", []).unwrap();
        (directory, core)
    }
    #[test]
    fn backup_file_is_complete_and_existing_copy_is_never_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("backup.json");
        write_backup(&path, b"{\"version\":1}").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"{\"version\":1}");
        assert!(write_backup(&path, b"replacement").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"{\"version\":1}");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
    #[test]
    fn speaker_backup_roundtrip_restores_labels_profiles_and_rules() {
        let (_directory, core) = fixture();
        let (_, bytes) = core.speaker_backup_bytes().unwrap();
        let db = core.database().connect().unwrap();
        db.execute("DELETE FROM voice_profiles", []).unwrap();
        db.execute(
            "UPDATE meeting_speakers SET display_name='Unknown',match_state='unknown'",
            [],
        )
        .unwrap();
        db.execute("DELETE FROM speaker_merge_rules", []).unwrap();
        let result = core.restore_speaker_backup(&bytes).unwrap();
        assert_eq!(
            (result.profiles, result.speakers, result.skipped),
            (1, 1, 0)
        );
        let row: (String, String, String) = db
            .query_row(
                "SELECT display_name,profile_id,match_state FROM meeting_speakers",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, ("Jordan".into(), "p".into(), "matched".into()));
        assert_eq!(
            db.query_row("SELECT count(*) FROM speaker_merge_rules", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        // Repeated restore is idempotent.
        core.restore_speaker_backup(&bytes).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM voice_profiles", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
    #[test]
    fn backup_rejects_future_versions_without_changing_data_and_skips_missing_meetings() {
        let (_directory, core) = fixture();
        let (_, bytes) = core.speaker_backup_bytes().unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["version"] = 2.into();
        assert!(core
            .restore_speaker_backup(&serde_json::to_vec(&value).unwrap())
            .is_err());
        let db = core.database().connect().unwrap();
        db.execute("DELETE FROM meetings", []).unwrap();
        let result = core.restore_speaker_backup(&bytes).unwrap();
        assert_eq!(result.skipped, 2);
        assert_eq!(result.speakers, 0);
    }
    #[test]
    fn restore_reapplies_merge_to_visual_turns_and_words() {
        let (_directory, core) = fixture();
        let (_, bytes) = core.speaker_backup_bytes().unwrap();
        let db = core.database().connect().unwrap();
        db.execute("DELETE FROM speaker_merge_rules", []).unwrap();
        db.execute("INSERT INTO meeting_speakers(id,meeting_id,cluster_label,display_name) VALUES ('source','m','speaker-1','Unknown')",[]).unwrap();
        db.execute("INSERT INTO transcript_turns(id,meeting_id,speaker_id,start_ms,end_ms,model_text,created_at_ms,updated_at_ms) VALUES ('turn','m','source',0,100,'hello',1,1)",[]).unwrap();
        db.execute("INSERT INTO words(id,turn_id,speaker_id,sequence,start_ms,end_ms,text) VALUES ('word','turn','source',0,0,100,'hello')",[]).unwrap();
        core.restore_speaker_backup(&bytes).unwrap();
        let speaker: String = db
            .query_row(
                "SELECT speaker_id FROM transcript_turns WHERE id='turn'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let word: String = db
            .query_row("SELECT speaker_id FROM words WHERE id='word'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(speaker, "s");
        assert_eq!(word, "s");
        assert!(!exists(&db, "meeting_speakers", &SqlValue::Text("source".into())).unwrap());
    }
    #[test]
    fn restore_rejects_cross_meeting_merge_and_conflicting_speaker_ids_atomically() {
        let (_directory, core) = fixture();
        let db = core.database().connect().unwrap();
        db.execute("INSERT INTO meetings(id,title,source_kind,status,created_at_ms) VALUES ('other','Other','import','ready',1)", []).unwrap();
        let (_, bytes) = core.speaker_backup_bytes().unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["tables"][0][0][1] = serde_json::json!("Should roll back");
        value["tables"][3][0][0] = serde_json::json!("other");
        assert!(core
            .restore_speaker_backup(&serde_json::to_vec(&value).unwrap())
            .is_err());
        let name: String = db
            .query_row(
                "SELECT display_name FROM voice_profiles WHERE id='p'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(name, "Jordan");
        value = serde_json::from_slice(&bytes).unwrap();
        value["tables"][1][0][0] = serde_json::json!("different-id");
        assert!(core
            .restore_speaker_backup(&serde_json::to_vec(&value).unwrap())
            .is_err());
    }
    #[test]
    fn manual_merge_preserves_confirmed_voice_sample() {
        let directory = tempfile::tempdir().unwrap();
        let core = CoreService::open(directory.path()).unwrap();
        let meeting = uuid::Uuid::now_v7().to_string();
        let source = uuid::Uuid::now_v7().to_string();
        let target = uuid::Uuid::now_v7().to_string();
        let db = core.database().connect().unwrap();
        db.execute("INSERT INTO meetings(id,title,source_kind,status,created_at_ms) VALUES (?1,'Meeting','import','ready',1)",[&meeting]).unwrap();
        db.execute("INSERT INTO voice_profiles(id,display_name,created_at_ms,updated_at_ms) VALUES ('p','Jordan',1,1)",[]).unwrap();
        db.execute("INSERT INTO meeting_speakers(id,meeting_id,cluster_label,display_name) VALUES (?1,?3,'a','Jordan'),(?2,?3,'b','Jordan')",params![source,target,meeting]).unwrap();
        db.execute("INSERT INTO voice_profile_samples(id,profile_id,meeting_id,speaker_id,clean_duration_ms,encrypted_embedding,confirmed_at_ms) VALUES ('sample','p',?1,?2,12000,?3,1)",params![meeting,source,vec![1u8,2,3]]).unwrap();
        core.merge_speakers(&meeting, &source, &target).unwrap();
        let retained: String = db
            .query_row(
                "SELECT speaker_id FROM voice_profile_samples WHERE id='sample'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(retained, target);
    }
    #[test]
    fn malformed_sample_cannot_bypass_protection_validation() {
        let (_directory, core) = fixture();
        let (_, bytes) = core.speaker_backup_bytes().unwrap();
        for embedding in [
            serde_json::Value::Null,
            serde_json::json!("not-protected"),
            serde_json::json!(12),
        ] {
            let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            value["tables"][2] =
                serde_json::json!([["sample", "p", "m", "s", 12000, embedding, 1]]);
            assert!(core
                .restore_speaker_backup(&serde_json::to_vec(&value).unwrap())
                .is_err());
        }
    }
    #[test]
    fn fractional_legacy_confidence_is_accepted() {
        let (_directory, core) = fixture();
        let (_, bytes) = core.speaker_backup_bytes().unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["tables"][1][0][9] = serde_json::json!(0.85);
        core.restore_speaker_backup(&serde_json::to_vec(&value).unwrap())
            .unwrap();
        let confidence: String = core
            .database()
            .connect()
            .unwrap()
            .query_row(
                "SELECT attribution_confidence FROM meeting_speakers",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(confidence, "0.85");
    }
    #[cfg(windows)]
    #[test]
    fn encrypted_voice_sample_survives_backup_and_restore() {
        let (_directory, core) = fixture();
        let encrypted = crate::crypto::protect_embedding(b"[0.25,0.75]").unwrap();
        let db = core.database().connect().unwrap();
        db.execute("INSERT INTO voice_profile_samples(id,profile_id,meeting_id,speaker_id,clean_duration_ms,encrypted_embedding,confirmed_at_ms) VALUES ('sample','p','m','s',12000,?1,1)",params![encrypted]).unwrap();
        let (_, bytes) = core.speaker_backup_bytes().unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("[0.25,0.75]"));
        db.execute("DELETE FROM voice_profile_samples", []).unwrap();
        assert_eq!(core.restore_speaker_backup(&bytes).unwrap().samples, 1);
        let restored: Vec<u8> = db
            .query_row(
                "SELECT encrypted_embedding FROM voice_profile_samples",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            crate::crypto::unprotect_embedding(&restored).unwrap(),
            b"[0.25,0.75]"
        );
    }
}
