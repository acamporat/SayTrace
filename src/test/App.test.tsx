import { fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import App from "../App";
import { ModelSetupView } from "../features/setup/ModelSetupView";

describe("SayTrace workspace", () => {
  it("renders the approved transcript editor with categorical speaker states", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(screen.getByRole("tab", { name: "Speakers" }));

    expect(
      screen.getByRole("heading", { name: "Weekly production meeting" }),
    ).toBeInTheDocument();
    expect(screen.getAllByText("Matched").length).toBeGreaterThan(0);
    expect(screen.getByText("Review")).toBeInTheDocument();
    expect(screen.queryByText(/confidence/i)).not.toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Play" }),
    ).toBeInTheDocument();
  });

  it("opens search from Ctrl+F and highlights transcript results", async () => {
    const user = userEvent.setup();
    render(<App />);

    fireEvent.keyDown(window, { key: "f", ctrlKey: true });
    const search = screen.getByRole("textbox", { name: "Search transcript" });
    expect(search).toHaveFocus();
    await user.type(search, "Activation");

    expect(screen.getByText("1 found")).toBeInTheDocument();
    expect(screen.getByText("Activation", { selector: "mark" })).toBeInTheDocument();
  });

  it("renames meetings and exposes persistent review and replacement actions", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(screen.getByRole("button", { name: "Rename meeting" }));
    const title = screen.getByRole("textbox", { name: "Meeting title" });
    await user.clear(title);
    await user.type(title, "Production launch review");
    await user.click(screen.getByRole("button", { name: "Save" }));
    expect(
      screen.getByRole("heading", { name: "Production launch review" }),
    ).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Flag for review" }));
    expect(
      screen.getByRole("button", { name: "Clear review flag" }),
    ).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Find and replace" }));
    await user.type(screen.getByRole("textbox", { name: "Find text" }), "Activation");
    await user.type(
      screen.getByRole("textbox", { name: "Replacement text" }),
      "Adoption",
    );
    await user.click(screen.getByRole("button", { name: "Replace all" }));
    expect(screen.getByText(/Adoption is up 12%/)).toBeInTheDocument();
  });

  it("starts a browser-preview recording from the new transcription dialog", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(
      screen.getByRole("button", { name: /new transcription/i }),
    );
    const dialog = screen.getByRole("dialog", { name: "New transcription" });
    await user.selectOptions(
      within(dialog).getByRole("combobox", {
        name: "Microphone input device",
      }),
      "mic-array",
    );
    await user.selectOptions(
      within(dialog).getByRole("combobox", {
        name: "System audio output device",
      }),
      "output-headset",
    );
    await user.click(
      within(dialog).getByRole("checkbox", {
        name: /This microphone is only me/i,
      }),
    );
    await user.click(
      within(dialog).getByRole("button", { name: /record a meeting/i }),
    );
    const confirmation = screen.getByRole("dialog", {
      name: "Review before recording",
    });
    await user.click(
      within(confirmation).getByRole("checkbox", {
        name: /I understand that the entire main display/i,
      }),
    );
    await user.click(
      within(confirmation).getByRole("button", {
        name: "Start recording with screen",
      }),
    );

    expect(
      screen.getByRole("heading", { name: "Live draft" }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /stop and finalize/i }),
    ).toBeInTheDocument();
    expect(screen.getByText("Microphone active")).toBeInTheDocument();
    expect(screen.getByText("System audio active")).toBeInTheDocument();
    expect(
      screen.getByRole("combobox", { name: "Active microphone input" }),
    ).toHaveValue("mic-array");
    expect(
      screen.getByRole("combobox", { name: "Active system audio output" }),
    ).toHaveValue("output-headset");
    expect(screen.getByRole("main")).toHaveAttribute(
      "data-microphone-mode",
      "room",
    );
  });

  it("allows live draft captions to be disabled per recording", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(
      screen.getByRole("button", { name: /new transcription/i }),
    );
    const dialog = screen.getByRole("dialog", { name: "New transcription" });
    await user.click(
      within(dialog).getByRole("checkbox", {
        name: /Show live draft captions/i,
      }),
    );
    await user.click(
      within(dialog).getByRole("button", { name: /record a meeting/i }),
    );
    const confirmation = screen.getByRole("dialog", {
      name: "Review before recording",
    });
    await user.click(
      within(confirmation).getByRole("checkbox", {
        name: /I understand that the entire main display/i,
      }),
    );
    await user.click(
      within(confirmation).getByRole("button", {
        name: "Start recording with screen",
      }),
    );

    expect(
      screen.getByText("Live draft captions are off for this recording."),
    ).toBeInTheDocument();
    expect(
      screen.queryByText(/Thanks everyone for joining/i),
    ).not.toBeInTheDocument();
    expect(
      screen.getByText(/saved tracks will be transcribed after you stop/i),
    ).toBeInTheDocument();
  });

  it("defaults screen context on and requires explicit final main-display acknowledgement", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(
      screen.getByRole("button", { name: /new transcription/i }),
    );
    const dialog = screen.getByRole("dialog", { name: "New transcription" });
    expect(
      within(dialog).getByRole("checkbox", {
        name: /Record the main display/i,
      }),
    ).toBeChecked();
    expect(
      within(dialog).getByRole("checkbox", {
        name: /Add relevant screenshots to the transcript/i,
      }),
    ).toBeChecked();
    expect(
      within(dialog).getByRole("checkbox", {
        name: /Use meeting-app visual cues to suggest speakers/i,
      }),
    ).toBeChecked();
    expect(
      within(dialog).getByText(/Everything visible on the main display/i),
    ).toBeInTheDocument();

    const recordButton = within(dialog).getByRole("button", {
      name: /record a meeting/i,
    });
    expect(recordButton).toBeEnabled();
    await user.click(recordButton);

    const confirmation = screen.getByRole("dialog", {
      name: "Review before recording",
    });
    const startButton = within(confirmation).getByRole("button", {
      name: "Start recording with screen",
    });
    expect(startButton).toBeDisabled();
    expect(
      within(confirmation).getByText("Screen recording is ON"),
    ).toBeInTheDocument();
    await user.click(
      within(confirmation).getByRole("checkbox", {
        name: /I understand that the entire main display/i,
      }),
    );
    expect(startButton).toBeEnabled();
    await user.click(startButton);

    expect(
      screen.getByText("The main display is being recorded"),
    ).toBeInTheDocument();
    expect(screen.getByText("Screen capture active")).toBeInTheDocument();
    expect(
      screen.getByText(/Relevant screenshots enabled/),
    ).toBeInTheDocument();
    expect(
      screen.getByText(/Visual speaker suggestions enabled/),
    ).toBeInTheDocument();
  });

  it("navigates between library, voice profiles, and settings", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(screen.getByRole("button", { name: "Library" }));
    expect(screen.getByRole("heading", { name: "Library" })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Voice profiles" }));
    expect(
      screen.getByRole("heading", { name: "Voice profiles" }),
    ).toBeInTheDocument();
    expect(
      screen.getByText("Protected with your macOS Keychain"),
    ).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Settings" }));
    expect(screen.getByRole("heading", { name: "Settings" })).toBeInTheDocument();
    expect(
      screen.getByText("Local transcription models installed"),
    ).toBeInTheDocument();
    expect(screen.getByText("Apple Silicon acceleration")).toBeInTheDocument();
    expect(screen.getByText(/Library\/Application Support/)).toBeInTheDocument();
    expect(screen.getByText(/turn on FileVault/i)).toBeInTheDocument();
  });

  it("explicitly confirms a clean local speaker sample without renderer embeddings", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(screen.getByRole("tab", { name: "Speakers" }));

    expect(
      screen.getByText("Visual suggestion · Review", {
        selector: ".speaker-card__visual-evidence strong",
      }),
    ).toBeInTheDocument();
    expect(
      screen.getByText(/Microsoft Teams highlighted Sam Rivera/i),
    ).toBeInTheDocument();

    await user.click(
      screen.getByRole("button", { name: /create voice profile/i }),
    );
    await user.selectOptions(
      screen.getByRole("combobox", { name: "Voice profile" }),
      "profile-sam",
    );
    await user.click(
      screen.getByRole("button", { name: "Confirm clean sample" }),
    );

    expect(
      await screen.findByText("Voice sample confirmed for Sam Rivera."),
    ).toBeInTheDocument();
  });

  it("allows an explicit review candidate decision", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(screen.getByRole("tab", { name: "Speakers" }));

    await user.click(
      screen.getByRole("button", { name: "Accept Sam Rivera" }),
    );

    expect(screen.getByText("Speaker matched to Sam Rivera.")).toBeInTheDocument();
    expect(screen.getByText("Sam Rivera", { selector: ".speaker-card strong" })).toBeInTheDocument();
  });

  it("commits an inline speaker rename when focus leaves the field", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(screen.getByRole("tab", { name: "Speakers" }));

    const renameButtons = screen.getAllByRole("button", { name: "Rename" });
    await user.click(renameButtons[0]);
    const name = screen.getByRole("textbox", { name: "Speaker name" });
    await user.clear(name);
    await user.type(name, "Alex Morgan");
    await user.tab();

    expect(
      screen.getByText("Alex Morgan", { selector: ".speaker-card strong" }),
    ).toBeInTheDocument();
    expect(screen.getByText("Speaker renamed.")).toBeInTheDocument();
  });

  it("highlights the corresponding speaker card when a transcript turn is selected", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(screen.getByRole("tab", { name: "Speakers" }));

    await user.click(screen.getByText(/One thing to note/));

    expect(
      screen.getByText("Speaker 3", { selector: ".speaker-card.is-selected strong" }),
    ).toBeInTheDocument();
  });

  it("automatically creates a voice profile when an unknown speaker is renamed", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(screen.getByRole("tab", { name: "Speakers" }));

    await user.click(
      screen.getByRole("button", { name: "More actions for Speaker 3" }),
    );
    await user.click(screen.getByRole("button", { name: "Rename speaker" }));
    const name = screen.getByRole("textbox", { name: "Speaker name" });
    await user.clear(name);
    await user.type(name, "Taylor Reed");
    await user.tab();

    expect(
      screen.getByText("Speaker renamed and voice profile updated automatically."),
    ).toBeInTheDocument();
    expect(
      screen.getByText("Taylor Reed", { selector: ".speaker-card strong" }),
    ).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Voice profiles" }));
    const taylorProfile = screen.getByText("Taylor Reed").closest("article");
    expect(taylorProfile).not.toBeNull();
    expect(within(taylorProfile!).getByText("Ready to match")).toBeInTheDocument();
  });

  it("asks the local transcript assistant and jumps back from a citation", async () => {
    const user = userEvent.setup();
    render(<App />);

    expect(
      screen.getByRole("heading", { name: "Ask this transcript" }),
    ).toBeInTheDocument();
    expect(screen.getByText("Runs locally on this device")).toBeInTheDocument();

    await user.click(
      screen.getByRole("button", { name: "Summarize this meeting" }),
    );
    expect(
      await screen.findByText(/Early beta activation was up 12%/),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("heading", { name: "Key points" }),
    ).toBeInTheDocument();
    expect(screen.getByText("Results:")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "03:00" }));
    expect(
      screen.getByText("Maya", {
        selector: '[data-turn-id="turn-4"].is-selected strong',
      }),
    ).toBeInTheDocument();
  });
});

describe("Model setup", () => {
  it("shows runtime verification without a false repair warning", () => {
    render(
      <ModelSetupView
        status={{
          runtime: "checking",
          liveModel: "missing",
          finalModel: "missing",
          diarizationModel: "missing",
          device: "Apple MLX (Metal)",
          diskRequiredGb: 2,
          diskAvailableGb: 100,
        }}
        onBack={() => undefined}
        onInstall={async () => undefined}
      />,
    );

    expect(
      screen.getByRole("heading", { name: "Verifying local runtime…" }),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("heading", { name: "This installation needs repair" }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByLabelText("Hugging Face access token"),
    ).not.toBeInTheDocument();
  });

  it("explains that a missing bundled runtime is repaired by reinstalling the app", () => {
    render(
      <ModelSetupView
        status={{
          runtime: "missing",
          liveModel: "missing",
          finalModel: "missing",
          diarizationModel: "missing",
          device: "CPU fallback",
          diskRequiredGb: 12,
          diskAvailableGb: 100,
        }}
        onBack={() => undefined}
        onInstall={async () => undefined}
      />,
    );

    expect(
      screen.getByRole("heading", {
        name: "This installation needs repair",
      }),
    ).toBeInTheDocument();
    expect(
      screen.getByText(/included automatically with the macOS app/i),
    ).toBeInTheDocument();
    expect(
      screen.getByText(/do not need to find or install a separate runtime pack/i),
    ).toBeInTheDocument();
    expect(
      screen.queryByLabelText("Hugging Face access token"),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Download and install" }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: /runtime pack/i }),
    ).not.toBeInTheDocument();
  });

  it("does not mistake an installed runtime for installed model packs", () => {
    render(
      <ModelSetupView
        status={{
          runtime: "ready",
          liveModel: "missing",
          finalModel: "missing",
          diarizationModel: "missing",
          device: "Apple MLX (Metal)",
          diskRequiredGb: 12,
          diskAvailableGb: 100,
        }}
        onBack={() => undefined}
        onInstall={async () => undefined}
      />,
    );

    expect(
      screen.getByRole("heading", { name: "Set up local transcription" }),
    ).toBeInTheDocument();
    expect(
      screen.getByLabelText("Hugging Face access token"),
    ).toBeInTheDocument();
  });

  it("shows determinate current-model and overall setup progress", () => {
    render(
      <ModelSetupView
        status={{
          runtime: "ready",
          liveModel: "missing",
          finalModel: "missing",
          diarizationModel: "missing",
          device: "NVIDIA RTX 2080 Ti",
          diskRequiredGb: 12,
          diskAvailableGb: 100,
        }}
        progress={{
          request_id: "request-4",
          key: "diarization",
          code: "MODEL_SETUP_PROGRESS",
          phase: "verifying",
          completed_steps: 2,
          total_steps: 4,
        }}
        onBack={() => undefined}
        onInstall={async () => undefined}
      />,
    );

    expect(screen.getByText("Speaker separation")).toBeInTheDocument();
    expect(screen.getByText("Verifying file integrity")).toBeInTheDocument();
    expect(
      screen.getByRole("progressbar", {
        name: "Speaker separation progress",
      }),
    ).toHaveAttribute("value", "50");
    expect(
      screen.getByRole("progressbar", {
        name: "Overall model setup progress",
      }),
    ).toHaveAttribute("value", "70");
    expect(screen.getByText("Overall setup · model 4 of 5")).toBeInTheDocument();
  });

  it("omits the alignment step and reaches 100% for Apple MLX setup", () => {
    render(
      <ModelSetupView
        status={{
          runtime: "ready",
          liveModel: "missing",
          finalModel: "missing",
          diarizationModel: "missing",
          device: "Apple MLX (Metal)",
          diskRequiredGb: 2,
          diskAvailableGb: 100,
        }}
        progress={{
          request_id: "request-mlx-4",
          key: "speaker_embedding",
          code: "MODEL_SETUP_PROGRESS",
          phase: "complete",
          completed_steps: 4,
          total_steps: 4,
        }}
        onBack={() => undefined}
        onInstall={async () => undefined}
      />,
    );

    expect(screen.getByText("Voice matching")).toBeInTheDocument();
    expect(screen.getByText("Overall setup · model 4 of 4")).toBeInTheDocument();
    expect(
      screen.getByRole("progressbar", {
        name: "Overall model setup progress",
      }),
    ).toHaveAttribute("value", "100");
  });

  it("describes the installed Apple MLX model set accurately", () => {
    render(
      <ModelSetupView
        status={{
          runtime: "ready",
          liveModel: "ready",
          finalModel: "ready",
          diarizationModel: "ready",
          device: "Apple MLX (Metal)",
          diskRequiredGb: 2,
          diskAvailableGb: 100,
        }}
        onBack={() => undefined}
        onInstall={async () => undefined}
      />,
    );

    expect(screen.getByText("Whisper Small.en MLX Q4")).toBeInTheDocument();
    expect(
      screen.getByText("Whisper Large v3 Turbo MLX Q4"),
    ).toBeInTheDocument();
    expect(screen.queryByText(/alignment/i)).not.toBeInTheDocument();
  });

  it("keeps the token available and shows an alert when setup fails", async () => {
    const user = userEvent.setup();
    render(
      <ModelSetupView
        status={{
          runtime: "ready",
          liveModel: "missing",
          finalModel: "missing",
          diarizationModel: "missing",
          device: "Apple MLX (Metal)",
          diskRequiredGb: 12,
          diskAvailableGb: 100,
        }}
        onBack={() => undefined}
        onInstall={async () => {
          throw new Error("Model download was interrupted.");
        }}
      />,
    );
    const token = screen.getByLabelText("Hugging Face access token");
    await user.type(token, "hf_retry_token");
    await user.click(
      screen.getByRole("button", { name: "Download and install" }),
    );

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Model download was interrupted.",
    );
    expect(token).toHaveValue("hf_retry_token");
    expect(
      screen.getByRole("heading", { name: "Set up local transcription" }),
    ).toBeInTheDocument();
  });

  it("shows direct Community-1 and token actions", () => {
    render(
      <ModelSetupView
        status={{
          runtime: "ready",
          liveModel: "missing",
          finalModel: "missing",
          diarizationModel: "missing",
          device: "Apple MLX (Metal)",
          diskRequiredGb: 12,
          diskAvailableGb: 100,
        }}
        onBack={() => undefined}
        onInstall={async () => undefined}
      />,
    );

    expect(
      screen.getByRole("button", { name: /open community-1 access/i }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /create read token/i }),
    ).toBeInTheDocument();
  });

  it("shows structured Tauri setup errors instead of a generic fallback", async () => {
    const user = userEvent.setup();
    render(
      <ModelSetupView
        status={{
          runtime: "ready",
          liveModel: "missing",
          finalModel: "missing",
          diarizationModel: "missing",
          device: "Apple MLX (Metal)",
          diskRequiredGb: 12,
          diskAvailableGb: 100,
        }}
        onBack={() => undefined}
        onInstall={async () => {
          throw {
            code: "worker_unavailable",
            message:
              "worker failed: MODEL_ACCESS_DENIED: Hugging Face denied access to Community-1.",
            retryable: true,
          };
        }}
      />,
    );

    await user.type(
      screen.getByLabelText("Hugging Face access token"),
      "hf_retry_token",
    );
    await user.click(
      screen.getByRole("button", { name: "Download and install" }),
    );

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "MODEL_ACCESS_DENIED: Hugging Face denied access to Community-1.",
    );
  });
});
