import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
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
});
