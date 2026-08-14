import { act, render, screen } from "@testing-library/react";
import { createRef } from "react";
import { describe, expect, it, vi } from "vitest";
import {
  LevelMeter,
  type LevelMeterHandle,
} from "../components/LevelMeter";
import { Waveform, type WaveformHandle } from "../components/Waveform";

describe("high-frequency display components", () => {
  it("updates a single CSS meter fill without a React state update", () => {
    const meterRef = createRef<LevelMeterHandle>();
    const { container } = render(
      <LevelMeter ref={meterRef} label="Microphone input level" />,
    );

    act(() => meterRef.current?.setLevel(0.73));

    expect(screen.getByRole("meter")).toHaveAttribute("aria-valuenow", "73");
    expect(container.querySelectorAll(".level-meter__fill")).toHaveLength(1);
    expect(container.querySelector(".level-meter__fill")).toHaveStyle({
      transform: "scaleX(0.73)",
    });
  });

  it("updates waveform progress through its compact SVG handle", () => {
    const waveformRef = createRef<WaveformHandle>();
    const { container } = render(
      <Waveform ref={waveformRef} progress={0} onSeek={vi.fn()} />,
    );

    act(() => waveformRef.current?.setProgress(0.5));

    expect(screen.getByRole("slider")).toHaveAttribute("aria-valuenow", "50");
    expect(container.querySelector("clipPath rect")).toHaveAttribute(
      "width",
      "59",
    );
    expect(container.querySelector(".waveform__cursor")).toHaveAttribute(
      "x1",
      "59",
    );
  });
});
