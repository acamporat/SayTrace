import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { NewTranscriptionDialog } from "../components/NewTranscriptionDialog";
import type { AudioDevice } from "../types";

const devices: AudioDevice[] = [
  {
    id: "default-microphone",
    name: "Default microphone",
    kind: "input",
    isDefault: true,
  },
  {
    id: "default-output",
    name: "Default speakers",
    kind: "output",
    isDefault: true,
  },
];

function renderDialog(onRecord = vi.fn()) {
  render(
    <NewTranscriptionDialog
      devices={devices}
      onClose={vi.fn()}
      onImport={vi.fn()}
      onRecord={onRecord}
    />,
  );
  return { onRecord };
}

describe("NewTranscriptionDialog recording confirmation", () => {
  it("defaults all screen context features on and requires final main-display confirmation", async () => {
    const user = userEvent.setup();
    const { onRecord } = renderDialog();
    const settingsDialog = screen.getByRole("dialog", {
      name: "New transcription",
    });

    expect(
      within(settingsDialog).getByRole("checkbox", {
        name: /Record the main display/i,
      }),
    ).toBeChecked();
    expect(
      within(settingsDialog).getByRole("checkbox", {
        name: /Add relevant screenshots to the transcript/i,
      }),
    ).toBeChecked();
    expect(
      within(settingsDialog).getByRole("checkbox", {
        name: /Use meeting-app visual cues to suggest speakers/i,
      }),
    ).toBeChecked();
    expect(
      within(settingsDialog).getByRole("button", {
        name: /Upload a recording/i,
      }),
    ).toHaveTextContent(/Video: transcript, screenshots, and visual cues/i);

    await user.click(
      within(settingsDialog).getByRole("button", {
        name: /Record a meeting/i,
      }),
    );

    expect(onRecord).not.toHaveBeenCalled();
    const confirmationDialog = screen.getByRole("dialog", {
      name: "Review before recording",
    });
    expect(
      within(confirmationDialog).getByRole("heading", {
        name: "Review before recording",
      }),
    ).toHaveFocus();
    expect(
      within(confirmationDialog).getByText("Screen recording is ON"),
    ).toBeInTheDocument();
    expect(
      within(confirmationDialog).getByText(
        "Inline relevant screenshots are ON",
      ),
    ).toBeInTheDocument();
    expect(
      within(confirmationDialog).getByText(
        "Visual speaker suggestions are ON",
      ),
    ).toBeInTheDocument();

    const startButton = within(confirmationDialog).getByRole("button", {
      name: "Start recording with screen",
    });
    expect(startButton).toBeDisabled();
    await user.click(
      within(confirmationDialog).getByRole("checkbox", {
        name: /I understand that the entire main display/i,
      }),
    );
    expect(startButton).toBeEnabled();
    await user.click(startButton);

    expect(onRecord).toHaveBeenCalledTimes(1);
    expect(onRecord).toHaveBeenCalledWith(
      "default-microphone",
      "default-output",
      true,
      true,
      true,
      true,
      true,
    );
  });

  it("warns clearly and requires a deliberate audio-only action when screen capture is off", async () => {
    const user = userEvent.setup();
    const { onRecord } = renderDialog();
    const settingsDialog = screen.getByRole("dialog", {
      name: "New transcription",
    });

    await user.click(
      within(settingsDialog).getByRole("checkbox", {
        name: /Record the main display/i,
      }),
    );
    await user.click(
      within(settingsDialog).getByRole("button", {
        name: /Record a meeting/i,
      }),
    );

    const confirmationDialog = screen.getByRole("dialog", {
      name: "Review before recording",
    });
    expect(within(confirmationDialog).getByRole("alert")).toHaveTextContent(
      /No screen video or inline screenshots will exist/i,
    );
    expect(
      within(confirmationDialog).getByRole("button", {
        name: /Turn on screen recording, screenshots, and visual cues/i,
      }),
    ).toBeInTheDocument();
    expect(onRecord).not.toHaveBeenCalled();

    await user.click(
      within(confirmationDialog).getByRole("button", {
        name: "Continue with audio only",
      }),
    );

    expect(onRecord).toHaveBeenCalledWith(
      "default-microphone",
      "default-output",
      true,
      true,
      false,
      false,
      false,
    );
  });

  it("offers a prominent recovery from audio-only review and restores all visual defaults", async () => {
    const user = userEvent.setup();
    const { onRecord } = renderDialog();
    const settingsDialog = screen.getByRole("dialog", {
      name: "New transcription",
    });

    await user.click(
      within(settingsDialog).getByRole("checkbox", {
        name: /Record the main display/i,
      }),
    );
    await user.click(
      within(settingsDialog).getByRole("button", {
        name: /Record a meeting/i,
      }),
    );
    await user.click(
      screen.getByRole("button", {
        name: /Turn on screen recording, screenshots, and visual cues/i,
      }),
    );

    expect(screen.getByText("Screen recording is ON")).toBeInTheDocument();
    expect(
      screen.getByText("Inline relevant screenshots are ON"),
    ).toBeInTheDocument();
    expect(
      screen.getByText("Visual speaker suggestions are ON"),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Start recording with screen" }),
    ).toBeDisabled();
    expect(onRecord).not.toHaveBeenCalled();
  });
});
