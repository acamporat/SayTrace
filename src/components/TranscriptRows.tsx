import { Bookmark, MessageSquare, MoreVertical } from "lucide-react";
import { useMemo, useState } from "react";
import { formatDuration } from "../lib/format";
import type {
  MeetingSpeaker,
  TranscriptTurn,
  VisualContextEvent,
} from "../types";
import { SpeakerAvatar } from "./SpeakerAvatar";

interface TranscriptRowsProps {
  turns: TranscriptTurn[];
  speakers: MeetingSpeaker[];
  search?: string;
  editable?: boolean;
  selectedTurnId?: string;
  activeTurnId?: string;
  activeWordId?: string;
  playbackPositionMs?: number;
  visualContext?: VisualContextEvent[];
  visualContextUrls?: Readonly<Record<string, string>>;
  onSelectTurn?: (turnId: string) => void;
  onEdit?: (turnId: string, editedText: string) => void;
  onToggleMarker?: (turnId: string) => void;
  onToggleReview?: (turnId: string) => void;
}

const EMPTY_VISUAL_CONTEXT: VisualContextEvent[] = [];
const EMPTY_VISUAL_CONTEXT_URLS: Readonly<Record<string, string>> = {};

function VisualContextFigure({
  event,
  screenshotUrl,
}: {
  event: VisualContextEvent;
  screenshotUrl?: string;
}) {
  const sourceLabel = event.meetingSystem || "recorded desktop";
  const alt = `Screen context captured from ${sourceLabel} at ${formatDuration(
    event.atMs,
  )}`;
  return (
    <figure
      className="visual-context-card"
      data-visual-context-id={event.id}
      onClick={(clickEvent) => clickEvent.stopPropagation()}
    >
      {screenshotUrl ? (
        <img
          src={screenshotUrl}
          alt={alt}
          loading="lazy"
          decoding="async"
        />
      ) : (
        <div className="visual-context-card__unavailable" role="img" aria-label={alt}>
          Screen context image unavailable
        </div>
      )}
      <figcaption>
        <span>
          <strong>Screen context</strong>
          <small>{sourceLabel}</small>
        </span>
        <time dateTime={`PT${Math.max(0, Math.round(event.atMs / 1_000))}S`}>
          {formatDuration(event.atMs)}
        </time>
      </figcaption>
      <p>{event.reason}</p>
      {event.triggerText ? (
        <blockquote>“{event.triggerText}”</blockquote>
      ) : null}
    </figure>
  );
}

function HighlightedText({ text, query }: { text: string; query?: string }) {
  if (!query?.trim()) return text;
  const term = query.trim();
  const start = text.toLocaleLowerCase().indexOf(term.toLocaleLowerCase());
  if (start < 0) return text;
  return (
    <>
      {text.slice(0, start)}
      <mark>{text.slice(start, start + term.length)}</mark>
      {text.slice(start + term.length)}
    </>
  );
}

function HighlightedSlice({
  text,
  offset,
  highlightStart,
  highlightEnd,
}: {
  text: string;
  offset: number;
  highlightStart: number;
  highlightEnd: number;
}) {
  const localStart = Math.max(0, highlightStart - offset);
  const localEnd = Math.min(text.length, highlightEnd - offset);
  if (localStart >= localEnd) return text;
  return (
    <>
      {text.slice(0, localStart)}
      <mark>{text.slice(localStart, localEnd)}</mark>
      {text.slice(localEnd)}
    </>
  );
}

function findWordRange(text: string, token: string, cursor: number) {
  const exactStart = text
    .toLocaleLowerCase()
    .indexOf(token.toLocaleLowerCase(), cursor);
  if (exactStart >= 0) {
    return { start: exactStart, end: exactStart + token.length };
  }

  const normalizedToken = Array.from(token.toLocaleLowerCase()).filter((char) =>
    /[\p{L}\p{N}]/u.test(char),
  );
  if (!normalizedToken.length) return undefined;

  let matched = 0;
  let start = -1;
  for (let index = cursor; index < text.length; index += 1) {
    const char = text[index].toLocaleLowerCase();
    if (!/[\p{L}\p{N}]/u.test(char)) continue;
    if (char === normalizedToken[matched]) {
      if (matched === 0) start = index;
      matched += 1;
      if (matched === normalizedToken.length) {
        return { start, end: index + 1 };
      }
    } else {
      matched = char === normalizedToken[0] ? 1 : 0;
      start = matched ? index : -1;
    }
  }
  return undefined;
}

function TimedTranscriptText({
  text,
  query,
  words,
  activeWordId,
  playbackPositionMs,
}: {
  text: string;
  query?: string;
  words: TranscriptTurn["words"];
  activeWordId?: string;
  playbackPositionMs?: number;
}) {
  if (!words?.length) return <HighlightedText text={text} query={query} />;

  const lowerText = text.toLocaleLowerCase();
  const parts: Array<{
    key: string;
    text: string;
    start: number;
    word?: NonNullable<TranscriptTurn["words"]>[number];
  }> = [];
  const term = query?.trim() ?? "";
  const highlightStart = term
    ? lowerText.indexOf(term.toLocaleLowerCase())
    : -1;
  const highlightEnd = highlightStart < 0 ? -1 : highlightStart + term.length;
  let cursor = 0;

  for (const word of words) {
    const token = word.text.trim();
    if (!token) continue;
    const range = findWordRange(text, token, cursor);
    if (!range) continue;
    const { start, end } = range;
    if (start > cursor) {
      parts.push({
        key: `gap-${word.id}`,
        text: text.slice(cursor, start),
        start: cursor,
      });
    }
    parts.push({ key: word.id, text: text.slice(start, end), start, word });
    cursor = end;
  }

  if (cursor < text.length) {
    parts.push({ key: "tail", text: text.slice(cursor), start: cursor });
  }

  return (
    <>
      {parts.map((part) => {
        if (!part.word) {
          return (
            <HighlightedSlice
              key={part.key}
              text={part.text}
              offset={part.start}
              highlightStart={highlightStart}
              highlightEnd={highlightEnd}
            />
          );
        }
        const isCurrent = part.word.id === activeWordId;
        const isPlayed =
          playbackPositionMs !== undefined &&
          part.word.endMs <= playbackPositionMs;
        return (
          <span
            key={part.key}
            className={`transcript-word${isPlayed ? " is-played" : ""}${
              isCurrent ? " is-current" : ""
            }`}
            data-word-id={part.word.id}
            data-playback-current={isCurrent ? "true" : undefined}
          >
            <HighlightedSlice
              text={part.text}
              offset={part.start}
              highlightStart={highlightStart}
              highlightEnd={highlightEnd}
            />
          </span>
        );
      })}
    </>
  );
}

export function TranscriptRows({
  turns,
  speakers,
  search,
  editable = false,
  selectedTurnId,
  activeTurnId,
  activeWordId,
  playbackPositionMs,
  visualContext = EMPTY_VISUAL_CONTEXT,
  visualContextUrls = EMPTY_VISUAL_CONTEXT_URLS,
  onSelectTurn,
  onEdit,
  onToggleMarker,
  onToggleReview,
}: TranscriptRowsProps) {
  const [openTurnMenu, setOpenTurnMenu] = useState<string>();
  const speakerById = useMemo(
    () => new Map(speakers.map((speaker) => [speaker.id, speaker])),
    [speakers],
  );
  const visualContextByTurn = useMemo(() => {
    const grouped = new Map<string, VisualContextEvent[]>();
    for (const event of visualContext) {
      const current = grouped.get(event.turnId);
      if (current) current.push(event);
      else grouped.set(event.turnId, [event]);
    }
    return grouped;
  }, [visualContext]);

  return (
    <div className="transcript-rows">
      {turns.map((turn) => {
        const speaker =
          (turn.speakerId ? speakerById.get(turn.speakerId) : undefined) ??
          speakers[0] ?? {
            id: turn.speakerId ?? "unknown",
            displayName:
              turn.speakerId === "you"
                ? "You"
                : turn.speakerId
                  ? `Speaker ${turn.speakerId.replace(/\D/g, "") || ""}`.trim()
                  : "Speaker 1",
            initials: turn.speakerId === "you" ? "Y" : "U",
            color: turn.speakerId === "you" ? "#0868df" : "#676c72",
            state: "Unknown" as const,
          };
        const displayText = turn.editedText ?? turn.modelText;
        const alignedWords =
          turn.editedText == null || turn.editedText === turn.modelText
            ? turn.words
            : undefined;
        const turnVisualContext = visualContextByTurn.get(turn.id) ?? [];
        const visualSpeakerEvidence = turnVisualContext.find(
          (event) => event.kind === "speaker_evidence",
        );
        const visualAttribution =
          visualSpeakerEvidence || speaker.attributionSource?.startsWith("visual")
            ? speaker.attributionSource === "visual_confirmed" ||
              speaker.attributionConfidence === "confirmed"
              ? "Visual cue confirmed"
              : "Visual suggestion"
            : undefined;
        return (
          <article
            key={turn.id}
            className={`transcript-row ${
              selectedTurnId === turn.id ? "is-selected" : ""
            } ${activeTurnId === turn.id ? "is-playback-active" : ""} ${
              turn.needsReview ? "needs-review" : ""
            }`}
            data-playback-active={activeTurnId === turn.id ? "true" : undefined}
            data-turn-id={turn.id}
            onClick={() => onSelectTurn?.(turn.id)}
          >
            <time>{formatDuration(turn.startMs)}</time>
            <SpeakerAvatar initials={speaker.initials} color={speaker.color} />
            <div className="transcript-row__content">
              <div className="transcript-row__speaker-line">
                <strong>{speaker.displayName}</strong>
                {visualAttribution ? (
                  <span
                    className={`visual-attribution-badge${
                      visualAttribution === "Visual suggestion"
                        ? " is-review"
                        : " is-confirmed"
                    }`}
                    title={
                      visualSpeakerEvidence?.reason ??
                      "Speaker attribution includes local visual context."
                    }
                  >
                    {visualAttribution}
                  </span>
                ) : null}
              </div>
              {editable ? (
                <div
                  className="transcript-row__editor"
                  contentEditable
                  suppressContentEditableWarning
                  role="textbox"
                  aria-label={`${speaker.displayName} transcript at ${formatDuration(
                    turn.startMs,
                  )}`}
                  onBlur={(event) => {
                    const editedText = event.currentTarget.textContent ?? "";
                    if (editedText !== displayText) {
                      onEdit?.(turn.id, editedText);
                    }
                  }}
                >
                  <TimedTranscriptText
                    text={displayText}
                    query={search}
                    words={alignedWords}
                    activeWordId={activeWordId}
                    playbackPositionMs={playbackPositionMs}
                  />
                </div>
              ) : (
                <p>
                  <TimedTranscriptText
                    text={displayText}
                    query={search}
                    words={alignedWords}
                    activeWordId={activeWordId}
                    playbackPositionMs={playbackPositionMs}
                  />
                  {turn.isDraft &&
                  turn.id === turns[turns.length - 1]?.id ? (
                    <span className="draft-ellipsis" aria-label="Caption updating">
                      <i />
                      <i />
                      <i />
                    </span>
                  ) : null}
                </p>
              )}
              {turnVisualContext
                .filter(
                  (event) =>
                    event.kind !== "speaker_evidence" &&
                    Boolean(event.screenshotAssetId),
                )
                .map((event) => (
                  <VisualContextFigure
                    key={event.id}
                    event={event}
                    screenshotUrl={
                      event.screenshotAssetId
                        ? visualContextUrls[event.screenshotAssetId]
                        : undefined
                    }
                  />
                ))}
            </div>
            {editable ? (
              <div className="transcript-row__actions">
                <button
                  className={turn.isMarked ? "is-active" : ""}
                  type="button"
                  aria-label={
                    turn.isMarked ? "Remove bookmark" : "Add bookmark"
                  }
                  onClick={(event) => {
                    event.stopPropagation();
                    onToggleMarker?.(turn.id);
                  }}
                >
                  <Bookmark
                    size={18}
                    fill={turn.isMarked ? "currentColor" : "none"}
                  />
                </button>
                <button
                  className={turn.needsReview ? "is-active" : ""}
                  type="button"
                  aria-label="More transcript actions"
                  aria-expanded={openTurnMenu === turn.id}
                  onClick={(event) => {
                    event.stopPropagation();
                    setOpenTurnMenu((current) =>
                      current === turn.id ? undefined : turn.id,
                    );
                  }}
                >
                  {turn.needsReview ? (
                    <MessageSquare size={17} fill="currentColor" />
                  ) : (
                    <MoreVertical size={18} />
                  )}
                </button>
                {openTurnMenu === turn.id ? (
                  <div className="transcript-row__menu">
                    <button
                      type="button"
                      onClick={(event) => {
                        event.stopPropagation();
                        onToggleReview?.(turn.id);
                        setOpenTurnMenu(undefined);
                      }}
                    >
                      <MessageSquare size={15} />
                      {turn.needsReview
                        ? "Clear review flag"
                        : "Flag for review"}
                    </button>
                  </div>
                ) : null}
              </div>
            ) : null}
          </article>
        );
      })}
    </div>
  );
}
