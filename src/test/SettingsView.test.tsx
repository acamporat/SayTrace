import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { SettingsView } from "../features/settings/SettingsView";
import { modelStatus } from "../data/mock";

function renderSettings(onExportSpeakers: () => Promise<string>, onImportSpeakers: () => Promise<string>) {
  render(<SettingsView
    modelStatus={modelStatus}
    workerStatus={{ state: "ready", protocolVersion: 1, pipelineVersion: "test" }}
    onOpenSetup={vi.fn()} onBackup={vi.fn()} onCheckModels={vi.fn()}
    onOpenLibrary={vi.fn()} onRestartWorker={vi.fn()}
    onExportSpeakers={onExportSpeakers} onImportSpeakers={onImportSpeakers}
  />);
}

describe("Speaker backup controls", () => {
  it.each(["save", "restore"] as const)("disables both controls until %s finishes and reports completion", async (operation) => {
    const user = userEvent.setup();
    let complete!: (value: string) => void;
    const pending = new Promise<string>((resolve) => { complete = resolve; });
    const action = vi.fn(() => pending);
    const other = vi.fn(async () => "Not called");
    renderSettings(operation === "save" ? action : other, operation === "restore" ? action : other);
    const save = screen.getByRole("button", { name: "Save speaker backup" });
    const restore = screen.getByRole("button", { name: "Restore speaker backup" });
    await user.click(operation === "save" ? save : restore);
    expect(action).toHaveBeenCalledOnce();
    expect(save).toBeDisabled();
    expect(restore).toBeDisabled();
    await act(async () => { complete("Speaker backup completed."); });
    expect(screen.getByRole("status")).toHaveTextContent("Speaker backup completed.");
    expect(save).toBeEnabled();
    expect(restore).toBeEnabled();
    expect(other).not.toHaveBeenCalled();
  });

  it("reports restore failures and permits another attempt", async () => {
    const user = userEvent.setup();
    const restore = vi.fn().mockRejectedValueOnce(new Error("Invalid speaker backup version.")).mockResolvedValueOnce("Speaker matches restored.");
    renderSettings(vi.fn(async () => "Saved."), restore);
    const button = screen.getByRole("button", { name: "Restore speaker backup" });
    await user.click(button);
    expect(screen.getByRole("status")).toHaveTextContent("Invalid speaker backup version.");
    expect(button).toBeEnabled();
    expect(screen.getByRole("button", { name: "Save speaker backup" })).toBeEnabled();
    await user.click(button);
    expect(screen.getByRole("status")).toHaveTextContent("Speaker matches restored.");
  });
});
