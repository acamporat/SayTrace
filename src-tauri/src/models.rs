use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MeetingStatus {
    Importing,
    Recording,
    Processing,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meeting {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub source_kind: String,
    pub source_type: String,
    pub status: MeetingStatus,
    pub created_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub ended_at_ms: Option<i64>,
    pub duration_ms: i64,
    pub language: String,
    pub has_user_edits: bool,
    pub speaker_count: u32,
    pub asset_count: u32,
    pub asset_id: Option<String>,
    pub needs_review: bool,
    pub recovery_warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaAsset {
    pub id: String,
    pub meeting_id: String,
    pub kind: String,
    pub display_name: String,
    pub content_type: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub duration_ms: Option<i64>,
    pub codec: Option<String>,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u16>,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SpeakerMatchState {
    Matched,
    Review,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingSpeaker {
    pub id: String,
    pub meeting_id: String,
    pub label: String,
    pub display_name: String,
    pub initials: String,
    pub profile_id: Option<String>,
    pub state: SpeakerMatchState,
    pub match_state: SpeakerMatchState,
    pub needs_review: bool,
    pub color: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameSpeakerResult {
    pub speaker: MeetingSpeaker,
    pub profile: Option<VoiceProfile>,
    pub profile_created: bool,
    pub sample_saved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WordTiming {
    pub id: String,
    pub turn_id: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
    pub confidence: Option<f32>,
    pub speaker_id: Option<String>,
    pub is_overlap: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptTurn {
    pub id: String,
    pub meeting_id: String,
    pub speaker_id: Option<String>,
    pub start_ms: i64,
    pub end_ms: i64,
    pub model_text: String,
    pub edited_text: Option<String>,
    pub text: String,
    pub revision: u32,
    pub needs_review: bool,
    pub is_draft: bool,
    pub is_marked: bool,
    pub words: Vec<WordTiming>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingMarker {
    pub id: String,
    pub meeting_id: String,
    pub at_ms: i64,
    pub label: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingDetail {
    pub meeting: Meeting,
    pub assets: Vec<MediaAsset>,
    pub speakers: Vec<MeetingSpeaker>,
    pub turns: Vec<TranscriptTurn>,
    pub markers: Vec<RecordingMarker>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceProfile {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub initials: String,
    pub color: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub last_used_at: String,
    pub sample_count: u32,
    pub sample_duration_ms: i64,
    pub total_clean_duration_ms: i64,
    pub ready_for_matching: bool,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessingJob {
    pub id: String,
    pub meeting_id: String,
    pub stage: String,
    pub status: String,
    pub state: String,
    pub progress: f32,
    pub attempts: u32,
    pub max_attempts: u32,
    pub checkpoint_ms: Option<i64>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportMediaRequest {
    pub source_path: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportMediaResult {
    pub meeting: Meeting,
    pub asset: MediaAsset,
    pub job: ProcessingJob,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingListRequest {
    pub query: Option<String>,
    pub status: Option<MeetingStatus>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

impl Default for MeetingListRequest {
    fn default() -> Self {
        Self {
            query: None,
            status: None,
            limit: Some(100),
            offset: Some(0),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptSearchRequest {
    pub meeting_id: Option<String>,
    pub query: String,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptSearchHit {
    pub meeting_id: String,
    pub turn_id: String,
    pub start_ms: i64,
    pub speaker_name: Option<String>,
    pub text: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateTurnRequest {
    pub turn_id: String,
    pub text: String,
    pub expected_revision: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameSpeakerRequest {
    pub speaker_id: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeSpeakersRequest {
    pub source_speaker_id: String,
    pub target_speaker_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSpeakerReviewRequest {
    pub speaker_id: String,
    pub needs_review: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateVoiceProfileRequest {
    pub display_name: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct ConfirmVoiceSampleRequest {
    pub profile_id: String,
    pub meeting_id: String,
    pub speaker_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingConfig {
    pub capture_microphone: bool,
    pub capture_system_audio: bool,
    pub microphone_device_id: Option<String>,
    pub loopback_device_id: Option<String>,
    pub live_captions: bool,
    #[serde(default = "default_true")]
    pub microphone_is_personal: bool,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            capture_microphone: true,
            capture_system_audio: true,
            microphone_device_id: None,
            loopback_device_id: None,
            live_captions: true,
            microphone_is_personal: true,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingState {
    Idle,
    Starting,
    Recording,
    Paused,
    Finalizing,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingSession {
    pub id: String,
    pub meeting_id: String,
    pub state: RecordingState,
    pub elapsed_ms: i64,
    pub started_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingStatus {
    pub state: RecordingState,
    pub session_id: Option<String>,
    pub meeting_id: Option<String>,
    pub elapsed_ms: i64,
    pub microphone_active: bool,
    pub system_audio_active: bool,
    pub microphone_level: f32,
    pub system_audio_level: f32,
    pub dropped_capture_packets: u64,
    pub dropped_caption_chunks: u64,
    pub warning: Option<String>,
}

impl Default for RecordingStatus {
    fn default() -> Self {
        Self {
            state: RecordingState::Idle,
            session_id: None,
            meeting_id: None,
            elapsed_ms: 0,
            microphone_active: false,
            system_audio_active: false,
            microphone_level: 0.0,
            system_audio_level: 0.0,
            dropped_capture_packets: 0,
            dropped_caption_chunks: 0,
            warning: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddMarkerRequest {
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub is_default: bool,
    pub is_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDeviceList {
    pub microphones: Vec<AudioDevice>,
    pub outputs: Vec<AudioDevice>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Txt,
    #[serde(rename = "md")]
    Markdown,
    Srt,
    #[serde(rename = "vtt")]
    WebVtt,
    Json,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportTranscriptRequest {
    pub meeting_id: String,
    pub format: ExportFormat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportResult {
    pub artifact_id: String,
    pub file_name: String,
    pub format: ExportFormat,
    pub size_bytes: u64,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupRequest {
    pub include_media: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupResult {
    pub backup_id: String,
    pub file_count: u64,
    pub total_bytes: u64,
    pub includes_media: bool,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetDescriptor {
    pub id: String,
    pub display_name: String,
    pub content_type: String,
    pub size_bytes: u64,
    pub duration_ms: Option<i64>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetChunkRequest {
    pub asset_id: String,
    pub offset: u64,
    pub length: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetChunk {
    pub asset_id: String,
    pub offset: u64,
    pub bytes: Vec<u8>,
    pub end_of_file: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryStats {
    pub meeting_count: u64,
    pub recording_count: u64,
    pub processing_count: u64,
    pub storage_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerStatus {
    pub state: String,
    pub protocol_version: u32,
    pub pipeline_version: String,
    pub process_id: Option<u32>,
    pub last_heartbeat_ms: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelPackStatus {
    pub runtime: String,
    pub live_model: String,
    pub final_model: String,
    pub diarization_model: String,
    pub device: String,
    pub disk_required_gb: f32,
    pub disk_available_gb: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppStatus {
    pub app_version: String,
    pub schema_version: u32,
    pub first_run: bool,
    pub model_ready: bool,
    pub offline_ready: bool,
    pub active_recording: RecordingStatus,
    pub worker: WorkerStatus,
    pub model_revisions: BTreeMap<String, String>,
    pub capabilities: BTreeMap<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_recording_config_defaults_to_personal_microphone() {
        let config: RecordingConfig = serde_json::from_value(serde_json::json!({
            "captureMicrophone":true,
            "captureSystemAudio":true,
            "microphoneDeviceId":null,
            "loopbackDeviceId":null,
            "liveCaptions":true
        }))
        .unwrap();
        assert!(config.microphone_is_personal);
    }
}
