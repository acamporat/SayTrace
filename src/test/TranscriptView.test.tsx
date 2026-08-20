import { render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { TranscriptView } from "../features/transcript/TranscriptView";
import type { LocalAgentStatus, Meeting } from "../types";

const meeting: Meeting = {
  id: "meeting-screen",
  title: "Screen capture review",
  createdAt: "2026-08-19T17:27:06.000Z",
  durationMs: 27_800,
  status: "ready",
  sourceType: "recording",
  speakerCount: 0,
};

const agentStatus: LocalAgentStatus = {
  state: "ready",
  backend: "Ollama",
  endpoint: "127.0.0.1:11434",
  models: [],
};

describe("TranscriptView saved media", () => {
  it("keeps transcript audio separate from the saved screen recording player", () => {
    const { container } = render(
      <TranscriptView
        meeting={meeting}
        mediaSourceUrl="blob:transcript-audio"
        screenSourceUrl="blob:saved-screen"
        allowSimulatedPlayback={false}
        turns={[]}
        speakers={[]}
        markers={[]}
        visualContext={[]}
        visualContextUrls={{}}
        profiles={[]}
        chatMessages={[]}
        agentStatus={agentStatus}
        agentLoading={false}
        onUpdateTurn={vi.fn()}
        onToggleMarker={vi.fn()}
        onToggleTurnReview={vi.fn()}
        onRenameMeeting={vi.fn()}
        onRenameSpeaker={vi.fn()}
        onMergeSpeaker={vi.fn()}
        onReviewSpeaker={vi.fn()}
        onCancelJob={vi.fn()}
        onRetryJob={vi.fn()}
        onConfirmVoiceSample={vi.fn(async () => undefined)}
        onExport={vi.fn()}
        onAskTranscript={vi.fn(async () => undefined)}
        onClearTranscriptChat={vi.fn(async () => undefined)}
        onRefreshAgentStatus={vi.fn(async () => undefined)}
      />,
    );

    expect(container.querySelector("audio")).toHaveAttribute(
      "src",
      "blob:transcript-audio",
    );
    const screenRegion = screen.getByRole("region", {
      name: "Screen recording",
    });
    const screenVideo = within(screenRegion).getByLabelText(
      "Screen recording for Screen capture review",
    );
    expect(screenVideo).toHaveAttribute("src", "blob:saved-screen");
    expect(screenVideo).toHaveAttribute("controls");
    expect(screenVideo).toHaveAttribute("preload", "metadata");
    expect(screenVideo).toHaveAttribute("playsinline");
  });
});
