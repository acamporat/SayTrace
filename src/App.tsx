import { useEffect, useMemo, useRef, useState } from "react";
import { NewTranscriptionDialog } from "./components/NewTranscriptionDialog";
import { Sidebar } from "./components/Sidebar";
import { Titlebar } from "./components/Titlebar";
import { Toast, type ToastMessage } from "./components/Toast";
import {
  devices as mockDevices,
  draftSpeakers,
  draftTurns,
  markers as seedMarkers,
  meetings as seedMeetings,
  modelStatus as seedModelStatus,
  speakers as seedSpeakers,
  transcriptTurns as seedTurns,
  voiceProfiles as seedProfiles,
} from "./data/mock";
import { LibraryView } from "./features/library/LibraryView";
import { VoiceProfilesView } from "./features/profiles/VoiceProfilesView";
import { RecordingView } from "./features/recording/RecordingView";
import { SettingsView } from "./features/settings/SettingsView";
import { ModelSetupView } from "./features/setup/ModelSetupView";
import { TranscriptView } from "./features/transcript/TranscriptView";
import {
  createAssetObjectUrl,
  invokeCommand,
  isTauriRuntime,
  listenEvent,
} from "./lib/tauri";
import {
  applyDraftRevision,
  isDraftRevisionEvent,
  upsertDraftSpeaker,
} from "./lib/liveDraft";
import type {
  AudioDevice,
  AppStatus,
  AppView,
  Marker,
  Meeting,
  MeetingSpeaker,
  ModelPackStatus,
  ModelSetupProgressEvent,
  ProcessingJob,
  RecordingStatus,
  RecordingSession,
  RecordingLevels,
  TranscriptTurn,
  VoiceProfile,
} from "./types";

type ExportFormat = "txt" | "md" | "srt" | "vtt" | "json";

function safeMeetingStatus(
  meeting: Meeting,
): "ready" | "processing" | "recording" | "failed" {
  if (meeting.status === "recording") return "recording";
  if (meeting.status === "failed") return "failed";
  if (meeting.status === "processing" || meeting.status === "importing") {
    return "processing";
  }
  return "ready";
}

function normalizeMeeting(meeting: Meeting): Meeting {
  return {
    ...meeting,
    sourceType: meeting.sourceType ?? meeting.sourceKind ?? "import",
    status: safeMeetingStatus(meeting),
    durationMs: meeting.durationMs ?? 0,
    speakerCount: meeting.speakerCount ?? 0,
  };
}

function normalizeSpeakers(raw: MeetingSpeaker[]): MeetingSpeaker[] {
  const colors = ["#3aa66f", "#8052ca", "#169c9d", "#d57b26"];
  return raw.map((speaker, index) => ({
    ...speaker,
    color: speaker.color || colors[index % colors.length],
    initials:
      speaker.initials ||
      speaker.displayName
        .split(/\s+/)
        .slice(0, 2)
        .map((part) => part[0]?.toUpperCase())
        .join(""),
  }));
}

function transcriptText(
  title: string,
  turns: TranscriptTurn[],
  speakers: MeetingSpeaker[],
  format: ExportFormat,
) {
  if (format === "json") {
    return JSON.stringify(
      {
        schemaVersion: 1,
        exportedAt: new Date().toISOString(),
        meeting: { title },
        speakers,
        turns,
      },
      null,
      2,
    );
  }
  const lines = turns.map((turn) => {
    const speaker =
      speakers.find((candidate) => candidate.id === turn.speakerId)?.displayName ??
      "Unknown speaker";
    return `${speaker}: ${turn.editedText ?? turn.modelText}`;
  });
  if (format === "md") return `# ${title}\n\n${lines.join("\n\n")}\n`;
  return lines.join("\n\n");
}

export default function App() {
  const desktopRuntime = isTauriRuntime();
  const [view, setView] = useState<AppView>(
    desktopRuntime
      ? { kind: "library" }
      : { kind: "transcript", meetingId: "weekly-production" },
  );
  const [meetings, setMeetings] = useState<Meeting[]>(
    desktopRuntime ? [] : seedMeetings,
  );
  const [turns, setTurns] = useState<TranscriptTurn[]>(
    desktopRuntime ? [] : seedTurns,
  );
  const [speakers, setSpeakers] = useState<MeetingSpeaker[]>(
    desktopRuntime ? [] : seedSpeakers,
  );
  const [profiles, setProfiles] = useState<VoiceProfile[]>(
    desktopRuntime ? [] : seedProfiles,
  );
  const [markers, setMarkers] = useState<Marker[]>(
    desktopRuntime ? [] : seedMarkers,
  );
  const [audioDevices, setAudioDevices] = useState<AudioDevice[]>(
    desktopRuntime ? [] : mockDevices,
  );
  const [modelStatus, setModelStatus] = useState<ModelPackStatus>(
    desktopRuntime
      ? {
          runtime: "checking",
          liveModel: "missing",
          finalModel: "missing",
          diarizationModel: "missing",
          device: "Detecting local hardware…",
          diskRequiredGb: 0,
          diskAvailableGb: 0,
        }
      : seedModelStatus,
  );
  const [modelSetupProgress, setModelSetupProgress] =
    useState<ModelSetupProgressEvent>();
  const [recordingSession, setRecordingSession] =
    useState<RecordingSession>();
  const [recordingStatus, setRecordingStatus] = useState<RecordingStatus>({
    state: "idle",
    elapsedMs: 0,
    microphoneActive: !desktopRuntime,
    systemAudioActive: !desktopRuntime,
    microphoneLevel: 0,
    systemAudioLevel: 0,
    droppedCapturePackets: 0,
    droppedCaptionChunks: 0,
  });
  const [liveDraftTurns, setLiveDraftTurns] = useState<TranscriptTurn[]>([]);
  const [liveDraftSpeakers, setLiveDraftSpeakers] = useState<
    MeetingSpeaker[]
  >([]);
  const [recordingLevels, setRecordingLevels] = useState<RecordingLevels>({
    microphone: desktopRuntime ? 0 : 0.62,
    system: desktopRuntime ? 0 : 0.59,
  });
  const [recordingDevices, setRecordingDevices] = useState({
    microphoneDeviceId: "",
    outputDeviceId: "",
    microphoneIsPersonal: true,
    liveCaptions: true,
  });
  const [meetingRefreshToken, setMeetingRefreshToken] = useState(0);
  const [jobs, setJobs] = useState<ProcessingJob[]>([]);
  const [workerStatus, setWorkerStatus] = useState<AppStatus["worker"]>({
    state: desktopRuntime ? "checking" : "ready",
    protocolVersion: 1,
    pipelineVersion: "2026.07.28.1",
  });
  const [mediaUrl, setMediaUrl] = useState<string>();
  const [newDialogOpen, setNewDialogOpen] = useState(false);
  const [profileSampleTargetId, setProfileSampleTargetId] =
    useState<string>();
  const [toast, setToast] = useState<ToastMessage>();
  const fileInputRef = useRef<HTMLInputElement>(null);
  const toastSequence = useRef(0);
  const recordingSessionRef = useRef<RecordingSession>();

  const selectedMeeting = useMemo(() => {
    if (view.kind !== "transcript" && view.kind !== "recording") {
      return meetings[0];
    }
    return (
      meetings.find((meeting) => meeting.id === view.meetingId) ?? meetings[0]
    );
  }, [meetings, view]);

  function notify(
    message: string,
    tone: ToastMessage["tone"] = "success",
  ) {
    const next = { id: ++toastSequence.current, message, tone };
    setToast(next);
    window.setTimeout(
      () => setToast((current) => (current?.id === next.id ? undefined : current)),
      3_600,
    );
  }

  useEffect(() => {
    if (!desktopRuntime) return;
    let alive = true;
    void invokeCommand("get_app_status")
      .then((status) => {
        if (!alive) return;
        setWorkerStatus(status.worker);
        setRecordingStatus(status.activeRecording);
        if (status.firstRun && !status.modelReady) {
          setModelStatus((current) => ({
            ...current,
            runtime: "missing",
            liveModel: "missing",
            finalModel: "missing",
            diarizationModel: "missing",
            device:
              String(status.capabilities?.gpu ?? "") || current.device,
          }));
          setView({ kind: "setup" });
        }
      })
      .catch(() => undefined);
    void invokeCommand("get_model_status")
      .then((status) => {
        if (alive) setModelStatus(status);
      })
      .catch(() => undefined);
    void invokeCommand("list_meetings")
      .then((desktopMeetings) => {
        if (alive) setMeetings(desktopMeetings.map(normalizeMeeting));
      })
      .catch(() => {
        notify("The local library could not be loaded yet.", "warning");
      });
    void invokeCommand("list_voice_profiles")
      .then((result) => {
        if (alive) setProfiles(result);
      })
      .catch(() => undefined);
    void invokeCommand("list_processing_jobs", {})
      .then((result) => {
        if (alive) setJobs(result);
      })
      .catch(() => undefined);
    void invokeCommand("list_audio_devices")
      .then((result) => {
        if (alive) setAudioDevices(result);
      })
      .catch(() => undefined);

    const unlisten: Array<() => void> = [];
    void listenEvent("transcript://draft-revision", (event) => {
      if (!isDraftRevisionEvent(event)) return;
      const activeSession = recordingSessionRef.current;
      if (!activeSession || event.session_id !== activeSession.id) return;
      setLiveDraftTurns((current) =>
        applyDraftRevision(current, event, activeSession.elapsedMs),
      );
      setLiveDraftSpeakers((current) => upsertDraftSpeaker(current, event));
    }).then((dispose) => unlisten.push(dispose));
    void listenEvent("device://warning", (event) => {
      notify(event.message, "warning");
      setRecordingStatus((current) => {
        const microphoneFailed = event.deviceId === "microphone";
        const systemFailed =
          event.deviceId === "loopback" || event.deviceId === "system";
        const microphoneActive = microphoneFailed
          ? false
          : current.microphoneActive;
        const systemAudioActive = systemFailed
          ? false
          : current.systemAudioActive;
        return {
          ...current,
          state:
            !microphoneActive && !systemAudioActive ? "failed" : current.state,
          microphoneActive,
          systemAudioActive,
          warning: event.message,
        };
      });
    }).then((dispose) => unlisten.push(dispose));
    void listenEvent("job://progress", ({ job }) => {
      setJobs((current) => [
        job,
        ...current.filter((candidate) => candidate.id !== job.id),
      ]);
      if (job.state === "completed") {
        notify("Final transcript is ready.");
      } else if (job.state === "failed") {
        notify(
          job.errorMessage ?? "Final transcript processing needs attention.",
          "warning",
        );
      }
      if (["completed", "failed", "cancelled"].includes(job.state)) {
        setMeetingRefreshToken((current) => current + 1);
        void invokeCommand("list_meetings")
          .then((desktopMeetings) =>
            setMeetings(desktopMeetings.map(normalizeMeeting)),
          )
          .catch(() => undefined);
      }
    }).then((dispose) => unlisten.push(dispose));
    void listenEvent("model://setup-progress", (progress) => {
      setModelSetupProgress(progress);
    }).then((dispose) => unlisten.push(dispose));
    void listenEvent("worker://health", (event) => {
      setWorkerStatus((current) => ({
        ...current,
        state: event.status,
        error: event.status === "offline" ? "Worker is offline." : undefined,
      }));
    }).then((dispose) => unlisten.push(dispose));
    void listenEvent("meeting://changed", () => {
      setMeetingRefreshToken((current) => current + 1);
      void invokeCommand("list_meetings")
        .then((desktopMeetings) =>
          setMeetings(desktopMeetings.map(normalizeMeeting)),
        )
        .catch(() => undefined);
    }).then((dispose) => unlisten.push(dispose));
    void listenEvent("recording://state", (session) => {
      recordingSessionRef.current = session;
      setRecordingSession(session);
      setRecordingStatus((current) => ({
        ...current,
        state: session.state,
        sessionId: session.id,
        meetingId: session.meetingId,
        elapsedMs: session.elapsedMs,
      }));
    }).then((dispose) => unlisten.push(dispose));
    void listenEvent("recording://levels", (levels) => {
      setRecordingLevels(levels);
    }).then((dispose) => unlisten.push(dispose));
    return () => {
      alive = false;
      unlisten.forEach((dispose) => dispose());
    };
  }, [desktopRuntime]);

  useEffect(() => {
    if (!desktopRuntime || view.kind !== "transcript") {
      setMediaUrl((current) => {
        if (current) URL.revokeObjectURL(current);
        return undefined;
      });
      return;
    }
    let alive = true;
    let objectUrl: string | undefined;
    setTurns([]);
    setSpeakers([]);
    setMarkers([]);
    setMediaUrl(undefined);
    void invokeCommand("get_meeting", { meetingId: view.meetingId })
      .then(async (detail) => {
        if (!alive) return;
        const meeting = normalizeMeeting(detail.meeting);
        setMeetings((current) =>
          current.map((candidate) =>
            candidate.id === meeting.id ? meeting : candidate,
          ),
        );
        setTurns(detail.turns);
        setSpeakers(normalizeSpeakers(detail.speakers));
        setMarkers(detail.markers);
        const preferredAsset =
          detail.assets.find(
            (asset) => asset.kind === "playback" || asset.kind === "mixed",
          ) ??
          detail.assets.find(
            (asset) =>
              asset.mediaKind === "audio" ||
              asset.contentType?.startsWith("audio/"),
          ) ??
          detail.assets[0];
        if (preferredAsset) {
          try {
            objectUrl = await createAssetObjectUrl(preferredAsset.id);
            if (alive) setMediaUrl(objectUrl);
          } catch {
            if (alive) {
              notify(
                "Playback media is unavailable while processing continues.",
                "info",
              );
            }
          }
        }
      })
      .catch(() => {
        if (alive) notify("Meeting detail could not be loaded.", "warning");
      });
    return () => {
      alive = false;
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [desktopRuntime, meetingRefreshToken, view]);

  async function importMedia() {
    setNewDialogOpen(false);
    if (!desktopRuntime) {
      fileInputRef.current?.click();
      return;
    }
    try {
      const meeting = await invokeCommand("import_media");
      if (!meeting) return;
      setMeetings((current) => [
        normalizeMeeting(meeting),
        ...current.filter((candidate) => candidate.id !== meeting.id),
      ]);
      setView({ kind: "transcript", meetingId: meeting.id });
      notify("Media copied into your local library.");
    } catch (error) {
      notify(
        error instanceof Error ? error.message : "Could not import this media.",
        "warning",
      );
    }
  }

  function importBrowserFile(file?: File) {
    if (!file) return;
    const id = `import-${Date.now()}`;
    const meeting: Meeting = {
      id,
      title: file.name.replace(/\.[^.]+$/, ""),
      createdAt: new Date().toISOString(),
      durationMs: 0,
      status: "processing",
      sourceType: "import",
      speakerCount: 0,
      assetId: `browser-${id}`,
    };
    setMeetings((current) => [meeting, ...current]);
    setView({ kind: "transcript", meetingId: id });
    notify(`${file.name} added to the browser preview.`, "info");
  }

  async function startRecording(
    requestedMicrophoneId?: string,
    requestedOutputId?: string,
    microphoneIsPersonal = true,
    liveCaptions = true,
  ) {
    setNewDialogOpen(false);
    const microphone =
      audioDevices.find(
        (device) =>
          device.kind === "input" && device.id === requestedMicrophoneId,
      ) ??
      audioDevices.find(
        (device) => device.kind === "input" && device.isDefault,
      ) ??
      audioDevices.find((device) => device.kind === "input");
    const output =
      audioDevices.find(
        (device) =>
          device.kind === "output" && device.id === requestedOutputId,
      ) ??
      audioDevices.find(
        (device) => device.kind === "output" && device.isDefault,
      ) ??
      audioDevices.find((device) => device.kind === "output");
    if (desktopRuntime && (!microphone || !output)) {
      notify(
        "Choose an available microphone and output device before recording.",
        "warning",
      );
      return;
    }
    setRecordingDevices({
      microphoneDeviceId: microphone?.id ?? "",
      outputDeviceId: output?.id ?? "",
      microphoneIsPersonal,
      liveCaptions,
    });
    const id = `recording-${Date.now()}`;
    let nextMeeting: Meeting = {
      id,
      title: "New meeting",
      createdAt: new Date().toISOString(),
      durationMs: 0,
      status: "recording",
      sourceType: "recording",
      speakerCount: 3,
    };
    if (desktopRuntime) {
      try {
        const session = await invokeCommand("start_recording", {
          title: "New meeting",
          config: {
            microphoneDeviceId: microphone?.id ?? "",
            loopbackDeviceId: output?.id ?? "",
            captureMicrophone: true,
            captureSystemAudio: true,
            liveCaptions,
            microphoneIsPersonal,
          },
        });
        nextMeeting = {
          ...nextMeeting,
          id: session.meetingId,
          createdAt: session.startedAt,
        };
        recordingSessionRef.current = session;
        setRecordingSession(session);
        setRecordingStatus({
          state: session.state,
          sessionId: session.id,
          meetingId: session.meetingId,
          elapsedMs: session.elapsedMs,
          microphoneActive: true,
          systemAudioActive: true,
          microphoneLevel: 0,
          systemAudioLevel: 0,
          droppedCapturePackets: 0,
          droppedCaptionChunks: 0,
        });
      } catch (error) {
        notify(
          error instanceof Error ? error.message : "Could not start recording.",
          "warning",
        );
        return;
      }
    }
    setMeetings((current) => [nextMeeting, ...current]);
    setMarkers(desktopRuntime ? [] : seedMarkers);
    setLiveDraftTurns([]);
    setLiveDraftSpeakers([]);
    if (!desktopRuntime) {
      const previewSession: RecordingSession = {
        id: `preview-session-${Date.now()}`,
        meetingId: nextMeeting.id,
        state: "recording",
        elapsedMs: 1_122_000,
        startedAt: nextMeeting.createdAt,
      };
      recordingSessionRef.current = previewSession;
      setRecordingSession(previewSession);
      setRecordingStatus({
        state: "recording",
        sessionId: previewSession.id,
        meetingId: previewSession.meetingId,
        elapsedMs: previewSession.elapsedMs,
        microphoneActive: true,
        systemAudioActive: true,
        microphoneLevel: recordingLevels.microphone,
        systemAudioLevel: recordingLevels.system,
        droppedCapturePackets: 0,
        droppedCaptionChunks: 0,
      });
    }
    setView({ kind: "recording", meetingId: nextMeeting.id });
    notify("Recording started. Audio is being saved locally.");
  }

  async function toggleRecordingPause() {
    if (!desktopRuntime) {
      setRecordingSession((current) =>
        current
          ? {
              ...current,
              state: current.state === "paused" ? "recording" : "paused",
            }
          : current,
      );
      return;
    }
    if (!recordingSession?.id) {
      notify("The active recording session could not be resolved.", "warning");
      return;
    }
    try {
      const session =
        recordingSession?.state === "paused"
          ? await invokeCommand("resume_recording", {
              sessionId: recordingSession.id,
            })
          : await invokeCommand("pause_recording", {
              sessionId: recordingSession.id,
            });
      recordingSessionRef.current = session;
      setRecordingSession(session);
    } catch (error) {
      notify(
        error instanceof Error ? error.message : "Recording state could not change.",
        "warning",
      );
    }
  }

  async function stopRecording() {
    if (view.kind !== "recording") return;
    const meetingId = view.meetingId;
    if (desktopRuntime) {
      if (!recordingSession?.id) {
        notify("The active recording session could not be resolved.", "warning");
        return;
      }
      try {
        const finalized = await invokeCommand("stop_recording", {
          sessionId: recordingSession.id,
        });
        setMeetings((current) =>
          current.map((meeting) =>
            meeting.id === meetingId ? normalizeMeeting(finalized) : meeting,
          ),
        );
      } catch (error) {
        notify(
          error instanceof Error ? error.message : "Recording could not be finalized.",
          "warning",
        );
        return;
      }
    } else {
      setMeetings((current) =>
        current.map((meeting) =>
          meeting.id === meetingId
            ? { ...meeting, status: "processing", durationMs: 1_122_000 }
            : meeting,
        ),
      );
      setTurns(seedTurns);
      setSpeakers(seedSpeakers);
    }
    setRecordingSession(undefined);
    recordingSessionRef.current = undefined;
    setRecordingStatus((current) => ({
      ...current,
      state: "stopped",
      microphoneActive: false,
      systemAudioActive: false,
      microphoneLevel: 0,
      systemAudioLevel: 0,
    }));
    setRecordingDevices({
      microphoneDeviceId: "",
      outputDeviceId: "",
      microphoneIsPersonal: true,
      liveCaptions: true,
    });
    setLiveDraftTurns([]);
    setLiveDraftSpeakers([]);
    setView({ kind: "transcript", meetingId });
    notify("Recording saved. Final transcript processing has started.", "info");
  }

  function updateTurn(turnId: string, editedText: string) {
    const previousTurn = turns.find((turn) => turn.id === turnId);
    setTurns((current) =>
      current.map((turn) =>
        turn.id === turnId ? { ...turn, editedText } : turn,
      ),
    );
    if (desktopRuntime) {
      void invokeCommand("update_transcript_turn", {
        turnId,
        editedText,
        expectedRevision: previousTurn?.revision ?? 0,
      })
        .then((savedTurn) =>
          setTurns((current) =>
            current.map((turn) =>
              turn.id === turnId && turn.editedText === editedText
                ? savedTurn
                : turn,
            ),
          ),
        )
        .catch(() => {
          setTurns((current) =>
            current.map((turn) =>
              turn.id === turnId && turn.editedText === editedText && previousTurn
                ? previousTurn
                : turn,
            ),
          );
          notify(
            "The transcript changed before this edit could be saved. Reload and try again.",
            "warning",
          );
        });
    }
  }

  function toggleMarker(turnId: string) {
    const previousIsMarked =
      turns.find((turn) => turn.id === turnId)?.isMarked ?? false;
    const nextIsMarked = !previousIsMarked;
    setTurns((current) =>
      current.map((turn) =>
        turn.id === turnId ? { ...turn, isMarked: nextIsMarked } : turn,
      ),
    );
    if (desktopRuntime) {
      void invokeCommand("set_transcript_turn_bookmark", {
        turnId,
        isMarked: nextIsMarked,
      })
        .then((savedTurn) =>
          setTurns((current) =>
            current.map((turn) => (turn.id === turnId ? savedTurn : turn)),
          ),
        )
        .catch(() => {
          setTurns((current) =>
            current.map((turn) =>
              turn.id === turnId
                ? { ...turn, isMarked: previousIsMarked }
                : turn,
            ),
          );
          notify("The bookmark could not be saved.", "warning");
        });
    }
  }

  function toggleTurnReview(turnId: string) {
    const previousNeedsReview =
      turns.find((turn) => turn.id === turnId)?.needsReview ?? false;
    const nextNeedsReview = !previousNeedsReview;
    setTurns((current) =>
      current.map((turn) =>
        turn.id === turnId
          ? { ...turn, needsReview: nextNeedsReview }
          : turn,
      ),
    );
    if (desktopRuntime) {
      void invokeCommand("set_transcript_turn_review", {
        turnId,
        needsReview: nextNeedsReview,
      })
        .then((savedTurn) =>
          setTurns((current) =>
            current.map((turn) => (turn.id === turnId ? savedTurn : turn)),
          ),
        )
        .catch(() => {
          setTurns((current) =>
            current.map((turn) =>
              turn.id === turnId
                ? { ...turn, needsReview: previousNeedsReview }
                : turn,
            ),
          );
          notify("The review flag could not be saved.", "warning");
        });
    }
  }

  function renameMeeting(title: string) {
    if (!selectedMeeting) return;
    const meetingId = selectedMeeting.id;
    const previousTitle = selectedMeeting.title;
    setMeetings((current) =>
      current.map((meeting) =>
        meeting.id === meetingId ? { ...meeting, title } : meeting,
      ),
    );
    if (desktopRuntime) {
      void invokeCommand("rename_meeting", { meetingId, title })
        .then((savedMeeting) => {
          setMeetings((current) =>
            current.map((meeting) =>
              meeting.id === meetingId
                ? normalizeMeeting(savedMeeting)
                : meeting,
            ),
          );
          notify("Meeting renamed.");
        })
        .catch(() => {
          setMeetings((current) =>
            current.map((meeting) =>
              meeting.id === meetingId
                ? { ...meeting, title: previousTitle }
                : meeting,
            ),
          );
          notify("The meeting title could not be saved.", "warning");
        });
      return;
    }
    notify("Meeting renamed.");
  }

  function updateJob(job: ProcessingJob) {
    setJobs((current) => [
      job,
      ...current.filter((candidate) => candidate.id !== job.id),
    ]);
  }

  function cancelJob(jobId: string) {
    if (!desktopRuntime) return;
    void invokeCommand("cancel_processing_job", { jobId })
      .then((job) => {
        updateJob(job);
        notify("Processing cancellation requested.", "info");
      })
      .catch(() => notify("Processing could not be cancelled.", "warning"));
  }

  function retryJob(jobId: string) {
    if (!desktopRuntime) return;
    void invokeCommand("retry_processing_job", { jobId })
      .then((job) => {
        updateJob(job);
        notify("Processing was queued to retry.", "info");
      })
      .catch(() => notify("Processing could not be retried.", "warning"));
  }

  function renameSpeaker(speakerId: string, name: string) {
    const previousSpeaker = speakers.find((speaker) => speaker.id === speakerId);
    setSpeakers((current) =>
      current.map((speaker) =>
        speaker.id === speakerId
          ? {
              ...speaker,
              displayName: name,
              initials: name
                .split(/\s+/)
                .slice(0, 2)
                .map((part) => part[0]?.toUpperCase())
                .join(""),
            }
          : speaker,
      ),
    );
    if (desktopRuntime && selectedMeeting) {
      void invokeCommand("rename_speaker", {
        meetingId: selectedMeeting.id,
        speakerId,
        displayName: name,
      })
        .then(() => notify("Speaker renamed."))
        .catch(() => {
          if (previousSpeaker) {
            setSpeakers((current) =>
              current.map((speaker) =>
                speaker.id === speakerId && speaker.displayName === name
                  ? previousSpeaker
                  : speaker,
              ),
            );
          }
          notify("The speaker name could not be saved.", "warning");
        });
      return;
    }
    notify("Speaker renamed.");
  }

  function mergeSpeaker(sourceSpeakerId: string, targetSpeakerId: string) {
    const previousSpeakers = speakers;
    const sourceTurnIds = new Set(
      turns
        .filter((turn) => turn.speakerId === sourceSpeakerId)
        .map((turn) => turn.id),
    );
    setTurns((current) =>
      current.map((turn) =>
        turn.speakerId === sourceSpeakerId
          ? { ...turn, speakerId: targetSpeakerId }
          : turn,
      ),
    );
    setSpeakers((current) =>
      current.filter((speaker) => speaker.id !== sourceSpeakerId),
    );
    if (desktopRuntime && selectedMeeting) {
      void invokeCommand("merge_speakers", {
        meetingId: selectedMeeting.id,
        sourceSpeakerId,
        targetSpeakerId,
      })
        .then(() => notify("Speaker turns merged."))
        .catch(() => {
          setTurns((current) =>
            current.map((turn) =>
              sourceTurnIds.has(turn.id)
                ? { ...turn, speakerId: sourceSpeakerId }
                : turn,
            ),
          );
          setSpeakers(previousSpeakers);
          notify("The speaker merge could not be saved.", "warning");
        });
      return;
    }
    notify("Speaker turns merged.");
  }

  function reviewSpeaker(speakerId: string, accepted: boolean) {
    const previousSpeaker = speakers.find((speaker) => speaker.id === speakerId);
    if (!previousSpeaker) return;
    const matchedProfile = previousSpeaker.profileId
      ? profiles.find((profile) => profile.id === previousSpeaker.profileId)
      : undefined;
    if (accepted && !matchedProfile) {
      notify("This review item has no saved profile suggestion.", "warning");
      return;
    }
    setSpeakers((current) =>
      current.map((speaker) =>
        speaker.id === speakerId
          ? accepted
            ? {
                ...speaker,
                state: "Matched",
                displayName: matchedProfile?.name ?? speaker.displayName,
                initials: matchedProfile?.initials ?? speaker.initials,
              }
            : {
                ...speaker,
                state: "Unknown",
                displayName: "Unknown speaker",
                profileId: undefined,
                initials: "U",
              }
          : speaker,
      ),
    );
    if (desktopRuntime && selectedMeeting) {
      void invokeCommand("review_speaker", {
        meetingId: selectedMeeting.id,
        speakerId,
        accepted,
      }).catch(() => {
        setSpeakers((current) =>
          current.map((speaker) =>
            speaker.id === speakerId ? previousSpeaker : speaker,
          ),
        );
        notify("The speaker review could not be saved.", "warning");
      });
    }
    notify(
      accepted
        ? `Speaker matched to ${matchedProfile?.name}.`
        : "Match removed. This speaker will remain Unknown.",
      "info",
    );
  }

  async function addMarker(label: string, atMs: number) {
    if (desktopRuntime) {
      try {
        const saved = await invokeCommand("add_recording_marker", {
          request: { label },
        });
        setMarkers((current) => [...current, saved]);
        notify("Marker added.");
      } catch (error) {
        notify(
          error instanceof Error ? error.message : "Marker could not be saved.",
          "warning",
        );
      }
      return;
    }
    const marker: Marker = {
      id: `marker-${Date.now()}`,
      meetingId: selectedMeeting?.id ?? "new-meeting",
      atMs,
      label,
    };
    setMarkers((current) => [...current, marker]);
    notify("Marker added.");
  }

  function deleteProfile(profileId: string) {
    const previousProfile = profiles.find((profile) => profile.id === profileId);
    const previousSpeakers = speakers;
    setProfiles((current) =>
      current.filter((profile) => profile.id !== profileId),
    );
    setSpeakers((current) =>
      current.map((speaker) =>
        speaker.profileId === profileId
          ? {
              ...speaker,
              displayName: "Unknown speaker",
              initials: "U",
              profileId: undefined,
              state: "Unknown",
            }
          : speaker,
      ),
    );
    if (desktopRuntime) {
      void invokeCommand("delete_voice_profile", { profileId })
        .then(() => notify("Voice profile deleted.", "info"))
        .catch(() => {
          if (previousProfile) {
            setProfiles((current) => [
              previousProfile,
              ...current.filter((profile) => profile.id !== profileId),
            ]);
          }
          setSpeakers(previousSpeakers);
          notify("The voice profile could not be deleted.", "warning");
        });
      return;
    }
    notify("Voice profile deleted.", "info");
  }

  async function createProfile(name: string): Promise<VoiceProfile> {
    if (desktopRuntime) {
      try {
        const created = await invokeCommand("create_voice_profile", {
          name,
        });
        setProfiles((current) => [created, ...current]);
        notify("Voice profile created. Add three clean samples to enable matching.");
        return created;
      } catch (error) {
        notify(
          error instanceof Error ? error.message : "Voice profile could not be created.",
          "warning",
        );
        throw error;
      }
    }
    const words = name.trim().split(/\s+/);
    const created: VoiceProfile = {
      id: `profile-${Date.now()}`,
      name,
      initials: words
        .slice(0, 2)
        .map((word) => word[0]?.toUpperCase())
        .join(""),
      color: "#0868df",
      sampleDurationMs: 0,
      sampleCount: 0,
      lastUsedAt: new Date().toISOString(),
      status: "needs_samples",
    };
    setProfiles((current) => [created, ...current]);
    notify("Voice profile created. Add three clean samples to enable matching.");
    return created;
  }

  function addSampleToProfile(profileId: string) {
    const meeting =
      meetings.find(
        (candidate) =>
          candidate.status === "ready" && candidate.speakerCount > 0,
      ) ?? meetings.find((candidate) => candidate.status === "ready");
    if (!meeting) {
      notify(
        "Import or finalize a meeting before adding a voice sample.",
        "warning",
      );
      return;
    }
    setProfileSampleTargetId(profileId);
    setView({ kind: "transcript", meetingId: meeting.id });
    notify("Select a transcript turn from the speaker you want to confirm.", "info");
  }

  async function confirmVoiceSample(
    speakerId: string,
    existingProfileId?: string,
    newProfileName?: string,
  ) {
    if (!selectedMeeting) {
      notify("A finalized meeting is required for a voice sample.", "warning");
      throw new Error("No finalized meeting is selected.");
    }
    let profile = existingProfileId
      ? profiles.find((candidate) => candidate.id === existingProfileId)
      : undefined;
    if (!profile && newProfileName) {
      profile = await createProfile(newProfileName);
    }
    if (!profile) {
      notify("Choose or create a voice profile first.", "warning");
      throw new Error("Voice profile is missing.");
    }
    try {
      const confirmed = desktopRuntime
        ? await invokeCommand("confirm_voice_profile_sample", {
            request: {
              profileId: profile.id,
              meetingId: selectedMeeting.id,
              speakerId,
            },
          })
        : {
            ...profile,
            sampleDurationMs: profile.sampleDurationMs + 12_000,
            sampleCount: profile.sampleCount + 1,
            lastUsedAt: new Date().toISOString(),
            status:
              profile.sampleCount + 1 >= 3 &&
              profile.sampleDurationMs + 12_000 >= 30_000
                ? ("ready" as const)
                : ("needs_samples" as const),
          };
      setProfiles((current) => {
        const withoutConfirmed = current.filter(
          (candidate) => candidate.id !== confirmed.id,
        );
        return [confirmed, ...withoutConfirmed];
      });
      setSpeakers((current) =>
        current.map((speaker) =>
          speaker.id === speakerId
            ? {
                ...speaker,
                displayName: confirmed.name,
                initials: confirmed.initials,
                color: confirmed.color,
                state: "Matched",
                profileId: confirmed.id,
              }
            : speaker,
        ),
      );
      setProfileSampleTargetId(undefined);
      notify(`Voice sample confirmed for ${confirmed.name}.`);
    } catch (error) {
      notify(
        error instanceof Error
          ? error.message
          : "The clean voice sample could not be confirmed.",
        "warning",
      );
      throw error;
    }
  }

  function exportMeeting(format: ExportFormat) {
    if (!selectedMeeting) return;
    if (desktopRuntime) {
      void invokeCommand("export_transcript", {
        meetingId: selectedMeeting.id,
        format,
      })
        .then(() => notify(`Transcript exported as ${format.toUpperCase()}.`))
        .catch(() => notify("Export could not be completed.", "warning"));
      return;
    }
    const output = transcriptText(
      selectedMeeting.title,
      turns,
      speakers,
      format,
    );
    const blob = new Blob([output], {
      type: format === "json" ? "application/json" : "text/plain",
    });
    const url = URL.createObjectURL(blob);
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = `${selectedMeeting.title}.${format}`;
    anchor.click();
    URL.revokeObjectURL(url);
    notify(`Transcript exported as ${format.toUpperCase()}.`);
  }

  const content = (() => {
    if (view.kind === "library") {
      return (
        <LibraryView
          meetings={meetings}
          onOpen={(meetingId) =>
            setView({ kind: "transcript", meetingId })
          }
          onImport={() => void importMedia()}
          onRecord={() => setNewDialogOpen(true)}
        />
      );
    }
    if (view.kind === "profiles") {
      return (
        <VoiceProfilesView
          profiles={profiles}
          onCreate={createProfile}
          onDelete={deleteProfile}
          onAddSample={addSampleToProfile}
        />
      );
    }
    if (view.kind === "settings") {
      return (
        <SettingsView
          modelStatus={modelStatus}
          workerStatus={workerStatus}
          onOpenSetup={() => setView({ kind: "setup" })}
          onOpenLibrary={() => setView({ kind: "library" })}
          onRestartWorker={() => {
            if (!desktopRuntime) {
              notify("Local worker restarted.");
              return;
            }
            void invokeCommand("restart_worker")
              .then((status) => {
                setWorkerStatus(status);
                notify("Local worker restarted.");
              })
              .catch(() => notify("The local worker could not restart.", "warning"));
          }}
          onCheckModels={() => {
            if (!desktopRuntime) {
              notify("Model status refreshed from this device.");
              return;
            }
            void invokeCommand("get_model_status")
              .then((status) => {
                setModelStatus(status);
                notify(
                  status.runtime === "ready" &&
                    status.liveModel === "ready" &&
                    status.finalModel === "ready" &&
                    status.diarizationModel === "ready"
                    ? "All required model files are present on this device."
                    : "One or more model packs still need setup.",
                  "info",
                );
              })
              .catch(() =>
                notify("Model file status could not be refreshed.", "warning"),
              );
          }}
          onBackup={() => {
            if (!desktopRuntime) {
              notify("Library backup created.");
              return;
            }
            void invokeCommand("create_backup")
              .then(() => notify("Library backup created."))
              .catch((error) =>
                notify(
                  error instanceof Error
                    ? error.message
                    : "The library backup could not be created.",
                  "warning",
                ),
              );
          }}
        />
      );
    }
    if (view.kind === "setup") {
      return (
        <ModelSetupView
          status={modelStatus}
          progress={modelSetupProgress}
          onBack={() => setView({ kind: "settings" })}
          onInstall={async (token) => {
            setModelSetupProgress(undefined);
            if (desktopRuntime) {
              const installedStatus = await invokeCommand("install_model_pack", {
                huggingFaceToken: token,
              });
              setModelStatus(installedStatus);
              const installedWorker = await invokeCommand("get_worker_status");
              setWorkerStatus(installedWorker);
            } else {
              await new Promise((resolve) => window.setTimeout(resolve, 700));
            }
            notify("Model packs are verified and ready offline.");
          }}
        />
      );
    }
    if (view.kind === "recording") {
      return (
        <RecordingView
          meeting={selectedMeeting}
          devices={audioDevices}
          turns={
            recordingDevices.liveCaptions
              ? desktopRuntime
                ? liveDraftTurns
                : draftTurns
              : []
          }
          speakers={
            recordingDevices.liveCaptions
              ? desktopRuntime
                ? liveDraftSpeakers
                : draftSpeakers
              : []
          }
          markers={markers}
          session={recordingSession}
          status={recordingStatus}
          levels={recordingLevels}
          microphoneDeviceId={recordingDevices.microphoneDeviceId}
          outputDeviceId={recordingDevices.outputDeviceId}
          microphoneIsPersonal={recordingDevices.microphoneIsPersonal}
          liveCaptionsEnabled={recordingDevices.liveCaptions}
          availableStorageGb={modelStatus.diskAvailableGb}
          onRenameMeeting={renameMeeting}
          onTogglePause={() => void toggleRecordingPause()}
          onAddMarker={(label, atMs) => void addMarker(label, atMs)}
          onStop={() => void stopRecording()}
        />
      );
    }
    return (
      <TranscriptView
        meeting={selectedMeeting}
        mediaSourceUrl={mediaUrl}
        allowSimulatedPlayback={!desktopRuntime}
        turns={turns}
        speakers={speakers}
        markers={markers}
        processingJob={jobs.find(
          (job) => job.meetingId === selectedMeeting.id,
        )}
        profiles={profiles}
        profileSampleTargetId={profileSampleTargetId}
        onUpdateTurn={updateTurn}
        onToggleMarker={toggleMarker}
        onToggleTurnReview={toggleTurnReview}
        onRenameMeeting={renameMeeting}
        onRenameSpeaker={renameSpeaker}
        onMergeSpeaker={mergeSpeaker}
        onReviewSpeaker={reviewSpeaker}
        onCancelJob={cancelJob}
        onRetryJob={retryJob}
        onConfirmVoiceSample={confirmVoiceSample}
        onExport={exportMeeting}
      />
    );
  })();

  return (
    <div className="app">
      <Titlebar />
      <div className="app-shell">
        <Sidebar
          view={view}
          meetings={meetings}
          onNavigate={setView}
          onNew={() => setNewDialogOpen(true)}
        />
        {content}
      </div>
      <input
        ref={fileInputRef}
        className="sr-only"
        type="file"
        accept="audio/*,video/*,.mkv,.m4a,.flac,.ogg,.wav,.mp3,.mp4,.mov,.webm"
        onChange={(event) => {
          importBrowserFile(event.target.files?.[0]);
          event.currentTarget.value = "";
        }}
      />
      {newDialogOpen ? (
        <NewTranscriptionDialog
          devices={audioDevices}
          onClose={() => setNewDialogOpen(false)}
          onImport={() => void importMedia()}
          onRecord={(
            microphoneDeviceId,
            outputDeviceId,
            microphoneIsPersonal,
            liveCaptions,
          ) =>
            void startRecording(
              microphoneDeviceId,
              outputDeviceId,
              microphoneIsPersonal,
              liveCaptions,
            )
          }
        />
      ) : null}
      {toast ? <Toast toast={toast} onDismiss={() => setToast(undefined)} /> : null}
    </div>
  );
}
