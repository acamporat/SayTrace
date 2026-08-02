import type {
  AudioDevice,
  Marker,
  Meeting,
  MeetingSpeaker,
  ModelPackStatus,
  TranscriptTurn,
  VoiceProfile,
  WordTiming,
} from "../types";

function evenlyTimedWords(
  turnId: string,
  text: string,
  startMs: number,
  endMs: number,
): WordTiming[] {
  const tokens = text.match(/\S+/g) ?? [];
  const wordDuration = (endMs - startMs) / Math.max(1, tokens.length);
  return tokens.map((token, index) => ({
    id: `${turnId}-word-${index + 1}`,
    text: token,
    startMs: Math.round(startMs + wordDuration * index),
    endMs: Math.round(startMs + wordDuration * (index + 1)),
  }));
}

export const meetings: Meeting[] = [
  {
    id: "weekly-production",
    title: "Weekly production meeting",
    createdAt: "2025-05-20T09:41:00",
    durationMs: 2_718_000,
    status: "ready",
    sourceType: "recording",
    speakerCount: 3,
    assetId: "asset-weekly-production",
  },
  {
    id: "roadmap-planning",
    title: "Roadmap planning",
    createdAt: "2025-05-19T14:15:00",
    durationMs: 3_128_000,
    status: "ready",
    sourceType: "import",
    speakerCount: 4,
    assetId: "asset-roadmap",
  },
  {
    id: "interview-jason",
    title: "Interview with Jason",
    createdAt: "2025-05-16T11:08:00",
    durationMs: 2_094_000,
    status: "ready",
    sourceType: "recording",
    speakerCount: 2,
    assetId: "asset-jason",
  },
  {
    id: "design-sync",
    title: "Design sync",
    createdAt: "2025-05-15T16:32:00",
    durationMs: 1_846_000,
    status: "ready",
    sourceType: "recording",
    speakerCount: 5,
    assetId: "asset-design",
  },
  {
    id: "client-feedback",
    title: "Client feedback call",
    createdAt: "2025-05-14T10:20:00",
    durationMs: 2_360_000,
    status: "ready",
    sourceType: "import",
    speakerCount: 3,
    assetId: "asset-feedback",
  },
];

export const speakers: MeetingSpeaker[] = [
  {
    id: "alex",
    displayName: "Alex",
    color: "#3aa66f",
    initials: "A",
    state: "Matched",
    profileId: "profile-alex",
  },
  {
    id: "maya",
    displayName: "Maya",
    color: "#8052ca",
    initials: "M",
    state: "Matched",
    profileId: "profile-maya",
  },
  {
    id: "unknown",
    label: "SPEAKER_02",
    displayName: "Speaker 3",
    color: "#676c72",
    initials: "S3",
    state: "Review",
    profileId: "profile-sam",
  },
];

export const transcriptTurns: TranscriptTurn[] = [
  {
    id: "turn-1",
    speakerId: "alex",
    startMs: 23_000,
    endMs: 58_000,
    modelText:
      "Thanks everyone for joining. Let’s start with a quick update on last week’s progress.",
  },
  {
    id: "turn-2",
    speakerId: "maya",
    startMs: 62_000,
    endMs: 132_000,
    modelText:
      "Sure. We completed the first pass of the new onboarding flow. The copy is in good shape, and engineering has the feature flag implemented.",
  },
  {
    id: "turn-3",
    speakerId: "alex",
    startMs: 135_000,
    endMs: 173_000,
    modelText: "Great. Any metrics from the beta group?",
  },
  {
    id: "turn-4",
    speakerId: "maya",
    startMs: 180_000,
    endMs: 231_000,
    modelText:
      "Early results look promising. Activation is up 12% and drop-off decreased by 8%. We’ll share the full report later this week.",
    words: evenlyTimedWords(
      "turn-4",
      "Early results look promising. Activation is up 12% and drop-off decreased by 8%. We’ll share the full report later this week.",
      180_000,
      231_000,
    ),
    isMarked: true,
  },
  {
    id: "turn-5",
    speakerId: "unknown",
    startMs: 238_000,
    endMs: 271_000,
    modelText:
      "One thing to note, a few users in the beta reported confusion on the permissions step.",
  },
  {
    id: "turn-6",
    speakerId: "alex",
    startMs: 275_000,
    endMs: 306_000,
    modelText:
      "Thanks for flagging that. Maybe we can add a short explanation tooltip there.",
  },
  {
    id: "turn-7",
    speakerId: "unknown",
    startMs: 310_000,
    endMs: 330_000,
    modelText: "Sounds good. I can draft some options for that.",
  },
  {
    id: "turn-8",
    speakerId: "maya",
    startMs: 332_000,
    endMs: 365_000,
    modelText: "Also, when are we thinking of rolling this out to everyone?",
  },
];

export const draftSpeakers: MeetingSpeaker[] = [
  {
    id: "you",
    displayName: "You",
    color: "#0864d9",
    initials: "Y",
    state: "Matched",
  },
  {
    id: "speaker-2",
    displayName: "Speaker 2",
    color: "#8052ca",
    initials: "S2",
    state: "Unknown",
  },
  {
    id: "speaker-3",
    displayName: "Speaker 3",
    color: "#169c9d",
    initials: "S3",
    state: "Unknown",
  },
];

export const draftTurns: TranscriptTurn[] = [
  {
    id: "draft-1",
    speakerId: "you",
    startMs: 5_000,
    endMs: 26_000,
    modelText:
      "Thanks everyone for joining. Let’s start with a quick update on last week’s progress.",
    isDraft: true,
  },
  {
    id: "draft-2",
    speakerId: "speaker-2",
    startMs: 28_000,
    endMs: 58_000,
    modelText:
      "Sure. We completed the first pass of the new onboarding flow. The copy is in good shape, and engineering has the feature flag implemented.",
    isDraft: true,
  },
  {
    id: "draft-3",
    speakerId: "speaker-3",
    startMs: 62_000,
    endMs: 75_000,
    modelText: "Great. Any metrics from the beta group?",
    isDraft: true,
  },
  {
    id: "draft-4",
    speakerId: "you",
    startMs: 78_000,
    endMs: 103_000,
    modelText:
      "Early results look promising. Activation is up 12% and drop-off decreased by 8%. We’ll share the full report later this week.",
    isDraft: true,
  },
  {
    id: "draft-5",
    speakerId: "speaker-2",
    startMs: 105_000,
    endMs: 124_000,
    modelText:
      "One thing to note, a few users in the beta reported confusion on the permissions step.",
    isDraft: true,
  },
  {
    id: "draft-6",
    speakerId: "speaker-3",
    startMs: 127_000,
    endMs: 141_000,
    modelText:
      "Thanks for flagging that. Maybe we can add a short explanation tooltip there.",
    isDraft: true,
  },
  {
    id: "draft-7",
    speakerId: "you",
    startMs: 146_000,
    endMs: 164_000,
    modelText:
      "Sounds good. I can draft some options for that. Also, when are we thinking of rolling this out",
    isDraft: true,
  },
];

export const markers: Marker[] = [
  {
    id: "marker-1",
    meetingId: "new-meeting",
    atMs: 192_000,
    label: "Feature flag rollout",
  },
  {
    id: "marker-2",
    meetingId: "new-meeting",
    atMs: 405_000,
    label: "Beta feedback themes",
  },
  {
    id: "marker-3",
    meetingId: "new-meeting",
    atMs: 721_000,
    label: "Next steps",
  },
];

export const voiceProfiles: VoiceProfile[] = [
  {
    id: "profile-alex",
    name: "Alex Morgan",
    initials: "AM",
    color: "#3aa66f",
    sampleDurationMs: 194_000,
    sampleCount: 5,
    lastUsedAt: "2025-05-20T09:41:00",
    status: "ready",
  },
  {
    id: "profile-maya",
    name: "Maya Chen",
    initials: "MC",
    color: "#8052ca",
    sampleDurationMs: 151_000,
    sampleCount: 4,
    lastUsedAt: "2025-05-20T09:41:00",
    status: "ready",
  },
  {
    id: "profile-jason",
    name: "Jason Lee",
    initials: "JL",
    color: "#159c9e",
    sampleDurationMs: 72_000,
    sampleCount: 3,
    lastUsedAt: "2025-05-16T11:08:00",
    status: "ready",
  },
  {
    id: "profile-sam",
    name: "Sam Rivera",
    initials: "SR",
    color: "#d57b26",
    sampleDurationMs: 19_000,
    sampleCount: 2,
    lastUsedAt: "2025-05-14T10:20:00",
    status: "needs_samples",
  },
];

export const devices: AudioDevice[] = [
  {
    id: "mic-shure-mv7",
    name: "Microphone — Shure MV7",
    kind: "input",
    isDefault: true,
  },
  {
    id: "mic-array",
    name: "Microphone — Realtek Array",
    kind: "input",
    isDefault: false,
  },
  {
    id: "output-speakers",
    name: "System audio — Speakers",
    kind: "output",
    isDefault: true,
  },
  {
    id: "output-headset",
    name: "System audio — USB Headset",
    kind: "output",
    isDefault: false,
  },
];

export const modelStatus: ModelPackStatus = {
  runtime: "ready",
  liveModel: "ready",
  finalModel: "ready",
  diarizationModel: "ready",
  device: "NVIDIA GeForce RTX 2080 Ti",
  diskRequiredGb: 12.4,
  diskAvailableGb: 128.4,
};
