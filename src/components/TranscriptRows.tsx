import { Bookmark, MessageSquare, MoreVertical } from "lucide-react";
import { useState } from "react";
import { formatDuration } from "../lib/format";
import type { MeetingSpeaker, TranscriptTurn } from "../types";
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
  onSelectTurn?: (turnId: string) => void;
  onEdit?: (turnId: string, editedText: string) => void;
  onToggleMarker?: (turnId: string) => void;
  onToggleReview?: (turnId: string) => void;
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
    const start = lowerText.indexOf(token.toLocaleLowerCase(), cursor);
    if (start < 0) return <HighlightedText text={text} query={query} />;
    if (start > cursor) {
      parts.push({
        key: `gap-${word.id}`,
        text: text.slice(cursor, start),
        start: cursor,
      });
    }
    const end = start + token.length;
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
  onSelectTurn,
  onEdit,
  onToggleMarker,
  onToggleReview,
}: TranscriptRowsProps) {
  const [openTurnMenu, setOpenTurnMenu] = useState<string>();

  return (
    <div className="transcript-rows">
      {turns.map((turn) => {
        const speaker =
          speakers.find((candidate) => candidate.id === turn.speakerId) ??
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
          turn.editedText === undefined || turn.editedText === turn.modelText
            ? turn.words
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
              <strong>{speaker.displayName}</strong>
              {editable ? (
                <div
                  className="transcript-row__editor"
                  contentEditable
                  suppressContentEditableWarning
                  role="textbox"
                  aria-label={`${speaker.displayName} transcript at ${formatDuration(
                    turn.startMs,
                  )}`}
                  onBlur={(event) =>
                    onEdit?.(turn.id, event.currentTarget.textContent ?? "")
                  }
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
