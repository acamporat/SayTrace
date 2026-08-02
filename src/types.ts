export type AppView =
  | { kind: "library" }
  | { kind: "transcript"; meetingId: string }
  | { kind: "recording"; meetingId: string }
  | { kind: "profiles" }
  | { kind: "settings" }
  | { kind: "setup" };

export type SpeakerState = "Matched" | "Review" | "Unknown";
export type JobStage =
  | "ingest"
  | "normalize"
  | "transcribe"
  | "align"
  | "diarize"
  | "identify"
  | "index"
  | "finalize";

export interface Meeting {
  id: string;
  title: string;
  createdAt: string;
  durationMs: number;
  status: "ready" | "processing" | "recording" | "failed" | "importing";
  sourceType: "import" | "recording";
  sourceKind?: "import" | "recording";
  speakerCount: number;
  assetId?: string;
}

export interface MediaAsset {
  id: string;
  meetingId: string;
  displayName: string;
  mediaKind?: "audio" | "video";
  kind?: string;
  contentType?: string;
  sizeBytes?: number;
  durationMs?: number;
}

export interface RecordingConfig {
  microphoneDeviceId: string;
  loopbackDeviceId: string;
  captureMicrophone: boolean;
  captureSystemAudio: boolean;
  liveCaptions: boolean;
  microphoneIsPersonal: boolean;
}

export interface RecordingSession {
  id: string;
  meetingId: string;
  state: "recording" | "paused" | "finalizing" | "stopped";
  elapsedMs: number;
  startedAt: string;
}

export interface WordTiming {
  id: string;
  text: string;
  startMs: number;
  endMs: number;
  confidence?: number;
}

export interface TranscriptTurn {
  id: string;
  speakerId: string | null;
  startMs: number;
  endMs: number;
  modelText: string;
  editedText?: string;
  isDraft?: boolean;
  isMarked?: boolean;
  needsReview?: boolean;
  revision?: number;
  words?: WordTiming[];
}

export interface MeetingSpeaker {
  id: string;
  label?: string;
  displayName: string;
  color: string;
  initials: string;
  state: SpeakerState;
  profileId?: string;
}

export interface VoiceProfile {
  id: string;
  name: string;
  initials: string;
  color: string;
  sampleDurationMs: number;
  sampleCount: number;
  lastUsedAt: string;
  status: "ready" | "needs_samples";
}

export interface RenameSpeakerResult {
  speaker: MeetingSpeaker;
  profile?: VoiceProfile;
  profileCreated: boolean;
  sampleSaved: boolean;
}

export interface ProcessingJob {
  id: string;
  meetingId: string;
  stage: JobStage;
  progress: number;
  state:
    | "queued"
    | "running"
    | "retry_wait"
    | "cancel_requested"
    | "interrupted"
    | "completed"
    | "failed"
    | "cancelled";
  errorCode?: string;
  errorMessage?: string;
}

export interface AudioDevice {
  id: string;
  name: string;
  kind: "input" | "output";
  isDefault: boolean;
}

export interface Marker {
  id: string;
  meetingId: string;
  atMs: number;
  label: string;
}

export interface MeetingDetail {
  meeting: Meeting;
  assets: MediaAsset[];
  speakers: MeetingSpeaker[];
  turns: TranscriptTurn[];
  markers: Marker[];
}

export interface ModelPackStatus {
  runtime: "ready" | "missing" | "checking";
  liveModel: "ready" | "missing" | "downloading";
  finalModel: "ready" | "missing" | "downloading";
  diarizationModel: "ready" | "missing" | "downloading";
  device: string;
  diskRequiredGb: number;
  diskAvailableGb: number;
}

export type ModelSetupKey =
  | "live_asr_en"
  | "final_asr_en"
  | "alignment_en"
  | "diarization"
  | "speaker_embedding";

export type ModelSetupPhase =
  | "checking"
  | "downloading"
  | "verifying"
  | "publishing"
  | "complete"
  | "failed";

export interface ModelSetupProgressEvent {
  request_id: string;
  key: ModelSetupKey;
  code: string;
  phase: ModelSetupPhase;
  completed_steps: number;
  total_steps: number;
  retryable?: boolean;
}

export interface AudioDeviceList {
  microphones: AudioDevice[];
  outputs: AudioDevice[];
}

export interface RecordingStatus {
  state:
    | "idle"
    | "starting"
    | "recording"
    | "paused"
    | "finalizing"
    | "stopped"
    | "failed";
  sessionId?: string;
  meetingId?: string;
  elapsedMs: number;
  microphoneActive: boolean;
  systemAudioActive: boolean;
  microphoneLevel: number;
  systemAudioLevel: number;
  droppedCapturePackets: number;
  droppedCaptionChunks: number;
  warning?: string;
}

export interface AppStatus {
  appVersion: string;
  schemaVersion: number;
  firstRun: boolean;
  modelReady: boolean;
  offlineReady: boolean;
  activeRecording: RecordingStatus;
  worker: {
    state: string;
    protocolVersion: number;
    pipelineVersion: string;
    processId?: number;
    lastHeartbeatMs?: number;
    error?: string;
  };
  capabilities?: Record<string, unknown>;
}

export interface RecordingLevels {
  microphone: number;
  system: number;
}

export interface DraftRevisionEvent {
  session_id: string;
  stream_id: string;
  speaker_hint: string;
  coalesced_audio_ms: number;
  decode_ms?: number;
  revision: number;
  replace_from_token: number;
  committed_text: string;
  unstable_text: string;
  is_final: boolean;
}

export interface JobProgressEvent {
  job: ProcessingJob;
}

export interface WorkerHealthEvent {
  status: "ready" | "busy" | "recovering" | "offline";
  backend: string;
}

export interface DeviceWarningEvent {
  deviceId: string;
  code:
    | "CAPTURE_FAILED"
    | "DEVICE_LOST"
    | "BUFFER_DISCONTINUITY"
    | "LOW_DISK";
  message: string;
}

export interface MeetingChangedEvent {
  meetingId: string;
  reason: "transcript_finalized" | string;
}
