import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { TranscriptRows } from "../components/TranscriptRows";

describe("TranscriptRows playback alignment", () => {
  it("colors completed words and singles out the word currently being spoken", () => {
    const { container } = render(
      <TranscriptRows
        turns={[
          {
            id: "turn-1",
            speakerId: "speaker-1",
            startMs: 0,
            endMs: 1_000,
            modelText: "Hello world",
            words: [
              { id: "word-1", text: "Hello", startMs: 0, endMs: 500 },
              { id: "word-2", text: "world", startMs: 500, endMs: 1_000 },
            ],
          },
        ]}
        speakers={[
          {
            id: "speaker-1",
            displayName: "Speaker 1",
            initials: "S1",
            color: "#0868df",
            state: "Unknown",
          },
        ]}
        activeTurnId="turn-1"
        activeWordId="word-2"
        playbackPositionMs={750}
        search="Hello world"
      />,
    );

    expect(screen.getByText("Hello").closest(".transcript-word")).toHaveClass(
      "is-played",
    );
    expect(screen.getByText("world").closest(".transcript-word")).toHaveClass(
      "is-current",
    );
    expect(
      Array.from(container.querySelectorAll("mark"))
        .map((mark) => mark.textContent)
        .join(""),
    ).toBe("Hello world");
  });

  it("keeps word following when transcript punctuation differs from timed tokens", () => {
    render(
      <TranscriptRows
        turns={[
          {
            id: "turn-1",
            speakerId: "speaker-1",
            startMs: 0,
            endMs: 1_000,
            modelText: "We’re ready, now.",
            words: [
              { id: "word-1", text: "We're", startMs: 0, endMs: 400 },
              { id: "word-2", text: "ready", startMs: 400, endMs: 700 },
              { id: "word-3", text: "missing-token", startMs: 700, endMs: 800 },
              { id: "word-4", text: "now", startMs: 800, endMs: 1_000 },
            ],
          },
        ]}
        speakers={[
          {
            id: "speaker-1",
            displayName: "Speaker 1",
            initials: "S1",
            color: "#0868df",
            state: "Unknown",
          },
        ]}
        activeTurnId="turn-1"
        activeWordId="word-4"
        playbackPositionMs={900}
      />,
    );

    expect(screen.getByText("We’re").closest(".transcript-word")).toHaveClass(
      "is-played",
    );
    expect(screen.getByText("now").closest(".transcript-word")).toHaveClass(
      "is-current",
    );
  });

  it("keeps timings enabled for null editedText and follows into the next untouched turn", () => {
    render(
      <TranscriptRows
        turns={[
          {
            id: "turn-1",
            speakerId: "speaker-1",
            startMs: 0,
            endMs: 500,
            modelText: "First turn",
            editedText: null,
            words: [
              { id: "word-1", text: "First", startMs: 0, endMs: 250 },
              { id: "word-2", text: "turn", startMs: 250, endMs: 500 },
            ],
          },
          {
            id: "turn-2",
            speakerId: "speaker-1",
            startMs: 500,
            endMs: 1_000,
            modelText: "Second turn",
            editedText: null,
            words: [
              { id: "word-3", text: "Second", startMs: 500, endMs: 750 },
              { id: "word-4", text: "turn", startMs: 750, endMs: 1_000 },
            ],
          },
        ]}
        speakers={[
          {
            id: "speaker-1",
            displayName: "Speaker 1",
            initials: "S1",
            color: "#0868df",
            state: "Unknown",
          },
        ]}
        activeTurnId="turn-2"
        activeWordId="word-4"
        playbackPositionMs={900}
      />,
    );

    expect(screen.getByText("First").closest(".transcript-word")).toHaveClass(
      "is-played",
    );
    expect(screen.getByText("Second").closest(".transcript-word")).toHaveClass(
      "is-played",
    );
    expect(screen.getAllByText("turn")[1].closest(".transcript-word")).toHaveClass(
      "is-current",
    );
  });

  it("does not create a transcript edit just from focusing and leaving a row", () => {
    const onEdit = vi.fn();
    render(
      <TranscriptRows
        editable
        turns={[
          {
            id: "turn-1",
            speakerId: "speaker-1",
            startMs: 0,
            endMs: 500,
            modelText: "Untouched transcript",
            editedText: null,
            words: [
              { id: "word-1", text: "Untouched", startMs: 0, endMs: 250 },
              { id: "word-2", text: "transcript", startMs: 250, endMs: 500 },
            ],
          },
        ]}
        speakers={[
          {
            id: "speaker-1",
            displayName: "Speaker 1",
            initials: "S1",
            color: "#0868df",
            state: "Unknown",
          },
        ]}
        onEdit={onEdit}
      />,
    );

    fireEvent.blur(
      screen.getByRole("textbox", {
        name: "Speaker 1 transcript at 00:00:00",
      }),
    );

    expect(onEdit).not.toHaveBeenCalled();
  });

  it("marks completed rows and words from the discrete playback cursor", () => {
    render(
      <TranscriptRows
        turns={[
          {
            id: "turn-1",
            speakerId: "speaker-1",
            startMs: 0,
            endMs: 500,
            modelText: "First turn",
            words: [
              { id: "word-1", text: "First", startMs: 0, endMs: 250 },
              { id: "word-2", text: "turn", startMs: 250, endMs: 500 },
            ],
          },
          {
            id: "turn-2",
            speakerId: "speaker-1",
            startMs: 500,
            endMs: 1_000,
            modelText: "Second turn",
            words: [
              { id: "word-3", text: "Second", startMs: 500, endMs: 750 },
              { id: "word-4", text: "turn", startMs: 750, endMs: 1_000 },
            ],
          },
        ]}
        speakers={[
          {
            id: "speaker-1",
            displayName: "Speaker 1",
            initials: "S1",
            color: "#0868df",
            state: "Unknown",
          },
        ]}
        activeTurnId="turn-2"
        activeWordId="word-4"
        playedWordId="word-3"
      />,
    );

    expect(screen.getByText("First").closest(".transcript-word")).toHaveClass(
      "is-played",
    );
    expect(screen.getByText("Second").closest(".transcript-word")).toHaveClass(
      "is-played",
    );
    expect(
      screen.getAllByText("turn")[1].closest(".transcript-word"),
    ).toHaveClass("is-current");
    expect(
      screen.getAllByText("turn")[1].closest(".transcript-word"),
    ).not.toHaveClass("is-played");
  });

  it("renders lazy screen context outside the transcript editor with review provenance", () => {
    const { container } = render(
      <TranscriptRows
        editable
        turns={[
          {
            id: "turn-visual",
            speakerId: "speaker-visual",
            startMs: 10_000,
            endMs: 20_000,
            modelText: "As you can see on this screen, activation improved.",
          },
        ]}
        speakers={[
          {
            id: "speaker-visual",
            displayName: "Speaker 1",
            initials: "S1",
            color: "#676c72",
            state: "Review",
            attributionSource: "visual",
            attributionConfidence: "review",
          },
        ]}
        visualContext={[
          {
            id: "context-1",
            meetingId: "meeting-1",
            turnId: "turn-visual",
            kind: "shared_content",
            atMs: 12_000,
            screenshotAssetId: "screen-1",
            reason: "Captured when shared results were discussed.",
            triggerText: "this screen",
            confidence: "high",
            source: "transcript_heuristic",
            meetingSystem: "Microsoft Teams",
            createdAtMs: 12_000,
          },
        ]}
        visualContextUrls={{ "screen-1": "blob:screen-context" }}
      />,
    );

    const image = screen.getByRole("img", {
      name: "Screen context captured from Microsoft Teams at 00:00:12",
    });
    expect(image).toHaveAttribute("loading", "lazy");
    expect(image).toHaveAttribute("decoding", "async");
    expect(screen.getByText("Visual suggestion")).toBeInTheDocument();
    expect(
      screen.getByText("Captured when shared results were discussed."),
    ).toBeInTheDocument();

    const editor = screen.getByRole("textbox", {
      name: "Speaker 1 transcript at 00:00:10",
    });
    const figure = container.querySelector("figure.visual-context-card");
    expect(figure).not.toBeNull();
    expect(editor.contains(figure)).toBe(false);
  });
});
