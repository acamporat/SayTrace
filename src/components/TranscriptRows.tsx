import { Bookmark, MessageSquare, MoreVertical } from "lucide-react";
import { memo, useCallback, useMemo, useRef, useState } from "react";
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
  playedWordId?: string;
  /** Retained for callers that provide a discrete position instead of a word cursor. */
  playbackPositionMs?: number;
  onSelectTurn?: (turnId: string) => void;
  onEdit?: (turnId: string, editedText: string) => void;
  onToggleMarker?: (turnId: string) => void;
  onToggleReview?: (turnId: string) => void;
}

type PlaybackState = "past" | "active" | "future" | "none";

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

type TimedPart = {
  key: string;
  text: string;
  start: number;
  word?: NonNullable<TranscriptTurn["words"]>[number];
  wordIndex?: number;
};

function alignTimedParts(
  text: string,
  words: NonNullable<TranscriptTurn["words"]>,
): TimedPart[] {
  const parts: TimedPart[] = [];
  let cursor = 0;

  words.forEach((word, wordIndex) => {
    const token = word.text.trim();
    if (!token) return;
    const range = findWordRange(text, token, cursor);
    if (!range) return;
    const { start, end } = range;
    if (start > cursor) {
      parts.push({
        key: `gap-${word.id}`,
        text: text.slice(cursor, start),
        start: cursor,
      });
    }
    parts.push({
      key: word.id,
      text: text.slice(start, end),
      start,
      word,
      wordIndex,
    });
    cursor = end;
  });

  if (cursor < text.length) {
    parts.push({ key: "tail", text: text.slice(cursor), start: cursor });
  }
  return parts;
}

const TimedTranscriptText = memo(function TimedTranscriptText({
  text,
  query,
  words,
  activeWordId,
  playedWordId,
  playbackState,
  playbackPositionMs,
}: {
  text: string;
  query?: string;
  words: TranscriptTurn["words"];
  activeWordId?: string;
  playedWordId?: string;
  playbackState: PlaybackState;
  playbackPositionMs?: number;
}) {
  const parts = useMemo(
    () => (words?.length ? alignTimedParts(text, words) : undefined),
    [text, words],
  );
  const playedWordIndex = useMemo(
    () => words?.findIndex((word) => word.id === playedWordId) ?? -1,
    [playedWordId, words],
  );

  if (!parts) return <HighlightedText text={text} query={query} />;

  const lowerText = text.toLocaleLowerCase();
  const term = query?.trim() ?? "";
  const highlightStart = term
    ? lowerText.indexOf(term.toLocaleLowerCase())
    : -1;
  const highlightEnd = highlightStart < 0 ? -1 : highlightStart + term.length;

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
          playbackPositionMs !== undefined
            ? part.word.endMs <= playbackPositionMs
            : playbackState === "past" ||
              (playbackState === "active" &&
                playedWordIndex >= 0 &&
                (part.wordIndex ?? -1) <= playedWordIndex);
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
});

interface TranscriptRowProps {
  turn: TranscriptTurn;
  speaker?: MeetingSpeaker;
  search?: string;
  editable: boolean;
  selected: boolean;
  playbackState: PlaybackState;
  activeWordId?: string;
  playedWordId?: string;
  playbackPositionMs?: number;
  isLastDraft: boolean;
  menuOpen: boolean;
  onSelectTurn: (turnId: string) => void;
  onEdit: (turnId: string, editedText: string) => void;
  onToggleMarker: (turnId: string) => void;
  onToggleReview: (turnId: string) => void;
  onToggleMenu: (turnId: string) => void;
}

const TranscriptRow = memo(function TranscriptRow({
  turn,
  speaker: suppliedSpeaker,
  search,
  editable,
  selected,
  playbackState,
  activeWordId,
  playedWordId,
  playbackPositionMs,
  isLastDraft,
  menuOpen,
  onSelectTurn,
  onEdit,
  onToggleMarker,
  onToggleReview,
  onToggleMenu,
}: TranscriptRowProps) {
  const speaker = suppliedSpeaker ?? {
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
  const active = playbackState === "active";

  return (
    <article
      className={`transcript-row ${selected ? "is-selected" : ""} ${
        active ? "is-playback-active" : ""
      } ${turn.needsReview ? "needs-review" : ""}`}
      data-playback-active={active ? "true" : undefined}
      data-turn-id={turn.id}
      onClick={() => onSelectTurn(turn.id)}
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
            onBlur={(event) => {
              const editedText = event.currentTarget.textContent ?? "";
              if (editedText !== displayText) onEdit(turn.id, editedText);
            }}
          >
            <TimedTranscriptText
              text={displayText}
              query={search}
              words={alignedWords}
              activeWordId={activeWordId}
              playedWordId={playedWordId}
              playbackState={playbackState}
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
              playedWordId={playedWordId}
              playbackState={playbackState}
              playbackPositionMs={playbackPositionMs}
            />
            {turn.isDraft && isLastDraft ? (
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
            aria-label={turn.isMarked ? "Remove bookmark" : "Add bookmark"}
            onClick={(event) => {
              event.stopPropagation();
              onToggleMarker(turn.id);
            }}
          >
            <Bookmark size={18} fill={turn.isMarked ? "currentColor" : "none"} />
          </button>
          <button
            className={turn.needsReview ? "is-active" : ""}
            type="button"
            aria-label="More transcript actions"
            aria-expanded={menuOpen}
            onClick={(event) => {
              event.stopPropagation();
              onToggleMenu(turn.id);
            }}
          >
            {turn.needsReview ? (
              <MessageSquare size={17} fill="currentColor" />
            ) : (
              <MoreVertical size={18} />
            )}
          </button>
          {menuOpen ? (
            <div className="transcript-row__menu">
              <button
                type="button"
                onClick={(event) => {
                  event.stopPropagation();
                  onToggleReview(turn.id);
                  onToggleMenu(turn.id);
                }}
              >
                <MessageSquare size={15} />
                {turn.needsReview ? "Clear review flag" : "Flag for review"}
              </button>
            </div>
          ) : null}
        </div>
      ) : null}
    </article>
  );
});

export function TranscriptRows({
  turns,
  speakers,
  search,
  editable = false,
  selectedTurnId,
  activeTurnId,
  activeWordId,
  playedWordId,
  playbackPositionMs,
  onSelectTurn,
  onEdit,
  onToggleMarker,
  onToggleReview,
}: TranscriptRowsProps) {
  const [openTurnMenu, setOpenTurnMenu] = useState<string>();
  const callbacksRef = useRef({
    onSelectTurn,
    onEdit,
    onToggleMarker,
    onToggleReview,
  });
  callbacksRef.current = {
    onSelectTurn,
    onEdit,
    onToggleMarker,
    onToggleReview,
  };
  const speakerById = useMemo(
    () => new Map(speakers.map((speaker) => [speaker.id, speaker])),
    [speakers],
  );
  const activeTurnIndex = useMemo(
    () => turns.findIndex((turn) => turn.id === activeTurnId),
    [activeTurnId, turns],
  );

  const selectTurn = useCallback((turnId: string) => {
    callbacksRef.current.onSelectTurn?.(turnId);
  }, []);
  const editTurn = useCallback((turnId: string, editedText: string) => {
    callbacksRef.current.onEdit?.(turnId, editedText);
  }, []);
  const toggleMarker = useCallback((turnId: string) => {
    callbacksRef.current.onToggleMarker?.(turnId);
  }, []);
  const toggleReview = useCallback((turnId: string) => {
    callbacksRef.current.onToggleReview?.(turnId);
  }, []);
  const toggleMenu = useCallback((turnId: string) => {
    setOpenTurnMenu((current) => (current === turnId ? undefined : turnId));
  }, []);
  const lastTurnId = turns[turns.length - 1]?.id;

  return (
    <div className="transcript-rows">
      {turns.map((turn, index) => {
        const playbackState: PlaybackState =
          activeTurnIndex < 0
            ? "none"
            : index < activeTurnIndex
              ? "past"
              : index === activeTurnIndex
                ? "active"
                : "future";
        const active = playbackState === "active";
        return (
          <TranscriptRow
            key={turn.id}
            turn={turn}
            speaker={
              turn.speakerId ? speakerById.get(turn.speakerId) : speakers[0]
            }
            search={search}
            editable={editable}
            selected={selectedTurnId === turn.id}
            playbackState={playbackState}
            activeWordId={active ? activeWordId : undefined}
            playedWordId={active ? playedWordId : undefined}
            playbackPositionMs={playbackPositionMs}
            isLastDraft={turn.id === lastTurnId}
            menuOpen={openTurnMenu === turn.id}
            onSelectTurn={selectTurn}
            onEdit={editTurn}
            onToggleMarker={toggleMarker}
            onToggleReview={toggleReview}
            onToggleMenu={toggleMenu}
          />
        );
      })}
    </div>
  );
}
