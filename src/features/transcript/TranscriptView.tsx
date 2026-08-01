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
import { useEffect, useMemo, useRef, useState } from "react";
import {
  formatDuration,
  formatLongDate,
  formatMeetingTime,
} from "../../lib/format";
import type {
  Meeting,
  MeetingSpeaker,
  Marker,
  ProcessingJob,
  SpeakerState,
  TranscriptTurn,
  VoiceProfile,
} from "../../types";
import { SpeakerAvatar } from "../../components/SpeakerAvatar";
import { TranscriptRows } from "../../components/TranscriptRows";
import { Waveform } from "../../components/Waveform";

type ExportFormat = "txt" | "md" | "srt" | "vtt" | "json";

interface TranscriptViewProps {
  meeting: Meeting;
  mediaSourceUrl?: string;
  allowSimulatedPlayback: boolean;
  turns: TranscriptTurn[];
  speakers: MeetingSpeaker[];
  markers: Marker[];
  processingJob?: ProcessingJob;
  profiles: VoiceProfile[];
  profileSampleTargetId?: string;
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
  onRename: (name: string) => void;
  onMerge: (targetId: string) => void;
  suggestedProfileName?: string;
  onAcceptReview: () => void;
  onRejectReview: () => void;
}

function SpeakerCard({
  speaker,
  allSpeakers,
  onRename,
  onMerge,
  suggestedProfileName,
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

  return (
    <div className="speaker-card">
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
            disabled={!suggestedProfileName}
            onClick={onAcceptReview}
          >
            <Check size={16} />{" "}
            {suggestedProfileName
              ? `Accept ${suggestedProfileName}`
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

export function TranscriptView({
  meeting,
  mediaSourceUrl,
  allowSimulatedPlayback,
  turns,
  speakers,
  markers,
  processingJob,
  profiles,
  profileSampleTargetId,
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
}: TranscriptViewProps) {
  const [search, setSearch] = useState("");
  const [playing, setPlaying] = useState(false);
  const [position, setPosition] = useState(767_000);
  const [selectedTurn, setSelectedTurn] = useState<string | undefined>("turn-4");
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
  const searchRef = useRef<HTMLInputElement>(null);
  const audioRef = useRef<HTMLAudioElement>(null);
  const transcriptScrollRef = useRef<HTMLElement>(null);
  const selectedSpeakerId = turns.find(
    (turn) => turn.id === selectedTurn,
  )?.speakerId;

  useEffect(() => {
    setMeetingTitleDraft(meeting.title);
  }, [meeting.title]);
  const selectedSpeaker = speakers.find(
    (speaker) => speaker.id === selectedSpeakerId,
  );
  const selectedTranscriptTurn = turns.find(
    (turn) => turn.id === selectedTurn,
  );
  const activeTurnId = useMemo(() => {
    const active = turns.find(
      (turn) => position >= turn.startMs && position < turn.endMs,
    );
    if (active) return active.id;
    const earlierTurns = turns.filter((turn) => turn.startMs <= position);
    return earlierTurns[earlierTurns.length - 1]?.id;
  }, [position, turns]);
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
    if (!playing || !allowSimulatedPlayback || mediaSourceUrl) return;
    const timer = window.setInterval(
      () =>
        setPosition((current) => {
          if (current >= meeting.durationMs) {
            setPlaying(false);
            return 0;
          }
          return current + 500 * speed;
        }),
      500,
    );
    return () => window.clearInterval(timer);
  }, [allowSimulatedPlayback, mediaSourceUrl, playing, meeting.durationMs, speed]);

  useEffect(() => {
    const media = audioRef.current;
    if (!media) return;
    media.volume = volume;
    media.playbackRate = speed;
  }, [mediaSourceUrl, speed, volume]);

  useEffect(() => {
    if (mediaSourceUrl) {
      setPosition(0);
      setPlaying(false);
    }
  }, [mediaSourceUrl]);

  useEffect(() => {
    if (!autoScroll || !playing || !activeTurnId) return;
    transcriptScrollRef.current
      ?.querySelector('[data-playback-active="true"]')
      ?.scrollIntoView({ block: "center", behavior: "smooth" });
  }, [activeTurnId, autoScroll, playing]);

  function togglePlayback() {
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
  }

  function seekTo(nextMs: number) {
    const clamped = Math.max(0, Math.min(mediaDuration, nextMs));
    setPosition(clamped);
    if (audioRef.current && mediaSourceUrl) {
      audioRef.current.currentTime = clamped / 1000;
    }
  }

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
  }, [allowSimulatedPlayback, mediaSourceUrl, mediaDuration]);

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
            setPosition(event.currentTarget.currentTime * 1000)
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
          onClick={() => seekTo(position - 10_000)}
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
          onClick={() => seekTo(position + 10_000)}
        >
          <SkipForward size={20} fill="currentColor" />
        </button>
        <span className="player__time">
          {formatDuration(position)} / {formatDuration(mediaDuration)}
        </span>
        <Waveform
          progress={mediaDuration ? position / mediaDuration : 0}
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
          <TranscriptRows
            turns={turns}
            speakers={speakers}
            search={search}
            editable
            selectedTurnId={selectedTurn}
            activeTurnId={activeTurnId}
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
        <aside className="speaker-panel" aria-label="Meeting speakers">
          <div className="panel-heading">
            <h2>Speakers</h2>
            <ChevronDown size={18} />
          </div>
          {speakers.map((speaker) => (
            <SpeakerCard
              key={speaker.id}
              speaker={speaker}
              allSpeakers={speakers}
              suggestedProfileName={
                profiles.find((profile) => profile.id === speaker.profileId)?.name
              }
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
