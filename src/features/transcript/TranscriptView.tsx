import {
  Bookmark,
  CalendarDays,
  Check,
  CheckCircle2,
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  EllipsisVertical,
  FileSearch,
  Gauge,
  Info,
  ListRestart,
  Merge,
  MessageSquare,
  MoreVertical,
  Monitor,
  Pause,
  Pencil,
  Play,
  Search,
  SkipBack,
  SkipForward,
  Sparkles,
  Volume2,
  Wifi,
  X,
} from "lucide-react";
import {
  forwardRef,
  useCallback,
  useEffect,
  useImperativeHandle,
  useMemo,
  useRef,
  useState,
} from "react";
import {
  formatDuration,
  formatLongDate,
  formatMeetingTime,
} from "../../lib/format";
import type {
  Meeting,
  MeetingSpeaker,
  Marker,
  LocalAgentStatus,
  ProcessingJob,
  SpeakerState,
  TranscriptChatMessage,
  TranscriptTurn,
  VisualContextEvent,
  VoiceProfile,
} from "../../types";
import { SpeakerAvatar } from "../../components/SpeakerAvatar";
import { TranscriptRows } from "../../components/TranscriptRows";
import { Waveform, type WaveformHandle } from "../../components/Waveform";
import { TranscriptAssistantPanel } from "./TranscriptAssistantPanel";

type ExportFormat = "txt" | "md" | "srt" | "vtt" | "json";

interface TranscriptViewProps {
  meeting: Meeting;
  mediaSourceUrl?: string;
  screenSourceUrl?: string;
  allowSimulatedPlayback: boolean;
  turns: TranscriptTurn[];
  speakers: MeetingSpeaker[];
  markers: Marker[];
  visualContext: VisualContextEvent[];
  visualContextUrls: Readonly<Record<string, string>>;
  processingJob?: ProcessingJob;
  profiles: VoiceProfile[];
  profileSampleTargetId?: string;
  chatMessages: TranscriptChatMessage[];
  agentStatus: LocalAgentStatus;
  agentLoading: boolean;
  agentError?: string;
  onUpdateTurn: (turnId: string, editedText: string) => void;
  onToggleMarker: (turnId: string) => void;
  onToggleTurnReview: (turnId: string) => void;
  onRenameMeeting: (title: string) => void;
  onRenameSpeaker: (speakerId: string, name: string) => void;
  onMergeSpeaker: (sourceSpeakerId: string, targetSpeakerId: string) => void;
  onReviewSpeaker: (speakerId: string, accepted: boolean) => void;
  onCancelJob: (jobId: string) => void;
  onRetryJob: (jobId: string) => void;
  onConfirmVoiceSample: (
    speakerId: string,
    profileId?: string,
    newProfileName?: string,
  ) => Promise<void>;
  onExport: (format: ExportFormat) => void;
  onAskTranscript: (question: string, model?: string) => Promise<void>;
  onClearTranscriptChat: () => Promise<void>;
  onRefreshAgentStatus: () => Promise<void>;
}

const pipelineSteps = [
  "Preparing media",
  "Transcribing",
  "Aligning words",
  "Identifying speakers",
];

function SpeakerStateBadge({ state }: { state: SpeakerState }) {
  return (
    <span className={`speaker-state speaker-state--${state.toLowerCase()}`}>
      {state === "Matched" ? <Check size={12} /> : null}
      {state}
    </span>
  );
}

interface SpeakerCardProps {
  speaker: MeetingSpeaker;
  allSpeakers: MeetingSpeaker[];
  selected: boolean;
  onRename: (name: string) => void;
  onMerge: (targetId: string) => void;
  suggestedProfileName?: string;
  visualEvidence?: VisualContextEvent;
  onAcceptReview: () => void;
  onRejectReview: () => void;
}

function SpeakerCard({
  speaker,
  allSpeakers,
  selected,
  onRename,
  onMerge,
  suggestedProfileName,
  visualEvidence,
  onAcceptReview,
  onRejectReview,
}: SpeakerCardProps) {
  const [editing, setEditing] = useState(false);
  const [name, setName] = useState(speaker.displayName);
  const [menuOpen, setMenuOpen] = useState(false);
  const [mergeOpen, setMergeOpen] = useState(false);

  function finishRename(commit: boolean) {
    const value = name.trim();
    if (commit && value && value !== speaker.displayName) {
      onRename(value);
    } else if (!commit || !value) {
      setName(speaker.displayName);
    }
    setEditing(false);
  }

  const visualSuggestion =
    speaker.attributionSource === "visual" || Boolean(visualEvidence);
  const visualSuggestionName = visualEvidence?.suggestedSpeakerName ??
    (speaker.attributionSource === "visual" ? speaker.displayName : undefined);

  return (
    <div
      className={`speaker-card${selected ? " is-selected" : ""}`}
      data-speaker-id={speaker.id}
      aria-current={selected ? "true" : undefined}
    >
      <div className="speaker-card__topline">
        <SpeakerAvatar initials={speaker.initials} color={speaker.color} />
        {editing ? (
          <form
            className="speaker-card__rename"
            onSubmit={(event) => {
              event.preventDefault();
              finishRename(true);
            }}
          >
            <input
              autoFocus
              aria-label="Speaker name"
              value={name}
              onChange={(event) => setName(event.target.value)}
              onBlur={() => finishRename(true)}
              onKeyDown={(event) => {
                if (event.key === "Escape") {
                  event.preventDefault();
                  finishRename(false);
                }
              }}
            />
          </form>
        ) : (
          <strong>{speaker.displayName}</strong>
        )}
        <SpeakerStateBadge state={speaker.state} />
        <button
          type="button"
          aria-label={`More actions for ${speaker.displayName}`}
          onClick={() => setMenuOpen((open) => !open)}
        >
          <MoreVertical size={18} />
        </button>
        {menuOpen ? (
          <div className="speaker-card__menu">
            <button
              type="button"
              onClick={() => {
                setName(speaker.displayName);
                setEditing(true);
                setMenuOpen(false);
              }}
            >
              <Pencil size={15} /> Rename speaker
            </button>
            <button
              type="button"
              onClick={() => {
                setMergeOpen(true);
                setMenuOpen(false);
              }}
            >
              <Merge size={15} /> Merge speaker
            </button>
          </div>
        ) : null}
      </div>

      <div className="speaker-card__rule">
        <span
          style={{
            width:
              speaker.state === "Matched"
                ? "78%"
                : speaker.state === "Review"
                  ? "44%"
                  : "18%",
            backgroundColor: speaker.color,
          }}
        />
      </div>

      {visualEvidence || speaker.attributionSource?.startsWith("visual") ? (
        <div
          className={`speaker-card__visual-evidence${
            speaker.attributionSource === "visual_confirmed" ||
            speaker.attributionConfidence === "confirmed"
              ? " is-confirmed"
              : " is-review"
          }`}
        >
          <strong>
            {speaker.attributionSource === "visual_confirmed" ||
            speaker.attributionConfidence === "confirmed"
              ? "Visual cue confirmed"
              : "Visual suggestion · Review"}
          </strong>
          <p>
            {visualEvidence?.reason ??
              "Local meeting-app visual context contributed to this speaker suggestion."}
          </p>
        </div>
      ) : null}

      {mergeOpen ? (
        <div className="speaker-card__merge">
          <span>Merge into</span>
          {allSpeakers
            .filter((candidate) => candidate.id !== speaker.id)
            .map((candidate) => (
              <button
                key={candidate.id}
                type="button"
                onClick={() => {
                  onMerge(candidate.id);
                  setMergeOpen(false);
                }}
              >
                {candidate.displayName}
              </button>
            ))}
          <button type="button" onClick={() => setMergeOpen(false)}>
            Cancel
          </button>
        </div>
      ) : speaker.state === "Review" ? (
        <div className="speaker-card__review-actions">
          <button
            className="speaker-card__review"
            type="button"
            disabled={!suggestedProfileName && !visualSuggestion}
            onClick={onAcceptReview}
          >
            <Check size={16} />{" "}
            {suggestedProfileName
              ? `Accept ${suggestedProfileName}`
              : visualSuggestionName
                ? `Accept ${visualSuggestionName}`
                : "No suggested match"}
          </button>
          <button type="button" onClick={onRejectReview}>
            <X size={15} /> Keep unknown
          </button>
        </div>
      ) : (
        <div className="speaker-card__actions">
          <button type="button" onClick={() => setEditing(true)}>
            <Pencil size={15} /> Rename
          </button>
          <button type="button" onClick={() => setMergeOpen(true)}>
            <Merge size={15} /> Merge
          </button>
        </div>
      )}
    </div>
  );
}

function findActiveWordId(
  words: Array<{ id: string; startMs: number; endMs: number }>,
  positionMs: number,
) {
  let low = 0;
  let high = words.length - 1;
  let candidate = -1;
  while (low <= high) {
    const middle = Math.floor((low + high) / 2);
    if (words[middle].startMs <= positionMs) {
      candidate = middle;
      low = middle + 1;
    } else {
      high = middle - 1;
    }
  }
  const word = candidate >= 0 ? words[candidate] : undefined;
  return word && positionMs < word.endMs ? word.id : undefined;
}

function findPlayedWordId(
  words: Array<{ id: string; startMs: number; endMs: number }>,
  positionMs: number,
) {
  let low = 0;
  let high = words.length - 1;
  let candidate = -1;
  while (low <= high) {
    const middle = Math.floor((low + high) / 2);
    if (words[middle].endMs <= positionMs) {
      candidate = middle;
      low = middle + 1;
    } else {
      high = middle - 1;
    }
  }
  return candidate >= 0 ? words[candidate].id : undefined;
}

function findActiveTurnIndex(turns: TranscriptTurn[], positionMs: number) {
  let low = 0;
  let high = turns.length - 1;
  let candidate = -1;
  while (low <= high) {
    const middle = Math.floor((low + high) / 2);
    if (turns[middle].startMs <= positionMs) {
      candidate = middle;
      low = middle + 1;
    } else {
      high = middle - 1;
    }
  }
  return candidate;
}

interface PlaybackTranscriptRowsHandle {
  updatePosition: (positionMs: number) => void;
}

interface PlaybackTranscriptRowsProps {
  turns: TranscriptTurn[];
  speakers: MeetingSpeaker[];
  visualContext: VisualContextEvent[];
  visualContextUrls: Readonly<Record<string, string>>;
  search: string;
  selectedTurnId?: string;
  autoScroll: boolean;
  initialPositionMs: number;
  playing: boolean;
  scrollRef: React.RefObject<HTMLElement | null>;
  onSelectTurn: (turnId: string) => void;
  onEdit: (turnId: string, editedText: string) => void;
  onToggleMarker: (turnId: string) => void;
  onToggleReview: (turnId: string) => void;
}

function playbackCursor(
  turns: TranscriptTurn[],
  positionMs: number,
) {
  const activeTurnIndex = findActiveTurnIndex(turns, positionMs);
  const activeTurn = activeTurnIndex >= 0 ? turns[activeTurnIndex] : undefined;
  const activeWords = activeTurn?.words ?? [];
  return {
    activeTurnId: activeTurn?.id,
    activeWordId: findActiveWordId(activeWords, positionMs),
    playedWordId: findPlayedWordId(activeWords, positionMs),
  };
}

const PlaybackTranscriptRows = forwardRef<
  PlaybackTranscriptRowsHandle,
  PlaybackTranscriptRowsProps
>(function PlaybackTranscriptRows(
  {
    turns,
    speakers,
    visualContext,
    visualContextUrls,
    search,
    selectedTurnId,
    autoScroll,
    initialPositionMs,
    playing,
    scrollRef,
    onSelectTurn,
    onEdit,
    onToggleMarker,
    onToggleReview,
  },
  forwardedRef,
) {
  const positionRef = useRef(initialPositionMs);
  const [cursor, setCursor] = useState(() =>
    playbackCursor(turns, initialPositionMs),
  );
  const cursorRef = useRef(cursor);

  const updatePosition = useCallback(
    (positionMs: number) => {
      positionRef.current = positionMs;
      const next = playbackCursor(turns, positionMs);
      const current = cursorRef.current;
      if (
        current.activeTurnId === next.activeTurnId &&
        current.activeWordId === next.activeWordId &&
        current.playedWordId === next.playedWordId
      ) {
        return;
      }
      cursorRef.current = next;
      setCursor(next);
    },
    [turns],
  );

  useImperativeHandle(
    forwardedRef,
    () => ({ updatePosition }),
    [updatePosition],
  );

  useEffect(() => {
    updatePosition(positionRef.current);
  }, [updatePosition]);

  useEffect(() => {
    if (!autoScroll || !playing || !cursor.activeTurnId) return;
    scrollRef.current
      ?.querySelector('[data-playback-active="true"]')
      ?.scrollIntoView({ block: "center", behavior: "smooth" });
  }, [autoScroll, cursor.activeTurnId, playing, scrollRef]);

  return (
    <TranscriptRows
      turns={turns}
      speakers={speakers}
      visualContext={visualContext}
      visualContextUrls={visualContextUrls}
      search={search}
      editable
      selectedTurnId={selectedTurnId}
      activeTurnId={cursor.activeTurnId}
      activeWordId={cursor.activeWordId}
      playedWordId={cursor.playedWordId}
      onSelectTurn={onSelectTurn}
      onEdit={onEdit}
      onToggleMarker={onToggleMarker}
      onToggleReview={onToggleReview}
    />
  );
});

export function TranscriptView({
  meeting,
  mediaSourceUrl,
  screenSourceUrl,
  allowSimulatedPlayback,
  turns,
  speakers,
  markers,
  visualContext,
  visualContextUrls,
  processingJob,
  profiles,
  profileSampleTargetId,
  chatMessages,
  agentStatus,
  agentLoading,
  agentError,
  onUpdateTurn,
  onToggleMarker,
  onToggleTurnReview,
  onRenameMeeting,
  onRenameSpeaker,
  onMergeSpeaker,
  onReviewSpeaker,
  onCancelJob,
  onRetryJob,
  onConfirmVoiceSample,
  onExport,
  onAskTranscript,
  onClearTranscriptChat,
  onRefreshAgentStatus,
}: TranscriptViewProps) {
  const initialPositionMs = allowSimulatedPlayback ? 767_000 : 0;
  const [search, setSearch] = useState("");
  const [playing, setPlaying] = useState(false);
  const [selectedTurn, setSelectedTurn] = useState<string | undefined>(
    () => turns[0]?.id,
  );
  const [autoScroll, setAutoScroll] = useState(true);
  const [speed, setSpeed] = useState(1);
  const [volume, setVolume] = useState(0.7);
  const [mediaDuration, setMediaDuration] = useState(meeting.durationMs);
  const [exportOpen, setExportOpen] = useState(false);
  const [profileOpen, setProfileOpen] = useState(false);
  const [profileId, setProfileId] = useState("");
  const [newProfileName, setNewProfileName] = useState("");
  const [savingProfile, setSavingProfile] = useState(false);
  const [replaceOpen, setReplaceOpen] = useState(false);
  const [findText, setFindText] = useState("");
  const [replaceText, setReplaceText] = useState("");
  const [renamingMeeting, setRenamingMeeting] = useState(false);
  const [meetingTitleDraft, setMeetingTitleDraft] = useState(meeting.title);
  const [sidePanel, setSidePanel] = useState<"ask" | "speakers">("ask");
  const searchRef = useRef<HTMLInputElement>(null);
  const audioRef = useRef<HTMLAudioElement>(null);
  const transcriptScrollRef = useRef<HTMLElement>(null);
  const speakerPanelRef = useRef<HTMLElement>(null);
  const playbackPositionRef = useRef(initialPositionMs);
  const mediaDurationRef = useRef(meeting.durationMs);
  const playbackTimeRef = useRef<HTMLSpanElement>(null);
  const waveformRef = useRef<WaveformHandle>(null);
  const playbackRowsRef = useRef<PlaybackTranscriptRowsHandle>(null);
  const selectedSpeakerId = turns.find(
    (turn) => turn.id === selectedTurn,
  )?.speakerId;

  useEffect(() => {
    setMeetingTitleDraft(meeting.title);
  }, [meeting.title]);

  useEffect(() => {
    if (!turns.length) {
      setSelectedTurn(undefined);
      return;
    }
    setSelectedTurn((current) =>
      current && turns.some((turn) => turn.id === current)
        ? current
        : turns[0].id,
    );
  }, [meeting.id, turns]);
  const selectedSpeaker = speakers.find(
    (speaker) => speaker.id === selectedSpeakerId,
  );
  const selectedTranscriptTurn = turns.find(
    (turn) => turn.id === selectedTurn,
  );
  const visualEvidenceBySpeaker = useMemo(() => {
    const turnSpeakerIds = new Map(
      turns.map((turn) => [turn.id, turn.speakerId]),
    );
    const evidence = new Map<string, VisualContextEvent>();
    for (const event of visualContext) {
      if (event.kind !== "speaker_evidence") continue;
      const speakerId = event.speakerId ?? turnSpeakerIds.get(event.turnId);
      if (speakerId && !evidence.has(speakerId)) evidence.set(speakerId, event);
    }
    return evidence;
  }, [turns, visualContext]);
  const stageOrder = [
    "ingest",
    "normalize",
    "transcribe",
    "align",
    "diarize",
    "identify",
    "index",
    "finalize",
  ];
  const stepStage = ["normalize", "transcribe", "align", "identify"];
  const currentStageIndex = processingJob
    ? stageOrder.indexOf(processingJob.stage)
    : -1;
  const pipelineState = (index: number) => {
    if (meeting.status === "ready" && !processingJob) return "completed";
    if (!processingJob) return "waiting";
    const targetStageIndex = stageOrder.indexOf(stepStage[index]);
    if (
      processingJob.state === "failed" &&
      currentStageIndex === targetStageIndex
    ) {
      return "failed";
    }
    if (currentStageIndex > targetStageIndex || processingJob.state === "completed") {
      return "completed";
    }
    if (currentStageIndex === targetStageIndex) return "running";
    return "waiting";
  };

  useEffect(() => {
    if (!profileSampleTargetId) return;
    setProfileId(profileSampleTargetId);
    setNewProfileName("");
    setProfileOpen(true);
  }, [profileSampleTargetId]);

  useEffect(() => {
    if (!selectedSpeakerId) return;
    const selectedCard = Array.from(
      speakerPanelRef.current?.querySelectorAll<HTMLElement>(
        "[data-speaker-id]",
      ) ?? [],
    ).find((card) => card.dataset.speakerId === selectedSpeakerId);
    selectedCard?.scrollIntoView?.({ block: "nearest", behavior: "smooth" });
  }, [selectedSpeakerId]);

  const syncPlaybackPosition = useCallback((positionMs: number) => {
    const durationMs = mediaDurationRef.current;
    const clamped = Math.max(0, Math.min(durationMs, positionMs));
    playbackPositionRef.current = clamped;
    if (playbackTimeRef.current) {
      playbackTimeRef.current.textContent = `${formatDuration(clamped)} / ${formatDuration(
        durationMs,
      )}`;
    }
    waveformRef.current?.setProgress(durationMs ? clamped / durationMs : 0);
    playbackRowsRef.current?.updatePosition(clamped);
  }, []);

  useEffect(() => {
    mediaDurationRef.current = mediaDuration;
    syncPlaybackPosition(playbackPositionRef.current);
  }, [mediaDuration, syncPlaybackPosition]);

  useEffect(() => {
    setMediaDuration(meeting.durationMs);
    mediaDurationRef.current = meeting.durationMs;
    syncPlaybackPosition(playbackPositionRef.current);
  }, [meeting.durationMs, syncPlaybackPosition]);

  useEffect(() => {
    syncPlaybackPosition(allowSimulatedPlayback ? 767_000 : 0);
    setPlaying(false);
  }, [allowSimulatedPlayback, meeting.id, syncPlaybackPosition]);

  useEffect(() => {
    if (!playing || !allowSimulatedPlayback || mediaSourceUrl) return;
    const timer = window.setInterval(() => {
      const next = playbackPositionRef.current + 100 * speed;
      if (next >= mediaDurationRef.current) {
        syncPlaybackPosition(0);
        setPlaying(false);
        return;
      }
      syncPlaybackPosition(next);
    }, 100);
    return () => window.clearInterval(timer);
  }, [allowSimulatedPlayback, mediaSourceUrl, playing, speed, syncPlaybackPosition]);

  useEffect(() => {
    if (!playing || !mediaSourceUrl || !audioRef.current) return;
    let frame = 0;
    const updatePlayback = () => {
      const media = audioRef.current;
      if (!media) return;
      syncPlaybackPosition(media.currentTime * 1000);
      frame = window.requestAnimationFrame(updatePlayback);
    };
    frame = window.requestAnimationFrame(updatePlayback);
    return () => window.cancelAnimationFrame(frame);
  }, [mediaSourceUrl, playing, syncPlaybackPosition]);

  useEffect(() => {
    const media = audioRef.current;
    if (!media) return;
    media.volume = volume;
    media.playbackRate = speed;
  }, [mediaSourceUrl, speed, volume]);

  useEffect(() => {
    if (mediaSourceUrl) {
      syncPlaybackPosition(0);
      setPlaying(false);
    }
  }, [mediaSourceUrl, syncPlaybackPosition]);

  const togglePlayback = useCallback(() => {
    const media = audioRef.current;
    if (mediaSourceUrl && media) {
      if (media.paused) {
        void media.play().catch(() => setPlaying(false));
      } else {
        media.pause();
      }
      return;
    }
    if (allowSimulatedPlayback) setPlaying((value) => !value);
  }, [allowSimulatedPlayback, mediaSourceUrl]);

  const seekTo = useCallback((nextMs: number) => {
    const clamped = Math.max(0, Math.min(mediaDurationRef.current, nextMs));
    syncPlaybackPosition(clamped);
    if (audioRef.current && mediaSourceUrl) {
      audioRef.current.currentTime = clamped / 1000;
    }
  }, [mediaSourceUrl, syncPlaybackPosition]);

  useEffect(() => {
    function handleKeyDown(event: KeyboardEvent) {
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "f") {
        event.preventDefault();
        searchRef.current?.focus();
      }
      if (
        event.code === "Space" &&
        !["INPUT", "TEXTAREA"].includes((event.target as HTMLElement).tagName)
      ) {
        event.preventDefault();
        togglePlayback();
      }
    }
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [togglePlayback]);

  const searchCount = useMemo(
    () =>
      search
        ? turns.filter((turn) =>
            (turn.editedText ?? turn.modelText)
              .toLocaleLowerCase()
              .includes(search.toLocaleLowerCase()),
          ).length
        : 0,
    [search, turns],
  );

  return (
    <main className="workspace transcript-workspace">
      {mediaSourceUrl ? (
        <audio
          ref={audioRef}
          src={mediaSourceUrl}
          preload="metadata"
          onLoadedMetadata={(event) => {
            const durationMs = event.currentTarget.duration * 1000;
            if (Number.isFinite(durationMs)) setMediaDuration(durationMs);
          }}
          onTimeUpdate={(event) =>
            syncPlaybackPosition(event.currentTarget.currentTime * 1000)
          }
          onPlay={() => setPlaying(true)}
          onPause={() => setPlaying(false)}
          onEnded={() => setPlaying(false)}
        />
      ) : null}
      <header className="meeting-header">
        <div className="meeting-title">
          {renamingMeeting ? (
            <form
              className="meeting-title__form"
              onSubmit={(event) => {
                event.preventDefault();
                const title = meetingTitleDraft.trim();
                if (title && title !== meeting.title) onRenameMeeting(title);
                setRenamingMeeting(false);
              }}
            >
              <input
                autoFocus
                aria-label="Meeting title"
                value={meetingTitleDraft}
                onChange={(event) => setMeetingTitleDraft(event.target.value)}
              />
              <button type="submit" disabled={!meetingTitleDraft.trim()}>
                Save
              </button>
              <button
                type="button"
                onClick={() => {
                  setMeetingTitleDraft(meeting.title);
                  setRenamingMeeting(false);
                }}
              >
                Cancel
              </button>
            </form>
          ) : (
            <>
              <h1>{meeting.title}</h1>
              <button
                type="button"
                aria-label="Rename meeting"
                onClick={() => setRenamingMeeting(true)}
              >
                <Pencil size={19} strokeWidth={1.6} />
              </button>
            </>
          )}
        </div>
        <div className="date-control" aria-label="Meeting date and time">
          <CalendarDays size={19} />
          <span>{formatLongDate(meeting.createdAt)}</span>
          <span>{formatMeetingTime(meeting.createdAt)}</span>
        </div>
        <div className="meeting-header__actions">
          <label className="transcript-search">
            <Search size={19} />
            <input
              ref={searchRef}
              value={search}
              onChange={(event) => setSearch(event.target.value)}
              placeholder="Search transcript"
              aria-label="Search transcript"
            />
            <kbd>{search ? `${searchCount} found` : "Ctrl+F"}</kbd>
          </label>
          <div
            className="offline-control"
            aria-label="Offline mode"
          >
            <Wifi size={20} />
            <span>Offline</span>
          </div>
        </div>
      </header>

      <section className="pipeline" aria-label="Processing progress">
        {pipelineSteps.map((step, index) => (
          <div
            className={`pipeline__step pipeline__step--${pipelineState(index)}`}
            key={step}
          >
            {pipelineState(index) === "completed" ? (
              <CheckCircle2 size={21} fill="#0a69dc" color="white" />
            ) : pipelineState(index) === "running" ? (
              <Gauge size={21} />
            ) : pipelineState(index) === "failed" ? (
              <X size={21} />
            ) : (
              <ListRestart size={21} />
            )}
            <span>
              <strong>{step}</strong>
              <small>
                {pipelineState(index) === "completed"
                  ? "Completed"
                  : pipelineState(index) === "running"
                    ? `${Math.round((processingJob?.progress ?? 0) * 100)}%`
                    : pipelineState(index) === "failed"
                      ? "Needs attention"
                      : "Waiting"}
              </small>
            </span>
            {index < pipelineSteps.length - 1 ? <i /> : null}
          </div>
        ))}
        {processingJob?.state === "failed" ? (
          <div className="pipeline__recovery" role="alert">
            <span>
              {processingJob.errorMessage ??
                processingJob.errorCode ??
                "Processing stopped."}
            </span>
            <button
              type="button"
              onClick={() => onRetryJob(processingJob.id)}
            >
              Retry
            </button>
          </div>
        ) : processingJob &&
          ["queued", "running", "retry_wait"].includes(processingJob.state) ? (
          <button
            className="pipeline__cancel"
            type="button"
            onClick={() => onCancelJob(processingJob.id)}
          >
            Cancel
          </button>
        ) : null}
      </section>

      <section className="player" aria-label="Transcript playback">
        <button
          className="player__skip"
          type="button"
          aria-label="Skip back 10 seconds"
          disabled={!allowSimulatedPlayback && !mediaSourceUrl}
          onClick={() => seekTo(playbackPositionRef.current - 10_000)}
        >
          <SkipBack size={20} fill="currentColor" />
        </button>
        <button
          className="player__play"
          type="button"
          aria-label={playing ? "Pause" : "Play"}
          disabled={!allowSimulatedPlayback && !mediaSourceUrl}
          onClick={togglePlayback}
        >
          {playing ? (
            <Pause size={21} fill="currentColor" />
          ) : (
            <Play size={21} fill="currentColor" />
          )}
        </button>
        <button
          className="player__skip"
          type="button"
          aria-label="Skip forward 10 seconds"
          disabled={!allowSimulatedPlayback && !mediaSourceUrl}
          onClick={() => seekTo(playbackPositionRef.current + 10_000)}
        >
          <SkipForward size={20} fill="currentColor" />
        </button>
        <span ref={playbackTimeRef} className="player__time">
          {formatDuration(initialPositionMs)} / {formatDuration(mediaDuration)}
        </span>
        <Waveform
          ref={waveformRef}
          progress={mediaDuration ? initialPositionMs / mediaDuration : 0}
          onSeek={(progress) => seekTo(progress * mediaDuration)}
        />
        <button
          className="player__speed"
          type="button"
          onClick={() =>
            setSpeed((current) =>
              current === 1 ? 1.25 : current === 1.25 ? 1.5 : 1,
            )
          }
          aria-label={`Playback speed ${speed} times`}
        >
          {speed.toFixed(1)}x <ChevronDown size={14} />
        </button>
        <Volume2 size={19} aria-hidden="true" />
        <input
          className="volume-slider"
          type="range"
          min={0}
          max={1}
          step={0.01}
          value={volume}
          aria-label="Playback volume"
          onChange={(event) => setVolume(Number(event.target.value))}
        />
        <div className="player-more">
          <button
            className="icon-button"
            type="button"
            aria-label="More playback options"
            aria-expanded={exportOpen}
            onClick={() => setExportOpen((open) => !open)}
          >
            <EllipsisVertical size={19} />
          </button>
          {exportOpen ? (
            <div className="export-menu">
              {markers.length ? (
                <>
                  <span className="export-menu__label">Markers</span>
                  {markers.map((marker) => (
                    <button
                      key={marker.id}
                      type="button"
                      onClick={() => {
                        seekTo(marker.atMs);
                        setExportOpen(false);
                      }}
                    >
                      {formatDuration(marker.atMs)} · {marker.label}
                    </button>
                  ))}
                  <span className="export-menu__label">Export</span>
                </>
              ) : null}
              {[
                ["txt", "Export plain text (.txt)"],
                ["md", "Export Markdown (.md)"],
                ["srt", "Export SubRip captions (.srt)"],
                ["vtt", "Export WebVTT captions (.vtt)"],
                ["json", "Export versioned data (.json)"],
              ].map(([format, label]) => (
                <button
                  key={format}
                  type="button"
                  onClick={() => {
                    onExport(format as ExportFormat);
                    setExportOpen(false);
                  }}
                >
                  {label}
                </button>
              ))}
            </div>
          ) : null}
        </div>
      </section>

      <div className="transcript-body">
        <section
          className="transcript-scroll"
          aria-label="Transcript"
          ref={transcriptScrollRef}
        >
          {screenSourceUrl ? (
            <section
              className="screen-recording-card"
              aria-labelledby="screen-recording-title"
            >
              <div className="screen-recording-card__heading">
                <span className="screen-recording-card__icon" aria-hidden="true">
                  <Monitor size={19} strokeWidth={1.8} />
                </span>
                <span>
                  <h2 id="screen-recording-title">Screen recording</h2>
                  <p>Main display · saved locally</p>
                </span>
              </div>
              <video
                className="screen-recording-card__video"
                src={screenSourceUrl}
                controls
                preload="metadata"
                playsInline
                aria-label={`Screen recording for ${meeting.title}`}
              >
                Screen recording playback is not supported on this device.
              </video>
            </section>
          ) : null}
          <PlaybackTranscriptRows
            ref={playbackRowsRef}
            turns={turns}
            speakers={speakers}
            visualContext={visualContext}
            visualContextUrls={visualContextUrls}
            search={search}
            selectedTurnId={selectedTurn}
            autoScroll={autoScroll}
            initialPositionMs={initialPositionMs}
            playing={playing}
            scrollRef={transcriptScrollRef}
            onSelectTurn={(turnId) => {
              setSelectedTurn(turnId);
              const turn = turns.find((candidate) => candidate.id === turnId);
              if (turn) seekTo(turn.startMs);
            }}
            onEdit={onUpdateTurn}
            onToggleMarker={onToggleMarker}
            onToggleReview={onToggleTurnReview}
          />
        </section>
        <aside
          className={`speaker-panel ${
            sidePanel === "ask" ? "speaker-panel--assistant" : ""
          }`}
          aria-label={sidePanel === "ask" ? "Ask this transcript" : "Meeting speakers"}
          ref={speakerPanelRef}
        >
          {sidePanel === "ask" ? (
            <TranscriptAssistantPanel
              meetingTitle={meeting.title}
              messages={chatMessages}
              status={agentStatus}
              loading={agentLoading}
              error={agentError}
              transcriptReady={meeting.status === "ready" && turns.length > 0}
              onOpenSpeakers={() => setSidePanel("speakers")}
              onAsk={onAskTranscript}
              onClear={onClearTranscriptChat}
              onRefreshStatus={onRefreshAgentStatus}
              onOpenCitation={(citation) => {
                setSelectedTurn(citation.turnId);
                seekTo(citation.startMs);
                window.setTimeout(() => {
                  transcriptScrollRef.current
                    ?.querySelector(`[data-turn-id="${citation.turnId}"]`)
                    ?.scrollIntoView?.({ block: "center", behavior: "smooth" });
                }, 0);
              }}
            />
          ) : (
            <>
          <div className="side-panel-tabs" role="tablist" aria-label="Transcript tools">
            <button
              type="button"
              role="tab"
              aria-selected="false"
              onClick={() => setSidePanel("ask")}
            >
              Ask
            </button>
            <button
              type="button"
              className="is-active"
              role="tab"
              aria-selected="true"
            >
              Speakers
            </button>
          </div>
          <div className="panel-heading">
            <h2>Speakers</h2>
            <ChevronDown size={18} />
          </div>
          {speakers.map((speaker) => (
            <SpeakerCard
              key={speaker.id}
              speaker={speaker}
              allSpeakers={speakers}
              selected={speaker.id === selectedSpeakerId}
              suggestedProfileName={
                profiles.find((profile) => profile.id === speaker.profileId)?.name
              }
              visualEvidence={visualEvidenceBySpeaker.get(speaker.id)}
              onRename={(name) => onRenameSpeaker(speaker.id, name)}
              onMerge={(targetId) => onMergeSpeaker(speaker.id, targetId)}
              onAcceptReview={() => onReviewSpeaker(speaker.id, true)}
              onRejectReview={() => onReviewSpeaker(speaker.id, false)}
            />
          ))}
          <button
            className="create-profile"
            type="button"
            aria-expanded={profileOpen}
            disabled={!selectedSpeakerId}
            onClick={() => setProfileOpen((open) => !open)}
          >
            <span>
              <Sparkles size={20} />
            </span>
            <span>
              <strong>Create voice profile</strong>
              <small>Save a new voice profile from selected segments.</small>
            </span>
            <ChevronRight size={19} />
          </button>
          {profileOpen ? (
            <form
              className="profile-sample-form"
              onSubmit={(event) => {
                event.preventDefault();
                if (!selectedSpeakerId || (!profileId && !newProfileName.trim())) {
                  return;
                }
                setSavingProfile(true);
                void onConfirmVoiceSample(
                  selectedSpeakerId,
                  profileId || undefined,
                  profileId ? undefined : newProfileName.trim(),
                )
                  .then(() => {
                    setProfileOpen(false);
                    setNewProfileName("");
                  })
                  .catch(() => undefined)
                  .finally(() => setSavingProfile(false));
              }}
            >
              <strong>
                Confirm sample from {selectedSpeaker?.displayName ?? "selected speaker"}
              </strong>
              <p>
                {selectedSpeakerId
                  ? "Only clean, non-overlapping speech from this speaker cluster is saved."
                  : "Select a transcript turn first; only clean, non-overlapping speech will be saved."}
              </p>
              <label>
                Voice profile
                <select
                  aria-label="Voice profile"
                  value={profileId}
                  onChange={(event) => setProfileId(event.target.value)}
                >
                  <option value="">Create a new profile…</option>
                  {profiles.map((profile) => (
                    <option key={profile.id} value={profile.id}>
                      {profile.name}
                    </option>
                  ))}
                </select>
              </label>
              {!profileId ? (
                <label>
                  Speaker name
                  <input
                    aria-label="New profile speaker name"
                    value={newProfileName}
                    onChange={(event) => setNewProfileName(event.target.value)}
                    placeholder="Full name"
                  />
                </label>
              ) : null}
              <div>
                <button
                  className="profile-sample-form__confirm"
                  type="submit"
                  disabled={
                    savingProfile ||
                    !selectedSpeakerId ||
                    (!profileId && !newProfileName.trim())
                  }
                >
                  {savingProfile ? "Saving…" : "Confirm clean sample"}
                </button>
                <button
                  type="button"
                  onClick={() => setProfileOpen(false)}
                  disabled={savingProfile}
                >
                  Cancel
                </button>
              </div>
            </form>
          ) : null}
          <div className="speaker-info">
            <Info size={18} />
            <p>
              Names are only assigned when a voice clears SayTrace’s
              strict match thresholds.
            </p>
          </div>
            </>
          )}
        </aside>
      </div>

      <footer className="transcript-toolbar">
        <div>
          <button
            type="button"
            disabled={!selectedTurn}
            onClick={() => selectedTurn && onToggleMarker(selectedTurn)}
          >
            <Bookmark size={19} />
            {selectedTranscriptTurn?.isMarked
              ? "Remove bookmark"
              : "Add bookmark"}
          </button>
          <button
            type="button"
            disabled={!selectedTurn}
            onClick={() => selectedTurn && onToggleTurnReview(selectedTurn)}
          >
            <MessageSquare size={19} />
            {selectedTranscriptTurn?.needsReview
              ? "Clear review flag"
              : "Flag for review"}
          </button>
          <button
            type="button"
            aria-expanded={replaceOpen}
            onClick={() => setReplaceOpen((open) => !open)}
          >
            <Search size={19} />
            Find and replace
          </button>
        </div>
        <label>
          <input
            type="checkbox"
            checked={autoScroll}
            onChange={(event) => setAutoScroll(event.target.checked)}
          />
          <span>
            <Check size={15} />
          </span>
          Auto-scroll
        </label>
        {replaceOpen ? (
          <form
            className="find-replace"
            onSubmit={(event) => {
              event.preventDefault();
              if (!findText) return;
              turns.forEach((turn) => {
                const current = turn.editedText ?? turn.modelText;
                if (current.includes(findText)) {
                  onUpdateTurn(
                    turn.id,
                    current.split(findText).join(replaceText),
                  );
                }
              });
              setReplaceOpen(false);
            }}
          >
            <input
              aria-label="Find text"
              value={findText}
              onChange={(event) => setFindText(event.target.value)}
              placeholder="Find"
            />
            <input
              aria-label="Replacement text"
              value={replaceText}
              onChange={(event) => setReplaceText(event.target.value)}
              placeholder="Replace with"
            />
            <button type="submit" disabled={!findText}>
              Replace all
            </button>
            <button type="button" onClick={() => setReplaceOpen(false)}>
              Cancel
            </button>
          </form>
        ) : null}
      </footer>
    </main>
  );
}
