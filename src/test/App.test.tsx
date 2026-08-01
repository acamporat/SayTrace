import { fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import App from "../App";
import { ModelSetupView } from "../features/setup/ModelSetupView";

describe("SayTrace workspace", () => {
  it("renders the approved transcript editor with categorical speaker states", () => {
    render(<App />);

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

  it("navigates between library, voice profiles, and settings", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(screen.getByRole("button", { name: "Library" }));
    expect(screen.getByRole("heading", { name: "Library" })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Voice profiles" }));
    expect(
      screen.getByRole("heading", { name: "Voice profiles" }),
    ).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Settings" }));
    expect(screen.getByRole("heading", { name: "Settings" })).toBeInTheDocument();
    expect(
      screen.getByText("Local transcription models installed"),
    ).toBeInTheDocument();
  });

  it("explicitly confirms a clean local speaker sample without renderer embeddings", async () => {
    const user = userEvent.setup();
    render(<App />);

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

    await user.click(
      screen.getByRole("button", { name: "Accept Sam Rivera" }),
    );

    expect(screen.getByText("Speaker matched to Sam Rivera.")).toBeInTheDocument();
    expect(screen.getByText("Sam Rivera", { selector: ".speaker-card strong" })).toBeInTheDocument();
  });

  it("commits an inline speaker rename when focus leaves the field", async () => {
    const user = userEvent.setup();
    render(<App />);

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
});

describe("Model setup", () => {
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
      screen.getByText(/included automatically with the Windows installer/i),
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
          device: "NVIDIA RTX 2080 Ti",
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

  it("keeps the token available and shows an alert when setup fails", async () => {
    const user = userEvent.setup();
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
          device: "NVIDIA RTX 2080 Ti",
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
          device: "NVIDIA RTX 2080 Ti",
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
