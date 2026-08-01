import {
  Bookmark,
  ChevronDown,
  HardDrive,
  Info,
  Mic,
  MonitorSpeaker,
  MoreVertical,
  Pause,
  Pencil,
  Play,
  Plus,
  Square,
  Volume2,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { LevelMeter } from "../../components/LevelMeter";
import { TranscriptRows } from "../../components/TranscriptRows";
import { formatDuration } from "../../lib/format";
import type {
  AudioDevice,
  Marker,
  Meeting,
  MeetingSpeaker,
  RecordingSession,
  RecordingLevels,
  RecordingStatus,
  TranscriptTurn,
} from "../../types";

interface RecordingViewProps {
  meeting: Meeting;
  devices: AudioDevice[];
  turns: TranscriptTurn[];
  speakers: MeetingSpeaker[];
  markers: Marker[];
  session?: RecordingSession;
  status: RecordingStatus;
  levels: RecordingLevels;
  microphoneDeviceId: string;
  outputDeviceId: string;
  microphoneIsPersonal: boolean;
  liveCaptionsEnabled: boolean;
  availableStorageGb: number;
  onRenameMeeting: (title: string) => void;
  onTogglePause: () => void;
  onAddMarker: (label: string, atMs: number) => void;
  onStop: () => void;
}

export function RecordingView({
  meeting,
  devices,
  turns,
  speakers,
  markers,
  session,
  status,
  levels,
  microphoneDeviceId,
  outputDeviceId,
  microphoneIsPersonal,
  liveCaptionsEnabled,
  availableStorageGb,
  onRenameMeeting,
  onTogglePause,
  onAddMarker,
  onStop,
}: RecordingViewProps) {
  const paused = session?.state === "paused";
  const microphoneActive = status.microphoneActive && !paused;
  const systemAudioActive = status.systemAudioActive && !paused;
  const [elapsed, setElapsed] = useState(
    session?.elapsedMs ?? (session ? 0 : 1_122_000),
  );
  const [markerDraft, setMarkerDraft] = useState("");
  const [addingMarker, setAddingMarker] = useState(false);
  const [autoScroll, setAutoScroll] = useState(true);
  const [renamingMeeting, setRenamingMeeting] = useState(false);
  const [meetingTitleDraft, setMeetingTitleDraft] = useState(meeting.title);
  const liveDraftScrollRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (session) setElapsed(session.elapsedMs);
  }, [session?.elapsedMs]);

  useEffect(() => {
    setMeetingTitleDraft(meeting.title);
  }, [meeting.title]);

  useEffect(() => {
    if (!autoScroll) return;
    const scrollArea = liveDraftScrollRef.current;
    if (scrollArea) scrollArea.scrollTop = scrollArea.scrollHeight;
  }, [autoScroll, turns]);

  useEffect(() => {
    if (paused) return;
    const timer = window.setInterval(
      () => setElapsed((current) => current + 1_000),
      1_000,
    );
    return () => window.clearInterval(timer);
  }, [paused]);

  function saveMarker() {
    const label = markerDraft.trim() || `Marker ${markers.length + 1}`;
    onAddMarker(label, elapsed);
    setMarkerDraft("");
    setAddingMarker(false);
  }

  return (
    <main
      className="workspace recording-workspace"
      data-microphone-mode={microphoneIsPersonal ? "personal" : "room"}
    >
      <header className="recording-header">
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
        <div className="recording-timer" aria-label="Recording duration">
          <span aria-hidden="true" />
          <strong>{formatDuration(elapsed)}</strong>
        </div>
        <button className="stop-button" type="button" onClick={onStop}>
          <Square size={13} fill="currentColor" />
          Stop and finalize
        </button>
        <button
          className="pause-button"
          type="button"
          aria-label={paused ? "Resume recording" : "Pause recording"}
          onClick={onTogglePause}
        >
          {paused ? <Play size={18} /> : <Pause size={18} fill="currentColor" />}
          {paused ? "Resume" : "Pause"}
        </button>
      </header>

      <div className="recording-main">
        <section className="recording-center">
          <div className="source-row">
            <Mic className="source-row__icon" size={24} />
            <label>
              <span className="sr-only">Microphone input</span>
              <select
                aria-label="Active microphone input"
                value={microphoneDeviceId}
                disabled
                title="Choose the recording device before capture starts"
              >
                {devices
                  .filter((device) => device.kind === "input")
                  .map((device) => (
                    <option key={device.id} value={device.id}>
                      {device.name}
                    </option>
                  ))}
              </select>
              <ChevronDown size={16} />
            </label>
            <LevelMeter level={paused ? 0 : levels.microphone} />
            <Volume2 size={18} />
            <input
              className="volume-slider"
              type="range"
              min={0}
              max={1}
              step={0.01}
              defaultValue={0.74}
              disabled
              aria-label="Microphone monitor level (controlled by Windows)"
            />
          </div>
          <div className="source-row">
            <MonitorSpeaker className="source-row__icon" size={24} />
            <label>
              <span className="sr-only">System audio output</span>
              <select
                aria-label="Active system audio output"
                value={outputDeviceId}
                disabled
                title="Choose the recording device before capture starts"
              >
                {devices
                  .filter((device) => device.kind === "output")
                  .map((device) => (
                    <option key={device.id} value={device.id}>
                      {device.name}
                    </option>
                  ))}
              </select>
              <ChevronDown size={16} />
            </label>
            <LevelMeter level={paused ? 0 : levels.system} />
            <Volume2 size={18} />
            <input
              className="volume-slider"
              type="range"
              min={0}
              max={1}
              step={0.01}
              defaultValue={0.66}
              disabled
              aria-label="System audio monitor level (controlled by Windows)"
            />
          </div>

          <section className="live-draft" aria-label="Live draft captions">
            <header>
              <h2>Live draft</h2>
              <Info size={17} />
              <p>
                {liveCaptionsEnabled
                  ? "Speaker names and timing are finalized after recording stops."
                  : "Live captions are off; final processing still starts from saved media."}
              </p>
            </header>
            <div className="live-draft__scroll" ref={liveDraftScrollRef}>
              {liveCaptionsEnabled ? (
                <TranscriptRows turns={turns} speakers={speakers} />
              ) : (
                <p className="live-draft__disabled">
                  Live draft captions are off for this recording.
                </p>
              )}
            </div>
            <footer>
              <span>
                <Info size={17} />
                {liveCaptionsEnabled
                  ? "Draft captions may change during final processing"
                  : "The saved tracks will be transcribed after you stop"}
              </span>
              <label>
                <input
                  type="checkbox"
                  checked={autoScroll}
                  onChange={(event) => setAutoScroll(event.target.checked)}
                />
                <span>✓</span> Auto-scroll
              </label>
            </footer>
          </section>
        </section>

        <aside className="recording-side">
          <section className="recording-health">
            <header>
              <h2>Recording</h2>
              <ChevronDown size={18} />
            </header>
            <ul>
              <li className={microphoneActive || paused ? undefined : "is-failed"}>
                <span /> Microphone{" "}
                {paused ? "paused" : microphoneActive ? "active" : "needs attention"}
              </li>
              <li className={systemAudioActive || paused ? undefined : "is-failed"}>
                <span /> System audio{" "}
                {paused ? "paused" : systemAudioActive ? "active" : "needs attention"}
              </li>
              <li>
                <span /> Saved locally
              </li>
            </ul>
            {status.warning ? (
              <p className="recording-health__warning" role="alert">
                {status.warning}
              </p>
            ) : null}
            <div>
              <HardDrive size={20} />
              {availableStorageGb.toFixed(1)} GB available
            </div>
          </section>

          {addingMarker ? (
            <form
              className="add-marker-form"
              onSubmit={(event) => {
                event.preventDefault();
                saveMarker();
              }}
            >
              <input
                autoFocus
                value={markerDraft}
                onChange={(event) => setMarkerDraft(event.target.value)}
                placeholder="Marker label"
                aria-label="Marker label"
              />
              <button type="submit">Save</button>
              <button type="button" onClick={() => setAddingMarker(false)}>
                Cancel
              </button>
            </form>
          ) : (
            <button
              className="add-marker"
              type="button"
              onClick={() => setAddingMarker(true)}
            >
              <Plus size={22} /> Add marker
            </button>
          )}

          <section className="markers">
            <h3>Markers</h3>
            {markers.map((marker) => (
              <div key={marker.id}>
                <Bookmark size={19} />
                <span>{formatDuration(marker.atMs)}</span>
                <strong>{marker.label}</strong>
                <MoreVertical size={17} aria-hidden="true" />
              </div>
            ))}
          </section>
        </aside>
      </div>
    </main>
  );
}
