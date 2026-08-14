import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import App from "../App";

const desktop = vi.hoisted(() => {
  const handlers = new Map<string, (payload: unknown) => void>();
  let runtimeReady = false;
  let recoveryVisible = false;

  const invokeCommand = vi.fn(async (command: string) => {
    switch (command) {
      case "get_app_status":
        return {
          appVersion: "0.2.0",
          schemaVersion: 1,
          firstRun: false,
          modelReady: runtimeReady,
          offlineReady: runtimeReady,
          activeRecording: {
            state: "idle",
            elapsedMs: 0,
            microphoneActive: false,
            systemAudioActive: false,
            microphoneLevel: 0,
            systemAudioLevel: 0,
            droppedCapturePackets: 0,
            droppedCaptionChunks: 0,
          },
          worker: {
            state: "ready",
            protocolVersion: 1,
            pipelineVersion: "2026.08.13.1",
          },
          capabilities: { gpu: "Apple MLX (Metal)" },
        };
      case "get_model_status":
        return {
          runtime: runtimeReady ? "ready" : "checking",
          liveModel: "ready",
          finalModel: "ready",
          diarizationModel: "ready",
          device: "Apple MLX (Metal)",
          diskRequiredGb: 2,
          diskAvailableGb: 100,
        };
      case "list_meetings":
        return recoveryVisible
          ? [
              {
                id: "recovered-meeting",
                title: "Recovered meeting",
                createdAt: "2026-08-13T12:00:00Z",
                durationMs: 30_000,
                status: "processing",
                sourceType: "recording",
                sourceKind: "recording",
                speakerCount: 0,
              },
            ]
          : [];
      case "list_voice_profiles":
      case "list_processing_jobs":
      case "list_audio_devices":
        return [];
      default:
        throw new Error(`Unexpected desktop command in test: ${command}`);
    }
  });

  return {
    handlers,
    invokeCommand,
    reset() {
      runtimeReady = false;
      recoveryVisible = false;
      handlers.clear();
      invokeCommand.mockClear();
    },
    completeRuntime() {
      runtimeReady = true;
    },
    completeRecovery() {
      recoveryVisible = true;
    },
    emit(name: string, payload: unknown) {
      handlers.get(name)?.(payload);
    },
  };
});

vi.mock("../lib/tauri", () => ({
  createAssetObjectUrl: vi.fn(),
  invokeCommand: desktop.invokeCommand,
  isTauriRuntime: () => true,
  listenEvent: vi.fn(
    async (name: string, handler: (payload: unknown) => void) => {
      desktop.handlers.set(name, handler);
      return () => desktop.handlers.delete(name);
    },
  ),
}));

describe("desktop startup completion events", () => {
  beforeEach(() => desktop.reset());

  it("catches early runtime completion and refreshes recovered library state", async () => {
    const user = userEvent.setup();
    render(<App />);

    await waitFor(() => {
      expect(desktop.handlers.has("startup://changed")).toBe(true);
    });
    await user.click(screen.getByRole("button", { name: "Settings" }));
    expect(await screen.findByText("Setup needed")).toBeInTheDocument();

    await act(async () => {
      desktop.completeRuntime();
      desktop.emit("startup://changed", {
        area: "runtime",
        status: "ready",
      });
    });
    expect(await screen.findByText("Ready offline")).toBeInTheDocument();

    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 180));
    });
    const meetingCallsBeforeRecovery = desktop.invokeCommand.mock.calls.filter(
      ([command]) => command === "list_meetings",
    ).length;

    await act(async () => {
      desktop.completeRecovery();
      desktop.emit("startup://changed", {
        area: "recovery",
        status: "complete",
      });
    });
    await waitFor(() => {
      const meetingCalls = desktop.invokeCommand.mock.calls.filter(
        ([command]) => command === "list_meetings",
      ).length;
      expect(meetingCalls).toBeGreaterThan(meetingCallsBeforeRecovery);
    });

    await user.click(screen.getByRole("button", { name: "Library" }));
    expect((await screen.findAllByText("Recovered meeting")).length).toBeGreaterThan(0);
  });
});
