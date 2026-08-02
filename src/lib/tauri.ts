import type {
  AppStatus,
  AudioDevice,
  DeviceWarningEvent,
  DraftRevisionEvent,
  JobProgressEvent,
  Marker,
  Meeting,
  MeetingChangedEvent,
  MeetingDetail,
  ModelPackStatus,
  ModelSetupProgressEvent,
  ProcessingJob,
  RecordingConfig,
  RecordingLevels,
  RecordingSession,
  RecordingStatus,
  RenameSpeakerResult,
  TranscriptTurn,
  VoiceProfile,
  WorkerHealthEvent,
} from "../types";

type ExportFormat = "txt" | "md" | "srt" | "vtt" | "json";

export interface ImportMediaResult {
  meeting: Meeting;
  asset: {
    id: string;
    meetingId: string;
    kind: string;
    displayName: string;
    contentType: string;
    sizeBytes: number;
    durationMs?: number;
  };
  job: ProcessingJob;
}

export interface AssetDescriptor {
  id: string;
  displayName: string;
  contentType: string;
  sizeBytes: number;
  durationMs?: number;
  url?: string;
}

interface AssetChunk {
  assetId: string;
  offset: number;
  bytes: number[];
  endOfFile: boolean;
}

interface LibraryStats {
  meetingCount: number;
  recordingCount: number;
  processingCount: number;
  storageBytes: number;
}

export interface CommandMap {
  get_app_status: { args: undefined; result: AppStatus };
  get_library_stats: { args: undefined; result: LibraryStats };
  list_meetings: {
    args: undefined;
    result: Meeting[];
  };
  get_meeting: { args: { meetingId: string }; result: MeetingDetail };
  rename_meeting: {
    args: { meetingId: string; title: string };
    result: Meeting;
  };
  import_media: {
    args: undefined;
    result: Meeting | null;
  };
  delete_meeting: { args: { meetingId: string }; result: void };
  search_transcript: {
    args: {
      request: { meetingId?: string; query: string; limit?: number };
    };
    result: Array<{
      meetingId: string;
      turnId: string;
      startMs: number;
      speakerName?: string;
      text: string;
      snippet: string;
    }>;
  };
  list_audio_devices: { args: undefined; result: AudioDevice[] };
  get_recording_status: { args: undefined; result: RecordingStatus };
  start_recording: {
    args: { title: string; config: RecordingConfig };
    result: RecordingSession;
  };
  pause_recording: {
    args: { sessionId: string };
    result: RecordingSession;
  };
  resume_recording: {
    args: { sessionId: string };
    result: RecordingSession;
  };
  stop_recording: { args: { sessionId: string }; result: Meeting };
  add_recording_marker: {
    args: { request: { label?: string } };
    result: Marker;
  };
  update_transcript_turn: {
    args: { turnId: string; editedText: string; expectedRevision?: number };
    result: TranscriptTurn;
  };
  set_transcript_turn_review: {
    args: { turnId: string; needsReview: boolean };
    result: TranscriptTurn;
  };
  set_transcript_turn_bookmark: {
    args: { turnId: string; isMarked: boolean };
    result: TranscriptTurn;
  };
  rename_speaker: {
    args: { meetingId: string; speakerId: string; displayName: string };
    result: RenameSpeakerResult;
  };
  merge_speakers: {
    args: {
      meetingId: string;
      sourceSpeakerId: string;
      targetSpeakerId: string;
    };
    result: void;
  };
  review_speaker: {
    args: {
      meetingId: string;
      speakerId: string;
      accepted: boolean;
    };
    result: void;
  };
  set_speaker_review: {
    args: { request: { speakerId: string; needsReview: boolean } };
    result: void;
  };
  list_voice_profiles: { args: undefined; result: VoiceProfile[] };
  create_voice_profile: {
    args: { name: string };
    result: VoiceProfile;
  };
  delete_voice_profile: { args: { profileId: string }; result: void };
  confirm_voice_profile_sample: {
    args: {
      request: {
        profileId: string;
        meetingId: string;
        speakerId: string;
      };
    };
    result: VoiceProfile;
  };
  list_processing_jobs: {
    args: { meetingId?: string };
    result: ProcessingJob[];
  };
  cancel_processing_job: {
    args: { jobId: string };
    result: ProcessingJob;
  };
  retry_processing_job: {
    args: { jobId: string };
    result: ProcessingJob;
  };
  export_transcript: {
    args: { meetingId: string; format: ExportFormat };
    result: {
      artifactId: string;
      fileName: string;
      format: ExportFormat;
      sizeBytes: number;
    };
  };
  get_model_status: { args: undefined; result: ModelPackStatus };
  install_model_pack: {
    args: { huggingFaceToken: string };
    result: ModelPackStatus;
  };
  create_backup: {
    args: undefined;
    result: {
      backupId: string;
      fileCount: number;
      totalBytes: number;
      includesMedia: boolean;
    };
  };
  get_asset_descriptor: {
    args: { assetId: string };
    result: AssetDescriptor;
  };
  read_asset_chunk: {
    args: { request: { assetId: string; offset: number; length: number } };
    result: AssetChunk;
  };
  get_worker_status: {
    args: undefined;
    result: AppStatus["worker"];
  };
  restart_worker: { args: undefined; result: AppStatus["worker"] };
}

export interface EventMap {
  "recording://levels": RecordingLevels;
  "recording://state": RecordingSession;
  "transcript://draft-revision": DraftRevisionEvent;
  "job://progress": JobProgressEvent;
  "meeting://changed": MeetingChangedEvent;
  "model://setup-progress": ModelSetupProgressEvent;
  "worker://health": WorkerHealthEvent;
  "device://warning": DeviceWarningEvent;
}

export function isTauriRuntime() {
  return "__TAURI_INTERNALS__" in window;
}

export async function invokeCommand<K extends keyof CommandMap>(
  command: K,
  ...args: CommandMap[K]["args"] extends undefined
    ? []
    : [args: CommandMap[K]["args"]]
): Promise<CommandMap[K]["result"]> {
  if (!isTauriRuntime()) {
    throw new Error(`Desktop command "${command}" is unavailable in browser preview.`);
  }
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<CommandMap[K]["result"]>(command, args[0] ?? {});
}

export async function listenEvent<K extends keyof EventMap>(
  eventName: K,
  handler: (payload: EventMap[K]) => void,
) {
  if (!isTauriRuntime()) return () => undefined;
  const { listen } = await import("@tauri-apps/api/event");
  return listen<EventMap[K]>(eventName, ({ payload }) => handler(payload));
}

export async function windowAction(
  action: "minimize" | "toggleMaximize" | "close",
) {
  if (!isTauriRuntime()) return;
  const { getCurrentWindow } = await import("@tauri-apps/api/window");
  const currentWindow = getCurrentWindow();
  if (action === "minimize") await currentWindow.minimize();
  if (action === "toggleMaximize") await currentWindow.toggleMaximize();
  if (action === "close") await currentWindow.close();
}

const approvedExternalUrls = new Set([
  "https://huggingface.co/pyannote/speaker-diarization-community-1",
  "https://huggingface.co/settings/tokens",
]);

export async function openExternalUrl(url: string) {
  if (!approvedExternalUrls.has(url)) {
    throw new Error("This external link is not approved by SayTrace.");
  }
  if (isTauriRuntime()) {
    const { openUrl } = await import("@tauri-apps/plugin-opener");
    await openUrl(url);
    return;
  }
  window.open(url, "_blank", "noopener,noreferrer");
}

export async function createAssetObjectUrl(assetId: string) {
  const descriptor = await invokeCommand("get_asset_descriptor", { assetId });
  if (descriptor.url) return descriptor.url;
  const maximumBlobFallbackBytes = 256 * 1024 * 1024;
  if (descriptor.sizeBytes > maximumBlobFallbackBytes) {
    throw new Error(
      "This media requires the desktop streaming protocol and is too large for the bounded Blob fallback.",
    );
  }
  const chunks: Uint8Array[] = [];
  let offset = 0;
  const chunkSize = 4 * 1024 * 1024;
  while (offset < descriptor.sizeBytes) {
    const chunk = await invokeCommand("read_asset_chunk", {
      request: {
        assetId,
        offset,
        length: Math.min(chunkSize, descriptor.sizeBytes - offset),
      },
    });
    chunks.push(Uint8Array.from(chunk.bytes));
    offset += chunk.bytes.length;
    if (chunk.endOfFile || chunk.bytes.length === 0) break;
  }
  return URL.createObjectURL(
    new Blob(chunks as BlobPart[], { type: descriptor.contentType }),
  );
}
