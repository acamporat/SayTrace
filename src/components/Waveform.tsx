import { useMemo } from "react";

interface WaveformProps {
  progress: number;
  onSeek: (progress: number) => void;
  label?: string;
}

export function Waveform({
  progress,
  onSeek,
  label = "Meeting audio timeline",
}: WaveformProps) {
  const bars = useMemo(
    () =>
      Array.from({ length: 118 }, (_, index) => {
        const pseudo = Math.abs(
          Math.sin(index * 0.87) * 0.55 + Math.sin(index * 0.22) * 0.42,
        );
        return Math.max(0.16, pseudo);
      }),
    [],
  );

  return (
    <button
      className="waveform"
      type="button"
      aria-label={label}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={Math.round(progress * 100)}
      role="slider"
      onClick={(event) => {
        const bounds = event.currentTarget.getBoundingClientRect();
        onSeek((event.clientX - bounds.left) / bounds.width);
      }}
    >
      {bars.map((height, index) => (
        <span
          key={index}
          className={index / bars.length <= progress ? "is-played" : ""}
          style={{ height: `${height * 88}%` }}
        />
      ))}
      <i style={{ left: `${progress * 100}%` }} />
    </button>
  );
}
