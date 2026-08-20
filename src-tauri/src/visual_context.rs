use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use uuid::Uuid;

use crate::{
    db::now_ms,
    error::{CoreError, CoreResult},
    local_agent::{self, VisualSpeakerFrame, VisualSpeakerObservation},
    media,
    media_tools::{self, MediaTool},
    models::{ImportVisualContextPolicy, RecordingConfig},
    service::CoreService,
    worker::WorkerSupervisor,
};

const ANALYZER_VERSION: &str = "2026.08.18.1";
const MAX_INLINE_SCREENSHOTS: usize = 12;
const MIN_SCREENSHOT_SPACING_MS: i64 = 20_000;
const MAX_VISION_FRAMES: usize = 12;
const MAX_FRAME_BYTES: u64 = 2 * 1024 * 1024;
const VISUAL_CLAIM_LEASE_MS: i64 = 30 * 60 * 1000;
const FRAME_EXTRACTION_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_CAPTURED_FFMPEG_ERROR_BYTES: usize = 64 * 1024;

#[derive(Debug, Default)]
pub struct VisualContextReport {
    pub screenshots_created: usize,
    pub visual_speakers_suggested: usize,
    pub vision_model: Option<String>,
    pub warnings: Vec<String>,
}

impl VisualContextReport {
    pub fn created_context(&self) -> bool {
        self.screenshots_created > 0 || self.visual_speakers_suggested > 0
    }
}

#[derive(Debug)]
pub struct VisualContextBackfill {
    pub meeting_id: String,
    pub report: VisualContextReport,
}

#[derive(Debug, Clone)]
struct TurnCandidate {
    id: String,
    speaker_id: Option<String>,
    start_ms: i64,
    end_ms: i64,
    text: String,
    marked: bool,
}

#[derive(Debug, Clone)]
struct MomentCandidate {
    turn: TurnCandidate,
    at_ms: i64,
    score: u8,
    kind: &'static str,
    reason: String,
    trigger: Option<String>,
}

#[derive(Debug)]
struct VisionFrameRecord {
    frame: VisualSpeakerFrame,
}

#[derive(Default)]
struct TemporaryFrameFiles(Vec<PathBuf>);

impl Drop for TemporaryFrameFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisualSourceKind {
    Screen,
    ImportedVideo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VisualSource {
    asset_id: String,
    relative_path: String,
    meeting_source_kind: String,
    kind: VisualSourceKind,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct VisualEnrichmentConfig {
    enabled: bool,
    auto_screenshots: bool,
    visual_speaker_attribution: bool,
}

/// Enrich a canonical transcript from its synchronized visual source.
///
/// Recordings use their captured screen and persisted recording flags. Managed
/// video imports use the original imported video only when their durable import
/// job carries the default-on visual-context policy. This is deliberately
/// best-effort in the bounded background visual worker: a visual-source or
/// local-vision failure must never invalidate an otherwise canonical audio
/// transcript.
#[cfg(test)]
fn enrich_meeting(core: &CoreService, meeting_id: &str) -> CoreResult<VisualContextReport> {
    enrich_meeting_with_optional_worker(core, None, meeting_id)
}

fn enrich_meeting_with_optional_worker(
    core: &CoreService,
    worker: Option<&WorkerSupervisor>,
    meeting_id: &str,
) -> CoreResult<VisualContextReport> {
    let result = enrich_meeting_inner(core, worker, meeting_id);
    if let Err(error) = &result {
        let _ = mark_run_failed(core, meeting_id, &error.to_string());
    }
    result
}

/// Claims and enriches one ready meeting whose durable visual source predates
/// visual-context processing. Historical imports are eligible only when their
/// original import job contains an explicit visual-context policy, preventing a
/// new binary from sweeping an existing library of long videos. The claim is
/// committed before any FFmpeg or local-vision work, so concurrent coordinator
/// polls cannot duplicate it and a failed attempt remains visible in
/// `visual_context_runs`.
#[cfg(test)]
fn backfill_next_missing(core: &CoreService) -> CoreResult<Option<VisualContextBackfill>> {
    backfill_next_missing_with_optional_worker(core, None)
}

pub fn backfill_next_missing_with_worker(
    core: &CoreService,
    worker: &WorkerSupervisor,
) -> CoreResult<Option<VisualContextBackfill>> {
    backfill_next_missing_with_optional_worker(core, Some(worker))
}

fn backfill_next_missing_with_optional_worker(
    core: &CoreService,
    worker: Option<&WorkerSupervisor>,
) -> CoreResult<Option<VisualContextBackfill>> {
    let Some(meeting_id) = claim_next_missing(core)? else {
        return Ok(None);
    };
    let report = enrich_meeting_with_optional_worker(core, worker, &meeting_id)?;
    Ok(Some(VisualContextBackfill { meeting_id, report }))
}

fn claim_next_missing(core: &CoreService) -> CoreResult<Option<String>> {
    let now = now_ms();
    let stale_before = now.saturating_sub(VISUAL_CLAIM_LEASE_MS);
    let mut connection = core.database().connect()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let candidate = transaction
        .query_row(
            r#"WITH ranked_sources AS (
                    SELECT m.id AS meeting_id,m.created_at_ms,a.id AS asset_id,
                           row_number() OVER (
                               PARTITION BY m.id
                               ORDER BY CASE WHEN a.kind='screen' THEN 0 ELSE 1 END,
                                        a.created_at_ms DESC,a.id DESC
                           ) AS source_rank
                    FROM meetings m
                    JOIN media_assets a ON a.meeting_id=m.id
                    WHERE m.status='ready'
                      AND (
                          (m.source_kind='recording' AND a.kind='screen')
                          OR
                          (m.source_kind='import'
                           AND EXISTS (
                               SELECT 1 FROM processing_jobs policy
                               WHERE policy.meeting_id=m.id
                                 AND json_type(policy.input_json,'$.visualContext')='object'
                           )
                           AND (
                               a.kind='screen'
                               OR
                               (a.kind='video' AND EXISTS (
                                   SELECT 1 FROM processing_jobs source_job
                                   WHERE source_job.meeting_id=m.id
                                     AND json_type(source_job.input_json,'$.visualContext')='object'
                                     AND json_extract(source_job.input_json,'$.assetId')=a.id
                               ))
                           ))
                      )
                )
                SELECT ranked.meeting_id,ranked.asset_id
                FROM ranked_sources ranked
                JOIN meetings m ON m.id=ranked.meeting_id
                WHERE ranked.source_rank=1
                  AND NOT EXISTS (
                      SELECT 1 FROM visual_context_runs run
                      WHERE run.meeting_id=ranked.meeting_id
                        AND run.screen_asset_id=ranked.asset_id
                        AND run.analyzer_version=?1
                        AND (run.status<>'running' OR run.updated_at_ms>?2)
                  )
                ORDER BY m.created_at_ms DESC,m.id DESC
                LIMIT 1"#,
            params![ANALYZER_VERSION, stale_before],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((meeting_id, visual_asset_id)) = candidate else {
        transaction.commit()?;
        return Ok(None);
    };
    let inserted = transaction.execute(
        "INSERT INTO visual_context_runs(
            meeting_id,screen_asset_id,analyzer_version,status,updated_at_ms
         ) VALUES (?1,?2,?3,'running',?4)
         ON CONFLICT(meeting_id) DO UPDATE SET
            screen_asset_id=excluded.screen_asset_id,
            analyzer_version=excluded.analyzer_version,
            status='running',vision_model=NULL,warning=NULL,
            updated_at_ms=excluded.updated_at_ms
         WHERE visual_context_runs.screen_asset_id<>excluded.screen_asset_id
            OR visual_context_runs.analyzer_version<>excluded.analyzer_version
            OR (visual_context_runs.status='running'
                AND visual_context_runs.updated_at_ms<=?5)",
        params![
            meeting_id,
            visual_asset_id,
            ANALYZER_VERSION,
            now,
            stale_before
        ],
    )?;
    transaction.commit()?;
    Ok((inserted == 1).then_some(meeting_id))
}

fn select_visual_source(
    connection: &Connection,
    meeting_id: &str,
) -> CoreResult<Option<VisualSource>> {
    connection
        .query_row(
            r#"SELECT a.id,a.relative_path,m.source_kind,a.kind
               FROM meetings m
               JOIN media_assets a ON a.meeting_id=m.id
               WHERE m.id=?1 AND m.status='ready'
                 AND (
                     (m.source_kind='recording' AND a.kind='screen')
                     OR
                     (m.source_kind='import'
                      AND EXISTS (
                          SELECT 1 FROM processing_jobs policy
                          WHERE policy.meeting_id=m.id
                            AND json_type(policy.input_json,'$.visualContext')='object'
                      )
                      AND (
                          a.kind='screen'
                          OR
                          (a.kind='video' AND EXISTS (
                              SELECT 1 FROM processing_jobs source_job
                              WHERE source_job.meeting_id=m.id
                                AND json_type(source_job.input_json,'$.visualContext')='object'
                                AND json_extract(source_job.input_json,'$.assetId')=a.id
                          ))
                      ))
                 )
               ORDER BY CASE WHEN a.kind='screen' THEN 0 ELSE 1 END,
                        a.created_at_ms DESC,a.id DESC
               LIMIT 1"#,
            [meeting_id],
            |row| {
                let asset_kind = row.get::<_, String>(3)?;
                Ok(VisualSource {
                    asset_id: row.get(0)?,
                    relative_path: row.get(1)?,
                    meeting_source_kind: row.get(2)?,
                    kind: if asset_kind == "screen" {
                        VisualSourceKind::Screen
                    } else {
                        VisualSourceKind::ImportedVideo
                    },
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

fn visual_config_for_source(
    meeting_source_kind: &str,
    recording_config_json: Option<&str>,
    import_job_input_json: Option<&str>,
) -> CoreResult<VisualEnrichmentConfig> {
    match meeting_source_kind {
        "recording" => {
            let config = recording_config_json
                .map(serde_json::from_str::<RecordingConfig>)
                .transpose()
                .map_err(|error| {
                    CoreError::InvalidInput(format!(
                        "recording visual configuration is invalid: {error}"
                    ))
                })?
                .unwrap_or_default();
            Ok(VisualEnrichmentConfig {
                enabled: config.capture_screen,
                auto_screenshots: config.auto_screenshots,
                visual_speaker_attribution: config.visual_speaker_attribution,
            })
        }
        "import" => {
            let Some(input_json) = import_job_input_json else {
                return Ok(VisualEnrichmentConfig::default());
            };
            let input: serde_json::Value = serde_json::from_str(input_json).map_err(|error| {
                CoreError::InvalidInput(format!("import visual-context policy is invalid: {error}"))
            })?;
            let Some(policy) = input.get("visualContext") else {
                return Ok(VisualEnrichmentConfig::default());
            };
            let policy: ImportVisualContextPolicy = serde_json::from_value(policy.clone())
                .map_err(|error| {
                    CoreError::InvalidInput(format!(
                        "import visual-context policy is invalid: {error}"
                    ))
                })?;
            Ok(VisualEnrichmentConfig {
                enabled: true,
                auto_screenshots: policy.auto_screenshots,
                visual_speaker_attribution: policy.visual_speaker_attribution,
            })
        }
        value => Err(CoreError::InvalidInput(format!(
            "meeting source kind {value} cannot provide visual context"
        ))),
    }
}

fn load_visual_config(
    connection: &Connection,
    meeting_id: &str,
    source: &VisualSource,
) -> CoreResult<VisualEnrichmentConfig> {
    match source.meeting_source_kind.as_str() {
        "recording" => {
            let config_json: Option<String> = connection
                .query_row(
                    "SELECT config_json FROM recording_sessions
                     WHERE meeting_id=?1 ORDER BY started_at_ms DESC,id DESC LIMIT 1",
                    [meeting_id],
                    |row| row.get(0),
                )
                .optional()?;
            visual_config_for_source("recording", config_json.as_deref(), None)
        }
        "import" => {
            let input_json: Option<String> = connection
                .query_row(
                    "SELECT input_json FROM processing_jobs
                     WHERE meeting_id=?1
                       AND json_type(input_json,'$.visualContext')='object'
                     ORDER BY created_at_ms DESC,id DESC LIMIT 1",
                    [meeting_id],
                    |row| row.get(0),
                )
                .optional()?;
            visual_config_for_source("import", None, input_json.as_deref())
        }
        value => visual_config_for_source(value, None, None),
    }
}

fn enrich_meeting_inner(
    core: &CoreService,
    worker: Option<&WorkerSupervisor>,
    meeting_id: &str,
) -> CoreResult<VisualContextReport> {
    let connection = core.database().connect()?;
    let Some(source) = select_visual_source(&connection, meeting_id)? else {
        return Ok(VisualContextReport::default());
    };
    let config = load_visual_config(&connection, meeting_id, &source)?;
    if !config.enabled || (!config.auto_screenshots && !config.visual_speaker_attribution) {
        upsert_run(core, meeting_id, &source.asset_id, "skipped", None, None)?;
        return Ok(VisualContextReport::default());
    }
    let completed: bool = connection.query_row(
        "SELECT count(*)>0 FROM visual_context_runs
             WHERE meeting_id=?1 AND screen_asset_id=?2 AND analyzer_version=?3
               AND status='complete'",
        params![meeting_id, source.asset_id, ANALYZER_VERSION],
        |row| row.get(0),
    )?;
    if completed {
        return Ok(VisualContextReport::default());
    }
    drop(connection);
    upsert_run(core, meeting_id, &source.asset_id, "running", None, None)?;

    let visual_path = core.layout().resolve_relative(&source.relative_path)?;
    let mut report = VisualContextReport::default();
    let video_stream_index = if source.kind == VisualSourceKind::ImportedVideo {
        match media::probe_visual_stream_index(core.layout(), &visual_path) {
            Ok(index) => Some(index),
            Err(error) => {
                let warning = format!(
                    "Imported media has no usable timed visual stream; visual context was skipped: {error}"
                );
                report.warnings.push(warning.clone());
                finish_run(
                    core,
                    meeting_id,
                    &source.asset_id,
                    "skipped",
                    None,
                    Some(&warning),
                )?;
                return Ok(report);
            }
        }
    } else {
        None
    };
    let turns = load_turns(core, meeting_id)?;

    if config.auto_screenshots {
        for moment in select_moments(&turns) {
            match persist_screenshot(core, meeting_id, &visual_path, video_stream_index, &moment) {
                Ok(()) => report.screenshots_created += 1,
                Err(error) => report.warnings.push(format!(
                    "Could not capture screen context at {} ms: {error}",
                    moment.at_ms
                )),
            }
        }
    }

    if config.visual_speaker_attribution {
        match analyze_speakers(
            core,
            worker,
            meeting_id,
            &visual_path,
            video_stream_index,
            &turns,
        ) {
            Ok((suggested, model, warnings)) => {
                report.visual_speakers_suggested = suggested;
                report.vision_model = model;
                report.warnings.extend(warnings);
            }
            Err(error) => report
                .warnings
                .push(format!("Visual speaker analysis was skipped: {error}")),
        }
    }

    let status = if report.warnings.is_empty() {
        "complete"
    } else if report.screenshots_created > 0 || report.visual_speakers_suggested > 0 {
        "partial"
    } else {
        "failed"
    };
    finish_run(
        core,
        meeting_id,
        &source.asset_id,
        status,
        report.vision_model.as_deref(),
        (!report.warnings.is_empty())
            .then(|| report.warnings.join("; "))
            .as_deref(),
    )?;
    Ok(report)
}

fn load_turns(core: &CoreService, meeting_id: &str) -> CoreResult<Vec<TurnCandidate>> {
    let connection = core.database().connect()?;
    let mut statement = connection.prepare(
        "SELECT id,speaker_id,start_ms,end_ms,COALESCE(edited_text,model_text),is_marked
         FROM transcript_turns WHERE meeting_id=?1 AND is_draft=0 ORDER BY start_ms,id",
    )?;
    let turns = statement
        .query_map([meeting_id], |row| {
            Ok(TurnCandidate {
                id: row.get(0)?,
                speaker_id: row.get(1)?,
                start_ms: row.get(2)?,
                end_ms: row.get(3)?,
                text: row.get(4)?,
                marked: row.get::<_, i64>(5)? != 0,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(turns)
}

fn select_moments(turns: &[TurnCandidate]) -> Vec<MomentCandidate> {
    let mut scored = turns
        .iter()
        .filter_map(score_moment)
        .collect::<Vec<MomentCandidate>>();
    scored.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.at_ms.cmp(&right.at_ms))
    });
    let mut selected = Vec::new();
    for candidate in scored {
        if selected.iter().any(|prior: &MomentCandidate| {
            (prior.at_ms - candidate.at_ms).abs() < MIN_SCREENSHOT_SPACING_MS
        }) {
            continue;
        }
        selected.push(candidate);
        if selected.len() == MAX_INLINE_SCREENSHOTS {
            break;
        }
    }
    selected.sort_by_key(|candidate| candidate.at_ms);
    selected
}

fn score_moment(turn: &TurnCandidate) -> Option<MomentCandidate> {
    let lower = turn.text.to_lowercase();
    let visual_cues = [
        "what i'm sharing",
        "what i am sharing",
        "on my screen",
        "sharing my screen",
        "as you can see",
        "take a look",
        "look at this",
        "shown here",
        "this slide",
        "this chart",
        "this dashboard",
        "this document",
        "this diagram",
    ];
    let visual_trigger = visual_cues
        .iter()
        .find(|phrase| lower.contains(**phrase))
        .map(|phrase| (*phrase).to_string());
    let importance_trigger = [
        "the key point",
        "important to note",
        "this is important",
        "critical risk",
        "we decided",
        "the decision is",
        "action item",
        "the deadline",
    ]
    .iter()
    .find(|phrase| lower.contains(**phrase))
    .map(|phrase| (*phrase).to_string());
    let visual_noun = [
        "screen",
        "slide",
        "chart",
        "dashboard",
        "document",
        "diagram",
        "mockup",
        "prototype",
        "report",
        "spreadsheet",
    ]
    .iter()
    .any(|word| lower.contains(word));
    let deictic = ["this", "that", "these", "here", "there"]
        .iter()
        .any(|word| {
            lower
                .split_whitespace()
                .any(|token| token.trim_matches(|c: char| !c.is_alphanumeric()) == *word)
        });
    let important_hits = [
        "decision",
        "approved",
        "metric",
        "result",
        "risk",
        "deadline",
        "timeline",
        "architecture",
        "budget",
        "action item",
    ]
    .iter()
    .filter(|word| lower.contains(**word))
    .count()
    .min(3) as u8;
    let has_number = lower
        .chars()
        .any(|character| character.is_ascii_digit() || character == '%');

    let mut score = important_hits;
    if visual_trigger.is_some() {
        score = score.saturating_add(6);
    } else if visual_noun && deictic {
        score = score.saturating_add(4);
    }
    if importance_trigger.is_some() {
        score = score.saturating_add(4);
    }
    if has_number && visual_noun {
        score = score.saturating_add(1);
    }
    if turn.marked {
        score = score.saturating_add(4);
    }
    if score < 4 {
        return None;
    }
    let duration = (turn.end_ms - turn.start_ms).max(0);
    let at_ms = (turn.start_ms + duration.saturating_mul(2) / 3).min(turn.end_ms);
    let shared = visual_trigger.is_some() || (visual_noun && deictic);
    let trigger = visual_trigger.or(importance_trigger);
    Some(MomentCandidate {
        turn: turn.clone(),
        at_ms,
        score,
        kind: if shared {
            "shared_content"
        } else {
            "important_moment"
        },
        reason: if shared {
            "The speaker referred to content visible on the shared screen.".into()
        } else if turn.marked {
            "This bookmarked moment has supporting screen context.".into()
        } else {
            "This turn combines an important topic with visible screen context.".into()
        },
        trigger,
    })
}

fn persist_screenshot(
    core: &CoreService,
    meeting_id: &str,
    visual_path: &Path,
    video_stream_index: Option<u32>,
    moment: &MomentCandidate,
) -> CoreResult<()> {
    let event_id = stable_id(&format!(
        "visual-context:{meeting_id}:{}:{}:{}",
        moment.turn.id, moment.kind, moment.at_ms
    ));
    let asset_id = stable_id(&format!("visual-context-asset:{event_id}"));
    let directory = core
        .layout()
        .media()
        .join(meeting_id)
        .join("screen-context");
    fs::create_dir_all(&directory)?;
    let destination = directory.join(format!("{event_id}.jpg"));
    extract_frame(
        core,
        visual_path,
        video_stream_index,
        moment.at_ms,
        &destination,
    )?;
    let metadata = fs::metadata(&destination)?;
    if metadata.len() == 0 || metadata.len() > MAX_FRAME_BYTES {
        return Err(CoreError::Media(
            "extracted screen context image has an invalid size".into(),
        ));
    }
    let sha256 = media::sha256_file(&destination)?;
    let relative = core.layout().relative_to_root(&destination)?;
    let now = now_ms();
    let mut connection = core.database().connect()?;
    let transaction = connection.transaction()?;
    let claim_is_active: bool = transaction.query_row(
        "SELECT count(*)>0
         FROM visual_context_runs run
         JOIN meetings meeting ON meeting.id=run.meeting_id
         WHERE run.meeting_id=?1 AND run.analyzer_version=?2
           AND run.status='running' AND meeting.status='ready'",
        params![meeting_id, ANALYZER_VERSION],
        |row| row.get(0),
    )?;
    if !claim_is_active {
        let _ = fs::remove_file(&destination);
        return Err(CoreError::Conflict(
            "visual context claim was invalidated before the snapshot could be saved".into(),
        ));
    }
    transaction.execute(
        "INSERT INTO media_assets(
            id,meeting_id,kind,display_name,relative_path,content_type,size_bytes,sha256,
            created_at_ms
         ) VALUES (?1,?2,'screen_snapshot',?3,?4,'image/jpeg',?5,?6,?7)
         ON CONFLICT(id) DO UPDATE SET
            display_name=excluded.display_name,relative_path=excluded.relative_path,
            content_type=excluded.content_type,size_bytes=excluded.size_bytes,
            sha256=excluded.sha256,created_at_ms=excluded.created_at_ms",
        params![
            asset_id,
            meeting_id,
            format!("Screen context at {} ms", moment.at_ms),
            relative,
            metadata.len() as i64,
            sha256,
            now
        ],
    )?;
    transaction.execute(
        "INSERT INTO visual_context_events(
            id,meeting_id,turn_id,kind,at_ms,screenshot_asset_id,reason,trigger_text,
            confidence,source,speaker_id,created_at_ms
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'high','transcript_heuristic',?9,?10)
         ON CONFLICT(id) DO UPDATE SET
            screenshot_asset_id=excluded.screenshot_asset_id,reason=excluded.reason,
            trigger_text=excluded.trigger_text,created_at_ms=excluded.created_at_ms",
        params![
            event_id,
            meeting_id,
            moment.turn.id,
            moment.kind,
            moment.at_ms,
            asset_id,
            moment.reason,
            moment.trigger,
            moment.turn.speaker_id,
            now
        ],
    )?;
    transaction.commit()?;
    Ok(())
}

fn analyze_speakers(
    core: &CoreService,
    worker: Option<&WorkerSupervisor>,
    meeting_id: &str,
    visual_path: &Path,
    video_stream_index: Option<u32>,
    turns: &[TurnCandidate],
) -> CoreResult<(usize, Option<String>, Vec<String>)> {
    let unknown = unknown_speakers(core, meeting_id)?;
    if unknown.is_empty() {
        return Ok((0, None, Vec::new()));
    }
    let mut by_speaker: BTreeMap<String, Vec<&TurnCandidate>> = BTreeMap::new();
    for turn in turns {
        if let Some(speaker_id) = turn.speaker_id.as_ref().filter(|id| unknown.contains(*id)) {
            by_speaker.entry(speaker_id.clone()).or_default().push(turn);
        }
    }
    let mut selected = Vec::new();
    for speaker_turns in by_speaker.values_mut() {
        speaker_turns.sort_by_key(|turn| std::cmp::Reverse(turn.end_ms - turn.start_ms));
        let mut chosen_times = Vec::new();
        for turn in speaker_turns.iter() {
            let at_ms = turn.start_ms + (turn.end_ms - turn.start_ms).max(0) / 2;
            if chosen_times
                .iter()
                .any(|prior: &i64| (*prior - at_ms).abs() < MIN_SCREENSHOT_SPACING_MS)
            {
                continue;
            }
            selected.push((*turn, at_ms));
            chosen_times.push(at_ms);
            if chosen_times.len() == 2 || selected.len() == MAX_VISION_FRAMES {
                break;
            }
        }
        if selected.len() == MAX_VISION_FRAMES {
            break;
        }
    }
    selected.sort_by_key(|(turn, _)| turn.start_ms);

    let mut records = Vec::new();
    let mut temporary_frames = TemporaryFrameFiles::default();
    for (frame_index, (turn, at_ms)) in selected.into_iter().enumerate() {
        let path = core.layout().temp().join(format!(
            "visual-speaker-{}-{}.jpg",
            stable_id(meeting_id),
            frame_index
        ));
        if let Err(error) = extract_frame(core, visual_path, video_stream_index, at_ms, &path) {
            log::warn!("could not extract visual speaker frame: {error}");
            continue;
        }
        temporary_frames.0.push(path.clone());
        let jpeg_bytes = fs::read(&path)?;
        if jpeg_bytes.is_empty() || jpeg_bytes.len() as u64 > MAX_FRAME_BYTES {
            let _ = fs::remove_file(&path);
            continue;
        }
        let Some(speaker_id) = turn.speaker_id.clone() else {
            let _ = fs::remove_file(&path);
            continue;
        };
        records.push(VisionFrameRecord {
            frame: VisualSpeakerFrame {
                frame_index,
                turn_id: turn.id.clone(),
                speaker_id,
                at_ms,
                jpeg_bytes,
            },
        });
    }
    if records.is_empty() {
        return Ok((
            0,
            None,
            vec!["No usable visual speaker frames were available.".into()],
        ));
    }
    let frames = records
        .iter()
        .map(|record| record.frame.clone())
        .collect::<Vec<_>>();
    if let Some(worker) = worker {
        if let Err(error) = worker.release_idle_performance_models_for_visual() {
            log::warn!(
                "could not release idle transcription models before local vision analysis: {error}"
            );
        }
    }
    let analysis = tauri::async_runtime::block_on(local_agent::analyze_visual_speakers(&frames));
    let Some(batch) = analysis.map_err(|error| CoreError::Worker(error.to_string()))? else {
        return Ok((
            0,
            None,
            vec![
                "No installed local vision-capable Ollama model was available for speaker cues."
                    .into(),
            ],
        ));
    };

    let observations_by_frame = batch
        .observations
        .iter()
        .map(|observation| (observation.frame_index, observation))
        .collect::<BTreeMap<_, _>>();
    let frames_by_index = frames
        .iter()
        .map(|frame| (frame.frame_index, frame))
        .collect::<BTreeMap<_, _>>();
    let mut grouped: BTreeMap<String, Vec<(&VisualSpeakerFrame, &VisualSpeakerObservation)>> =
        BTreeMap::new();
    for (index, observation) in observations_by_frame {
        let Some(frame) = frames_by_index.get(&index) else {
            continue;
        };
        grouped
            .entry(frame.speaker_id.clone())
            .or_default()
            .push((frame, observation));
    }

    let mut suggested = 0;
    for (speaker_id, evidence) in grouped {
        let mut names: BTreeMap<String, (String, BTreeSet<String>, Vec<usize>)> = BTreeMap::new();
        for (frame, observation) in &evidence {
            if observation.confidence != "high" {
                continue;
            }
            let Some(name) = observation
                .active_speaker_name
                .as_deref()
                .and_then(sanitize_speaker_name)
            else {
                continue;
            };
            let key = name.to_lowercase();
            let entry = names
                .entry(key)
                .or_insert_with(|| (name.clone(), BTreeSet::new(), Vec::new()));
            entry.1.insert(frame.turn_id.clone());
            entry.2.push(frame.frame_index);
        }
        let mut ranked = names.into_values().collect::<Vec<_>>();
        ranked.sort_by_key(|(_, turns, _)| std::cmp::Reverse(turns.len()));
        let Some((name, supporting_turns, supporting_frames)) = ranked.first() else {
            continue;
        };
        let runner_up = ranked.get(1).map(|(_, turns, _)| turns.len()).unwrap_or(0);
        if supporting_turns.len() < 2 || supporting_turns.len() <= runner_up {
            continue;
        }
        let changed = apply_visual_suggestion(core, meeting_id, &speaker_id, name)?;
        if !changed {
            continue;
        }
        suggested += 1;
        for frame_index in supporting_frames {
            let Some(frame) = frames_by_index.get(frame_index) else {
                continue;
            };
            let observation = evidence
                .iter()
                .find(|(_, candidate)| candidate.frame_index == *frame_index)
                .map(|(_, candidate)| *candidate);
            persist_speaker_evidence(
                core,
                meeting_id,
                frame,
                &speaker_id,
                name,
                observation.and_then(|value| value.meeting_system.as_deref()),
            )?;
        }
    }
    Ok((suggested, Some(batch.model), Vec::new()))
}

fn unknown_speakers(core: &CoreService, meeting_id: &str) -> CoreResult<BTreeSet<String>> {
    let connection = core.database().connect()?;
    let mut statement = connection.prepare(
        "SELECT id FROM meeting_speakers
         WHERE meeting_id=?1 AND match_state='unknown'
           AND attribution_source NOT IN ('user','voice_confirmed','visual_confirmed')",
    )?;
    let speakers = statement
        .query_map([meeting_id], |row| row.get::<_, String>(0))?
        .collect::<Result<BTreeSet<_>, _>>()?;
    Ok(speakers)
}

fn apply_visual_suggestion(
    core: &CoreService,
    meeting_id: &str,
    speaker_id: &str,
    name: &str,
) -> CoreResult<bool> {
    let connection = core.database().connect()?;
    Ok(connection.execute(
        "UPDATE meeting_speakers
         SET display_name=?1,match_state='review',needs_review=1,
             attribution_source='visual',attribution_confidence='review'
         WHERE id=?2 AND meeting_id=?3 AND match_state='unknown'
           AND attribution_source NOT IN ('user','voice_confirmed','visual_confirmed')
           AND EXISTS (
               SELECT 1 FROM visual_context_runs run
               WHERE run.meeting_id=?3 AND run.analyzer_version=?4
                 AND run.status='running'
           )
           AND EXISTS (
               SELECT 1 FROM meetings meeting
               WHERE meeting.id=?3 AND meeting.status='ready'
           )",
        params![name, speaker_id, meeting_id, ANALYZER_VERSION],
    )? == 1)
}

fn persist_speaker_evidence(
    core: &CoreService,
    meeting_id: &str,
    frame: &VisualSpeakerFrame,
    speaker_id: &str,
    suggested_name: &str,
    meeting_system: Option<&str>,
) -> CoreResult<()> {
    let event_id = stable_id(&format!(
        "visual-speaker:{meeting_id}:{}:{}:{}",
        frame.turn_id, speaker_id, frame.at_ms
    ));
    let connection = core.database().connect()?;
    connection.execute(
        "INSERT INTO visual_context_events(
            id,meeting_id,turn_id,kind,at_ms,reason,confidence,source,speaker_id,
            suggested_speaker_name,meeting_system,created_at_ms
         )
         SELECT ?1,?2,?3,'speaker_evidence',?4,?5,'review','local_vision',?6,?7,?8,?9
         WHERE EXISTS (
             SELECT 1 FROM visual_context_runs run
             WHERE run.meeting_id=?2 AND run.analyzer_version=?10
               AND run.status='running'
         )
           AND EXISTS (
             SELECT 1 FROM meetings meeting
             WHERE meeting.id=?2 AND meeting.status='ready'
         )
         ON CONFLICT(id) DO UPDATE SET
            reason=excluded.reason,suggested_speaker_name=excluded.suggested_speaker_name,
            meeting_system=excluded.meeting_system,created_at_ms=excluded.created_at_ms",
        params![
            event_id,
            meeting_id,
            frame.turn_id,
            frame.at_ms,
            "A local vision model found the same visible active-speaker label in distinct turns. Review before confirming.",
            speaker_id,
            suggested_name,
            meeting_system,
            now_ms(),
            ANALYZER_VERSION
        ],
    )?;
    Ok(())
}

fn extract_frame(
    core: &CoreService,
    visual_path: &Path,
    video_stream_index: Option<u32>,
    at_ms: i64,
    destination: &Path,
) -> CoreResult<()> {
    if at_ms < 0 {
        return Err(CoreError::InvalidInput(
            "screen context timestamp must not be negative".into(),
        ));
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let partial = PathBuf::from(format!("{}.partial", destination.to_string_lossy()));
    let _ = fs::remove_file(&partial);
    let seek_seconds = format!("{:.3}", at_ms as f64 / 1000.0);
    let visual_path = visual_path.to_string_lossy().into_owned();
    let partial_path = partial.to_string_lossy().into_owned();
    let mut command = Command::new(media_tools::resolve(core.layout(), MediaTool::Ffmpeg)?);
    command.args([
        "-y",
        "-v",
        "error",
        "-ss",
        seek_seconds.as_str(),
        "-i",
        visual_path.as_str(),
    ]);
    let stream_map = video_stream_index.map(|index| format!("0:{index}"));
    if let Some(stream_map) = stream_map.as_deref() {
        command.args(["-map", stream_map]);
    }
    command.args([
        "-frames:v",
        "1",
        "-vf",
        "scale=min(1440\\,iw):-2",
        "-c:v",
        "mjpeg",
        "-q:v",
        "3",
        "-f",
        "image2",
        partial_path.as_str(),
    ]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let (status, stderr) = match run_bounded_frame_command(command) {
        Ok(output) => output,
        Err(error) => {
            let _ = fs::remove_file(&partial);
            return Err(error);
        }
    };
    if !status.success() {
        let _ = fs::remove_file(&partial);
        return Err(CoreError::Media(format!(
            "ffmpeg frame extraction failed: {}",
            String::from_utf8_lossy(&stderr).trim()
        )));
    }
    let _ = fs::remove_file(destination);
    fs::rename(&partial, destination)?;
    Ok(())
}

fn run_bounded_frame_command(mut command: Command) -> CoreResult<(ExitStatus, Vec<u8>)> {
    let mut child = command
        .spawn()
        .map_err(|error| CoreError::Media(format!("ffmpeg frame extraction failed: {error}")))?;
    let stderr = child.stderr.take().ok_or_else(|| {
        CoreError::Media("ffmpeg frame extraction did not expose its error stream".into())
    })?;
    let stderr_reader = thread::spawn(move || {
        let mut stream = stderr;
        let mut captured = Vec::new();
        let mut buffer = [0_u8; 4 * 1024];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let remaining = MAX_CAPTURED_FFMPEG_ERROR_BYTES.saturating_sub(captured.len());
                    captured.extend_from_slice(&buffer[..read.min(remaining)]);
                }
            }
        }
        captured
    });
    let deadline = Instant::now() + FRAME_EXTRACTION_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stderr = stderr_reader.join().unwrap_or_default();
                return Ok((status, stderr));
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stderr_reader.join();
                return Err(CoreError::Media(format!(
                    "ffmpeg frame extraction exceeded its {} second deadline",
                    FRAME_EXTRACTION_TIMEOUT.as_secs()
                )));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stderr_reader.join();
                return Err(CoreError::Media(format!(
                    "ffmpeg frame extraction status failed: {error}"
                )));
            }
        }
    }
}

fn upsert_run(
    core: &CoreService,
    meeting_id: &str,
    screen_asset_id: &str,
    status: &str,
    vision_model: Option<&str>,
    warning: Option<&str>,
) -> CoreResult<()> {
    let connection = core.database().connect()?;
    connection.execute(
        "INSERT INTO visual_context_runs(
            meeting_id,screen_asset_id,analyzer_version,status,vision_model,warning,updated_at_ms
         ) VALUES (?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(meeting_id) DO UPDATE SET
            screen_asset_id=excluded.screen_asset_id,
            analyzer_version=excluded.analyzer_version,status=excluded.status,
            vision_model=excluded.vision_model,warning=excluded.warning,
            updated_at_ms=excluded.updated_at_ms",
        params![
            meeting_id,
            screen_asset_id,
            ANALYZER_VERSION,
            status,
            vision_model,
            warning,
            now_ms()
        ],
    )?;
    Ok(())
}

fn finish_run(
    core: &CoreService,
    meeting_id: &str,
    screen_asset_id: &str,
    status: &str,
    vision_model: Option<&str>,
    warning: Option<&str>,
) -> CoreResult<()> {
    let connection = core.database().connect()?;
    connection.execute(
        "UPDATE visual_context_runs
         SET status=?1,vision_model=?2,warning=?3,updated_at_ms=?4
         WHERE meeting_id=?5 AND screen_asset_id=?6 AND analyzer_version=?7
           AND status='running'",
        params![
            status,
            vision_model,
            warning,
            now_ms(),
            meeting_id,
            screen_asset_id,
            ANALYZER_VERSION
        ],
    )?;
    Ok(())
}

fn mark_run_failed(core: &CoreService, meeting_id: &str, warning: &str) -> CoreResult<()> {
    let connection = core.database().connect()?;
    connection.execute(
        "UPDATE visual_context_runs SET status='failed',warning=?1,updated_at_ms=?2
         WHERE meeting_id=?3",
        params![warning, now_ms(), meeting_id],
    )?;
    Ok(())
}

fn sanitize_speaker_name(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.len() < 2
        || trimmed.len() > 120
        || trimmed
            .chars()
            .any(|character| matches!(character, '\n' | '\r' | '\t'))
        || matches!(
            trimmed.to_ascii_lowercase().as_str(),
            "unknown" | "unknown speaker" | "speaker" | "none" | "n/a"
        )
    {
        return None;
    }
    Some(trimmed.to_string())
}

fn stable_id(value: &str) -> String {
    Uuid::new_v5(&Uuid::NAMESPACE_URL, value.as_bytes()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_screen_meeting(
        core: &CoreService,
        status: &str,
        created_at_ms: i64,
        existing_run: bool,
    ) -> (String, String) {
        let meeting_id = Uuid::now_v7().to_string();
        let session_id = Uuid::now_v7().to_string();
        let screen_asset_id = Uuid::now_v7().to_string();
        let screen_path = core
            .layout()
            .recordings()
            .join(&meeting_id)
            .join("screen.mp4");
        fs::create_dir_all(screen_path.parent().unwrap()).unwrap();
        fs::write(&screen_path, b"screen").unwrap();
        let screen_relative = core.layout().relative_to_root(&screen_path).unwrap();
        let config = RecordingConfig {
            capture_screen: true,
            auto_screenshots: false,
            visual_speaker_attribution: false,
            ..Default::default()
        };
        let connection = core.database().connect().unwrap();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Screen meeting','recording',?2,?3)",
                params![meeting_id, status, created_at_ms],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO recording_sessions(
                    id,meeting_id,state,config_json,manifest_relative_path,started_at_ms
                 ) VALUES (?1,?2,'stopped',?3,?4,?5)",
                params![
                    session_id,
                    meeting_id,
                    serde_json::to_string(&config).unwrap(),
                    format!("library/recordings/{meeting_id}/manifest.jsonl"),
                    created_at_ms
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO media_assets(
                    id,meeting_id,kind,display_name,relative_path,content_type,size_bytes,
                    sha256,duration_ms,codec,created_at_ms
                 ) VALUES (?1,?2,'screen','screen.mp4',?3,'video/mp4',6,'hash',1000,
                           'mpeg4',?4)",
                params![screen_asset_id, meeting_id, screen_relative, created_at_ms],
            )
            .unwrap();
        if existing_run {
            connection
                .execute(
                    "INSERT INTO visual_context_runs(
                        meeting_id,screen_asset_id,analyzer_version,status,updated_at_ms
                     ) VALUES (?1,?2,?3,'complete',?4)",
                    params![meeting_id, screen_asset_id, ANALYZER_VERSION, created_at_ms],
                )
                .unwrap();
        }
        (meeting_id, screen_asset_id)
    }

    fn insert_import_meeting(
        core: &CoreService,
        status: &str,
        created_at_ms: i64,
        media_kind: &str,
        visual_policy: bool,
    ) -> (String, String, PathBuf) {
        let meeting_id = Uuid::now_v7().to_string();
        let asset_id = Uuid::now_v7().to_string();
        let job_id = Uuid::now_v7().to_string();
        let extension = if media_kind == "video" { "mp4" } else { "m4a" };
        let media_path = core
            .layout()
            .media()
            .join(&meeting_id)
            .join(&asset_id)
            .join(format!("original.{extension}"));
        fs::create_dir_all(media_path.parent().unwrap()).unwrap();
        fs::write(&media_path, b"managed import").unwrap();
        let relative = core.layout().relative_to_root(&media_path).unwrap();
        let input = if visual_policy {
            serde_json::json!({
                "assetId": asset_id,
                "sourceKind": "managed_import",
                "relativePath": relative,
                "visualContext": ImportVisualContextPolicy::default()
            })
        } else {
            serde_json::json!({
                "assetId": asset_id,
                "sourceKind": "managed_import",
                "relativePath": relative
            })
        };
        let connection = core.database().connect().unwrap();
        connection
            .execute(
                "INSERT INTO meetings(id,title,source_kind,status,created_at_ms)
                 VALUES (?1,'Imported meeting','import',?2,?3)",
                params![meeting_id, status, created_at_ms],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO media_assets(
                    id,meeting_id,kind,display_name,relative_path,content_type,size_bytes,
                    sha256,duration_ms,codec,created_at_ms
                 ) VALUES (?1,?2,?3,?4,?5,?6,14,'hash',3000,?7,?8)",
                params![
                    asset_id,
                    meeting_id,
                    media_kind,
                    format!("original.{extension}"),
                    relative,
                    if media_kind == "video" {
                        "video/mp4"
                    } else {
                        "audio/mp4"
                    },
                    "aac",
                    created_at_ms
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO processing_jobs(
                    id,meeting_id,stage,status,progress,input_json,created_at_ms,updated_at_ms
                 ) VALUES (?1,?2,'finalize','completed',1,?3,?4,?4)",
                params![job_id, meeting_id, input.to_string(), created_at_ms],
            )
            .unwrap();
        (meeting_id, asset_id, media_path)
    }

    fn create_synthetic_import_video(core: &CoreService, destination: &Path) -> bool {
        let Ok(ffmpeg) = media_tools::resolve(core.layout(), MediaTool::Ffmpeg) else {
            eprintln!("skipping FFmpeg-backed screenshot extraction test: FFmpeg is unavailable");
            return false;
        };
        let mut command = Command::new(ffmpeg);
        command.args([
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=blue:s=320x180:r=5:d=3",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=3",
            "-shortest",
            "-c:v",
            "mpeg4",
            "-q:v",
            "5",
            "-c:a",
            "aac",
            "-f",
            "mp4",
        ]);
        command.arg(destination);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "could not create synthetic import video: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }

    fn turn(text: &str) -> TurnCandidate {
        TurnCandidate {
            id: "turn".into(),
            speaker_id: Some("speaker".into()),
            start_ms: 1_000,
            end_ms: 7_000,
            text: text.into(),
            marked: false,
        }
    }

    #[test]
    fn visual_deixis_selects_screen_context() {
        let candidate = score_moment(&turn(
            "As you can see on my screen, this chart shows activation up 12%.",
        ))
        .unwrap();
        assert_eq!(candidate.kind, "shared_content");
        assert!(candidate.score >= 6);
        assert_eq!(candidate.at_ms, 5_000);
    }

    #[test]
    fn ordinary_conversation_does_not_create_a_screenshot() {
        assert!(score_moment(&turn("Thanks everyone, let's meet again next week.")).is_none());
    }

    #[test]
    fn explicit_important_language_selects_supporting_context() {
        let candidate =
            score_moment(&turn("The key point is that the deadline moves to Friday.")).unwrap();
        assert_eq!(candidate.kind, "important_moment");
        assert_eq!(candidate.trigger.as_deref(), Some("the key point"));
    }

    #[test]
    fn marked_turn_is_eligible_without_visual_language() {
        let mut value = turn("This is the final decision.");
        value.marked = true;
        assert_eq!(score_moment(&value).unwrap().kind, "important_moment");
    }

    #[test]
    fn rejects_generic_or_multiline_visual_names() {
        assert!(sanitize_speaker_name("Unknown speaker").is_none());
        assert!(sanitize_speaker_name("Ignore\nAlex").is_none());
        assert_eq!(
            sanitize_speaker_name("Maya Chen").as_deref(),
            Some("Maya Chen")
        );
    }

    #[test]
    fn imported_video_policy_defaults_on_without_changing_recording_consent() {
        let import_input = serde_json::json!({
            "assetId": "video",
            "visualContext": ImportVisualContextPolicy::default()
        })
        .to_string();
        let imported = visual_config_for_source("import", None, Some(&import_input)).unwrap();
        assert!(imported.enabled);
        assert!(imported.auto_screenshots);
        assert!(imported.visual_speaker_attribution);
        assert_eq!(
            visual_config_for_source("import", None, None).unwrap(),
            VisualEnrichmentConfig::default()
        );

        let recording = RecordingConfig {
            capture_screen: false,
            auto_screenshots: true,
            visual_speaker_attribution: true,
            ..Default::default()
        };
        let recording_json = serde_json::to_string(&recording).unwrap();
        let recording = visual_config_for_source("recording", Some(&recording_json), None).unwrap();
        assert!(!recording.enabled);
        assert!(recording.auto_screenshots);
        assert!(recording.visual_speaker_attribution);
    }

    #[test]
    fn visual_source_prefers_screen_then_opted_in_original_import_video() {
        let temp = tempfile::tempdir().unwrap();
        let core = CoreService::open(temp.path()).unwrap();
        let (meeting_id, video_asset_id, _) =
            insert_import_meeting(&core, "ready", 10, "video", true);
        let connection = core.database().connect().unwrap();

        let source = select_visual_source(&connection, &meeting_id)
            .unwrap()
            .unwrap();
        assert_eq!(source.asset_id, video_asset_id);
        assert_eq!(source.kind, VisualSourceKind::ImportedVideo);

        let screen_id = Uuid::now_v7().to_string();
        let screen_path = core
            .layout()
            .recordings()
            .join(&meeting_id)
            .join("screen.mp4");
        fs::create_dir_all(screen_path.parent().unwrap()).unwrap();
        fs::write(&screen_path, b"screen").unwrap();
        let screen_relative = core.layout().relative_to_root(&screen_path).unwrap();
        connection
            .execute(
                "INSERT INTO media_assets(
                    id,meeting_id,kind,display_name,relative_path,content_type,size_bytes,
                    sha256,created_at_ms
                 ) VALUES (?1,?2,'screen','screen.mp4',?3,'video/mp4',6,'hash',5)",
                params![screen_id, meeting_id, screen_relative],
            )
            .unwrap();

        let source = select_visual_source(&connection, &meeting_id)
            .unwrap()
            .unwrap();
        assert_eq!(source.asset_id, screen_id);
        assert_eq!(source.kind, VisualSourceKind::Screen);
    }

    #[test]
    fn import_backfill_requires_durable_policy_and_real_video_kind() {
        let temp = tempfile::tempdir().unwrap();
        let core = CoreService::open(temp.path()).unwrap();
        let _ = insert_import_meeting(&core, "ready", 30, "video", false);
        let _ = insert_import_meeting(&core, "ready", 20, "audio", true);
        let (eligible_meeting, eligible_asset, _) =
            insert_import_meeting(&core, "ready", 10, "video", true);

        assert_eq!(
            claim_next_missing(&core).unwrap().as_deref(),
            Some(eligible_meeting.as_str())
        );
        assert!(claim_next_missing(&core).unwrap().is_none());

        let connection = core.database().connect().unwrap();
        let claimed_asset: String = connection
            .query_row(
                "SELECT screen_asset_id FROM visual_context_runs WHERE meeting_id=?1",
                [&eligible_meeting],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(claimed_asset, eligible_asset);
    }

    #[test]
    fn imported_video_extracts_inline_screenshot_and_completed_run_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let core = CoreService::open(temp.path()).unwrap();
        let (meeting_id, _, media_path) =
            insert_import_meeting(&core, "ready", now_ms(), "video", true);
        if !create_synthetic_import_video(&core, &media_path) {
            return;
        }
        let turn_id = Uuid::now_v7().to_string();
        let connection = core.database().connect().unwrap();
        connection
            .execute(
                "INSERT INTO transcript_turns(
                    id,meeting_id,start_ms,end_ms,model_text,created_at_ms,updated_at_ms
                 ) VALUES (?1,?2,250,1750,?3,?4,?4)",
                params![
                    turn_id,
                    meeting_id,
                    "As you can see on my screen, this chart shows the key result.",
                    now_ms()
                ],
            )
            .unwrap();
        drop(connection);

        let first = enrich_meeting(&core, &meeting_id).unwrap();
        assert_eq!(first.screenshots_created, 1);
        assert_eq!(first.visual_speakers_suggested, 0);
        assert!(first.warnings.is_empty());
        let second = enrich_meeting(&core, &meeting_id).unwrap();
        assert_eq!(second.screenshots_created, 0);

        let connection = core.database().connect().unwrap();
        let (assets, events, runs, status): (i64, i64, i64, String) = connection
            .query_row(
                "SELECT
                    (SELECT count(*) FROM media_assets
                     WHERE meeting_id=?1 AND kind='screen_snapshot'),
                    (SELECT count(*) FROM visual_context_events WHERE meeting_id=?1),
                    (SELECT count(*) FROM visual_context_runs WHERE meeting_id=?1),
                    (SELECT status FROM visual_context_runs WHERE meeting_id=?1)",
                [&meeting_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!((assets, events, runs), (1, 1, 1));
        assert_eq!(status, "complete");
        let screenshot_relative: String = connection
            .query_row(
                "SELECT relative_path FROM media_assets
                 WHERE meeting_id=?1 AND kind='screen_snapshot'",
                [&meeting_id],
                |row| row.get(0),
            )
            .unwrap();
        let screenshot = core
            .layout()
            .resolve_relative(&screenshot_relative)
            .unwrap();
        let bytes = fs::read(screenshot).unwrap();
        assert!(bytes.len() > 2);
        assert_eq!(&bytes[..2], &[0xff, 0xd8]);
    }

    #[test]
    fn backfill_claims_newest_ready_screen_meetings_exactly_once() {
        let temp = tempfile::tempdir().unwrap();
        let core = CoreService::open(temp.path()).unwrap();
        let (older, _) = insert_screen_meeting(&core, "ready", 10, false);
        let (newer, _) = insert_screen_meeting(&core, "ready", 20, false);
        let _ = insert_screen_meeting(&core, "processing", 30, false);
        let _ = insert_screen_meeting(&core, "ready", 40, true);

        assert_eq!(
            claim_next_missing(&core).unwrap().as_deref(),
            Some(newer.as_str())
        );
        assert_eq!(
            claim_next_missing(&core).unwrap().as_deref(),
            Some(older.as_str())
        );
        assert!(claim_next_missing(&core).unwrap().is_none());

        let connection = core.database().connect().unwrap();
        let duplicate_runs: i64 = connection
            .query_row(
                "SELECT count(*) FROM (
                    SELECT meeting_id,count(*) AS runs FROM visual_context_runs
                    GROUP BY meeting_id HAVING runs>1
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(duplicate_runs, 0);
    }

    #[test]
    fn backfill_reclaims_only_expired_running_visual_claims() {
        let temp = tempfile::tempdir().unwrap();
        let core = CoreService::open(temp.path()).unwrap();
        let (stale_meeting, stale_asset) = insert_screen_meeting(&core, "ready", 10, false);
        let (fresh_meeting, fresh_asset) = insert_screen_meeting(&core, "ready", 20, false);
        let now = now_ms();
        let connection = core.database().connect().unwrap();
        for (meeting_id, asset_id, updated_at_ms) in [
            (
                &stale_meeting,
                &stale_asset,
                now.saturating_sub(VISUAL_CLAIM_LEASE_MS + 1),
            ),
            (&fresh_meeting, &fresh_asset, now),
        ] {
            connection
                .execute(
                    "INSERT INTO visual_context_runs(
                        meeting_id,screen_asset_id,analyzer_version,status,updated_at_ms
                     ) VALUES (?1,?2,?3,'running',?4)",
                    params![meeting_id, asset_id, ANALYZER_VERSION, updated_at_ms],
                )
                .unwrap();
        }
        drop(connection);

        assert_eq!(
            claim_next_missing(&core).unwrap().as_deref(),
            Some(stale_meeting.as_str())
        );
        assert!(claim_next_missing(&core).unwrap().is_none());

        let connection = core.database().connect().unwrap();
        let reclaimed_at: i64 = connection
            .query_row(
                "SELECT updated_at_ms FROM visual_context_runs WHERE meeting_id=?1",
                [&stale_meeting],
                |row| row.get(0),
            )
            .unwrap();
        assert!(reclaimed_at >= now);
    }

    #[test]
    fn backfill_persists_a_terminal_run_and_does_not_requeue_it() {
        let temp = tempfile::tempdir().unwrap();
        let core = CoreService::open(temp.path()).unwrap();
        let (meeting_id, screen_asset_id) = insert_screen_meeting(&core, "ready", now_ms(), false);

        let backfill = backfill_next_missing(&core).unwrap().unwrap();

        assert_eq!(backfill.meeting_id, meeting_id);
        assert!(!backfill.report.created_context());
        assert!(backfill_next_missing(&core).unwrap().is_none());
        let connection = core.database().connect().unwrap();
        let run: (String, String, String, i64) = connection
            .query_row(
                "SELECT screen_asset_id,analyzer_version,status,count(*)
                 FROM visual_context_runs WHERE meeting_id=?1",
                [&meeting_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(run.0, screen_asset_id);
        assert_eq!(run.1, ANALYZER_VERSION);
        assert_eq!(run.2, "skipped");
        assert_eq!(run.3, 1);
    }

    #[test]
    fn report_marks_only_persisted_context_as_created() {
        let mut report = VisualContextReport::default();
        assert!(!report.created_context());
        report.warnings.push("vision unavailable".into());
        assert!(!report.created_context());
        report.screenshots_created = 1;
        assert!(report.created_context());
    }
}
