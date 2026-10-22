import { fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ComponentProps } from "react";
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

function speakerProps(): ComponentProps<typeof TranscriptView> {
  return {
    meeting, allowSimulatedPlayback: false,
    speakers: [
      { id: "alice", displayName: "Alice", initials: "A", color: "#123456", state: "Matched" },
      { id: "unknown", displayName: "Speaker 2", initials: "S2", color: "#234567", state: "Unknown" },
    ],
    turns: [
      { id: "one", speakerId: "alice", startMs: 0, endMs: 1000, modelText: "Alice first comment" },
      { id: "two", speakerId: "unknown", startMs: 1000, endMs: 2000, modelText: "Unknown comment" },
      { id: "three", speakerId: "alice", startMs: 2000, endMs: 3000, modelText: "Alice final comment" },
    ],
    markers: [], visualContext: [], visualContextUrls: {}, profiles: [], chatMessages: [],
    agentStatus, agentLoading: false,
    onUpdateTurn: vi.fn(), onToggleMarker: vi.fn(), onToggleTurnReview: vi.fn(),
    onRenameMeeting: vi.fn(), onRenameSpeaker: vi.fn(), onMergeSpeaker: vi.fn(),
    onReviewSpeaker: vi.fn(), onCancelJob: vi.fn(), onRetryJob: vi.fn(),
    onConfirmVoiceSample: vi.fn(async () => undefined), onExport: vi.fn(),
    onAskTranscript: vi.fn(async () => undefined), onClearTranscriptChat: vi.fn(async () => undefined),
    onRefreshAgentStatus: vi.fn(async () => undefined),
  };
}

describe("TranscriptView speaker filtering", () => {
  it("filters identified and unknown speakers and restores all turns", async () => {
    const user = userEvent.setup();
    render(<TranscriptView {...speakerProps()} />);
    await user.click(screen.getByRole("tab", { name: "Speakers" }));
    await user.click(screen.getByRole("button", { name: "Show only Alice" }));
    expect(screen.getByText("Alice first comment")).toBeInTheDocument();
    expect(screen.getByText("Alice final comment")).toBeInTheDocument();
    expect(screen.queryByText("Unknown comment")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Show only Alice" })).toHaveAttribute("aria-pressed", "true");
    screen.getByRole("button", { name: "Show only Speaker 2" }).focus();
    await user.keyboard(" ");
    expect(screen.queryByText("Alice first comment")).not.toBeInTheDocument();
    expect(screen.getByText("Unknown comment")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Show all speakers" }));
    expect(screen.getByText("Alice first comment")).toBeInTheDocument();
    expect(screen.getByText("Unknown comment")).toBeInTheDocument();
  });

  it("keeps speaker actions separate and scopes search and replacement to visible turns", async () => {
    const user = userEvent.setup();
    const props = speakerProps();
    render(<TranscriptView {...props} />);
    await user.click(screen.getByRole("tab", { name: "Speakers" }));
    await user.click(screen.getByRole("button", { name: "More actions for Alice" }));
    expect(screen.queryByRole("button", { name: "Show all speakers" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Show only Speaker 2" }));
    fireEvent.change(screen.getByRole("textbox", { name: "Search transcript" }), { target: { value: "comment" } });
    expect(screen.getByText("1 found")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Find and replace" }));
    fireEvent.change(screen.getByRole("textbox", { name: "Find text" }), { target: { value: "comment" } });
    fireEvent.change(screen.getByRole("textbox", { name: "Replacement text" }), { target: { value: "remark" } });
    await user.click(screen.getByRole("button", { name: "Replace for this speaker" }));
    expect(props.onUpdateTurn).toHaveBeenCalledExactlyOnceWith("two", "Unknown remark");
  });

  it("clears stale filters after a merge or meeting change", async () => {
    const user = userEvent.setup();
    const props = speakerProps();
    const { rerender } = render(<TranscriptView {...props} />);
    await user.click(screen.getByRole("tab", { name: "Speakers" }));
    await user.click(screen.getByRole("button", { name: "Show only Speaker 2" }));
    rerender(<TranscriptView {...props} speakers={props.speakers.slice(0, 1)} />);
    expect(screen.getByText("Alice first comment")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Show all speakers" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Show only Alice" }));
    rerender(<TranscriptView {...props} meeting={{ ...meeting, id: "different" }} />);
    expect(screen.getByText("Unknown comment")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Show all speakers" })).not.toBeInTheDocument();
  });
});
