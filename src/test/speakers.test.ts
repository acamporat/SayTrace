import { describe, expect, it } from "vitest";
import { normalizeUnknownSpeakerNames } from "../lib/speakers";
import type { MeetingSpeaker } from "../types";

function anonymousSpeaker(
  id: string,
  label: string,
  displayName: string,
): MeetingSpeaker {
  return {
    id,
    label,
    displayName,
    color: "#6E8BFF",
    initials: "?",
    state: "Unknown",
  };
}

describe("anonymous speaker names", () => {
  it("keeps every numbered name unique when a legacy unknown label is mixed in", () => {
    const speakers = normalizeUnknownSpeakerNames([
      anonymousSpeaker("a", "SPEAKER_00", "Speaker 1"),
      anonymousSpeaker("b", "SPEAKER_01", "Speaker 2"),
      anonymousSpeaker("c", "unknown", "Speaker 1"),
    ]);

    expect(speakers.map((speaker) => speaker.displayName)).toEqual([
      "Speaker 1",
      "Speaker 2",
      "Speaker 3",
    ]);
    expect(speakers.map((speaker) => speaker.initials)).toEqual([
      "S1",
      "S2",
      "S3",
    ]);
  });
});
