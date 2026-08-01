import type {
  DraftRevisionEvent,
  MeetingSpeaker,
  TranscriptTurn,
} from "../types";

const DRAFT_WINDOW_MS = 25_000;
const SPEAKER_COLORS = ["#0868df", "#8052ca", "#169c9d", "#d57b26"];

function stableHash(value: string) {
  let hash = 0;
  for (const character of value) {
    hash = (hash * 31 + character.charCodeAt(0)) | 0;
  }
  return Math.abs(hash);
}

function safeId(value: string) {
  return value.replace(/[^a-zA-Z0-9_-]/g, "-");
}

export function isDraftRevisionEvent(
  value: unknown,
): value is DraftRevisionEvent {
  if (!value || typeof value !== "object") return false;
  const event = value as Record<string, unknown>;
  return (
    typeof event.session_id === "string" &&
    event.session_id.length > 0 &&
    typeof event.stream_id === "string" &&
    event.stream_id.length > 0 &&
    typeof event.speaker_hint === "string" &&
    typeof event.committed_text === "string" &&
    typeof event.unstable_text === "string" &&
    typeof event.revision === "number" &&
    Number.isFinite(event.revision) &&
    typeof event.replace_from_token === "number" &&
    Number.isFinite(event.replace_from_token) &&
    typeof event.is_final === "boolean"
  );
}

export function draftSpeakerId(event: DraftRevisionEvent) {
  return event.speaker_hint.trim().toLocaleLowerCase() === "you"
    ? "you"
    : `draft-speaker-${safeId(event.stream_id)}`;
}

export function upsertDraftSpeaker(
  speakers: MeetingSpeaker[],
  event: DraftRevisionEvent,
): MeetingSpeaker[] {
  const id = draftSpeakerId(event);
  if (speakers.some((speaker) => speaker.id === id)) return speakers;
  const hint = event.speaker_hint.trim();
  const displayName = id === "you" ? "You" : hint || "Unknown speaker";
  const initials =
    id === "you"
      ? "Y"
      : displayName
          .split(/\s+/)
          .slice(0, 2)
          .map((part) => part[0]?.toUpperCase())
          .join("") || "U";
  return [
    ...speakers,
    {
      id,
      displayName,
      initials,
      color: SPEAKER_COLORS[stableHash(event.stream_id) % SPEAKER_COLORS.length],
      state: id === "you" ? "Matched" : "Unknown",
    },
  ];
}

export function applyDraftRevision(
  turns: TranscriptTurn[],
  event: DraftRevisionEvent,
  elapsedMs: number,
): TranscriptTurn[] {
  const id = `draft-${safeId(event.session_id)}-${safeId(event.stream_id)}`;
  const index = turns.findIndex((turn) => turn.id === id);
  const existing = index >= 0 ? turns[index] : undefined;
  if (
    existing?.revision !== undefined &&
    event.revision <= existing.revision
  ) {
    return turns;
  }

  const modelText = [event.committed_text.trim(), event.unstable_text.trim()]
    .filter(Boolean)
    .join(" ");
  if (!modelText && !existing) return turns;

  const next: TranscriptTurn = {
    id,
    speakerId: draftSpeakerId(event),
    startMs: existing?.startMs ?? Math.max(0, elapsedMs - DRAFT_WINDOW_MS),
    endMs: Math.max(existing?.endMs ?? 0, elapsedMs),
    modelText,
    isDraft: true,
    revision: event.revision,
  };
  if (index < 0) return [...turns, next];
  return turns.map((turn, turnIndex) => (turnIndex === index ? next : turn));
}
