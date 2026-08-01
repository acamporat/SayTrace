import { describe, expect, it } from "vitest";
import {
  applyDraftRevision,
  isDraftRevisionEvent,
  upsertDraftSpeaker,
} from "../lib/liveDraft";
import type { DraftRevisionEvent } from "../types";

function revision(
  overrides: Partial<DraftRevisionEvent> = {},
): DraftRevisionEvent {
  return {
    session_id: "session-1",
    stream_id: "microphone",
    speaker_hint: "You",
    coalesced_audio_ms: 0,
    revision: 1,
    replace_from_token: 2,
    committed_text: "hello there",
    unstable_text: "friend",
    is_final: false,
    ...overrides,
  };
}

describe("live draft revisions", () => {
  it("accepts the worker payload shape and rejects malformed events", () => {
    expect(isDraftRevisionEvent(revision())).toBe(true);
    expect(isDraftRevisionEvent({ turns: [] })).toBe(false);
  });

  it("replaces a stream turn and ignores stale revisions", () => {
    const first = applyDraftRevision([], revision(), 30_000);
    const second = applyDraftRevision(
      first,
      revision({
        revision: 2,
        replace_from_token: 3,
        committed_text: "hello there friend",
        unstable_text: "again",
      }),
      31_000,
    );
    const stale = applyDraftRevision(second, revision(), 32_000);

    expect(second).toHaveLength(1);
    expect(second[0]).toMatchObject({
      speakerId: "you",
      modelText: "hello there friend again",
      revision: 2,
      isDraft: true,
    });
    expect(stale).toBe(second);
  });

  it("keeps independent provisional turns and anonymous system speakers", () => {
    const microphone = revision();
    const system = revision({
      stream_id: "system",
      speaker_hint: "Speaker",
      committed_text: "remote participant",
    });
    const turns = applyDraftRevision(
      applyDraftRevision([], microphone, 30_000),
      system,
      30_500,
    );
    const speakers = upsertDraftSpeaker(
      upsertDraftSpeaker([], microphone),
      system,
    );

    expect(turns).toHaveLength(2);
    expect(new Set(turns.map((turn) => turn.id)).size).toBe(2);
    expect(speakers.map((speaker) => speaker.displayName)).toEqual([
      "You",
      "Speaker",
    ]);
    expect(speakers[1].state).toBe("Unknown");
  });
});
