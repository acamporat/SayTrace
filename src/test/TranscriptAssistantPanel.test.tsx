import { fireEvent, render, screen, waitFor, cleanup } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { TranscriptAssistantPanel } from "../features/transcript/TranscriptAssistantPanel";

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("../lib/tauri", () => ({ invokeCommand: invoke, isTauriRuntime: () => true, openExternalUrl: vi.fn() }));
afterEach(() => { cleanup(); vi.clearAllMocks(); });

function panel() {
  const ask = vi.fn(async () => undefined);
  render(<TranscriptAssistantPanel meetingTitle="Test meeting" messages={[]}
    status={{ state: "ready", backend: "Ollama", endpoint: "127.0.0.1", selectedModel: "local-model", models: [{ name: "local-model", sizeBytes: 100 }] }}
    loading={false} transcriptReady onOpenSpeakers={vi.fn()} onAsk={ask}
    onClear={vi.fn(async () => undefined)} onRefreshStatus={vi.fn(async () => undefined)} onOpenCitation={vi.fn()} />);
  return ask;
}

describe("optional ChatGPT meeting questions", () => {
  it("defaults to local AI without checking the cloud account", () => {
    const ask = panel();
    fireEvent.click(screen.getByText("What decisions were made?"));
    expect(ask).toHaveBeenCalledWith("What decisions were made?", "local-model", "local", false);
    expect(invoke).not.toHaveBeenCalled();
  });

  it("requires explicit sharing consent and clears it when switching providers", async () => {
    invoke.mockResolvedValue({ available: true, signedIn: true, models: [{ id: "cloud-model", displayName: "Cloud model" }] });
    const ask = panel();
    const provider = screen.getByLabelText("Answer with");
    fireEvent.change(provider, { target: { value: "chatgpt" } });
    await waitFor(() => expect(screen.getByText("Connected")).toBeInTheDocument());
    expect(screen.getByText("What decisions were made?")).toBeDisabled();
    expect(ask).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("checkbox"));
    fireEvent.click(screen.getByText("What decisions were made?"));
    expect(ask).toHaveBeenCalledWith("What decisions were made?", "cloud-model", "chatgpt", true);
    fireEvent.change(provider, { target: { value: "local" } });
    fireEvent.change(provider, { target: { value: "chatgpt" } });
    expect(screen.getByRole("checkbox")).not.toBeChecked();
    expect(screen.getByText("What decisions were made?")).toBeDisabled();
  });

  it("keeps questions disabled when signed out", async () => {
    invoke.mockResolvedValue({ available: true, signedIn: false, models: [] });
    panel();
    fireEvent.change(screen.getByLabelText("Answer with"), { target: { value: "chatgpt" } });
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("get_chatgpt_agent_status"));
    expect(screen.getByRole("button", { name: "Continue with ChatGPT" })).toBeEnabled();
    expect(screen.getByRole("checkbox")).toBeDisabled();
    expect(screen.getByText("What decisions were made?")).toBeDisabled();
  });
});
