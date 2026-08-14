import { render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Titlebar } from "../components/Titlebar";
import { windowAction } from "../lib/tauri";

vi.mock("../lib/tauri", () => ({
  windowAction: vi.fn(async () => undefined),
}));

const originalPlatform = navigator.platform;

function setPlatform(platform: string) {
  Object.defineProperty(navigator, "platform", {
    configurable: true,
    value: platform,
  });
}

afterEach(() => {
  setPlatform(originalPlatform);
  vi.clearAllMocks();
});

describe("Titlebar", () => {
  it("reserves room for native macOS controls without duplicating them", () => {
    setPlatform("MacIntel");
    const { container } = render(<Titlebar />);

    const titlebar = screen.getByRole("banner");
    expect(titlebar).toHaveAttribute("data-platform", "macos");
    expect(screen.queryByLabelText("Window controls")).not.toBeInTheDocument();
    expect(
      container.querySelector(".titlebar__native-controls-space"),
    ).toBeInTheDocument();
    expect(windowAction).not.toHaveBeenCalled();
  });

  it("keeps the existing desktop controls for non-Mac browser previews", () => {
    setPlatform("Win32");
    render(<Titlebar />);

    expect(screen.getByRole("banner")).toHaveAttribute(
      "data-platform",
      "default",
    );
    const controls = screen.getByLabelText("Window controls");
    expect(
      Array.from(controls.querySelectorAll("button"), (button) =>
        button.getAttribute("aria-label"),
      ),
    ).toEqual(["Minimize", "Maximize", "Close"]);
  });
});
