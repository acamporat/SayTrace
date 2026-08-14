import {
  forwardRef,
  useCallback,
  useEffect,
  useId,
  useImperativeHandle,
  useRef,
} from "react";

interface WaveformProps {
  progress: number;
  onSeek: (progress: number) => void;
  label?: string;
}

export interface WaveformHandle {
  setProgress: (progress: number) => void;
}

const BAR_COUNT = 118;
const VIEWBOX_HEIGHT = 35;
const WAVEFORM_PATH = Array.from({ length: BAR_COUNT }, (_, index) => {
  const pseudo = Math.abs(
    Math.sin(index * 0.87) * 0.55 + Math.sin(index * 0.22) * 0.42,
  );
  const height = Math.max(0.16, pseudo) * (VIEWBOX_HEIGHT - 4);
  const top = (VIEWBOX_HEIGHT - height) / 2;
  return `M${index + 0.5} ${top.toFixed(2)}V${(top + height).toFixed(2)}`;
}).join("");

function clampProgress(progress: number) {
  return Math.max(0, Math.min(1, Number.isFinite(progress) ? progress : 0));
}

export const Waveform = forwardRef<WaveformHandle, WaveformProps>(
  function Waveform(
    { progress, onSeek, label = "Meeting audio timeline" },
    forwardedRef,
  ) {
    const rootRef = useRef<HTMLButtonElement>(null);
    const clipRef = useRef<SVGRectElement>(null);
    const cursorRef = useRef<SVGLineElement>(null);
    const clipId = useId().replace(/:/g, "");

    const setProgress = useCallback((nextProgress: number) => {
      const normalized = clampProgress(nextProgress);
      const position = normalized * BAR_COUNT;
      clipRef.current?.setAttribute("width", String(position));
      cursorRef.current?.setAttribute("x1", String(position));
      cursorRef.current?.setAttribute("x2", String(position));
      rootRef.current?.setAttribute(
        "aria-valuenow",
        String(Math.round(normalized * 100)),
      );
    }, []);

    useImperativeHandle(forwardedRef, () => ({ setProgress }), [setProgress]);

    useEffect(() => {
      setProgress(progress);
    }, [progress, setProgress]);

    const initialPosition = clampProgress(progress) * BAR_COUNT;
    return (
      <button
        ref={rootRef}
        className="waveform"
        type="button"
        aria-label={label}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={Math.round(clampProgress(progress) * 100)}
        role="slider"
        onClick={(event) => {
          const bounds = event.currentTarget.getBoundingClientRect();
          onSeek((event.clientX - bounds.left) / bounds.width);
        }}
      >
        <svg
          viewBox={`0 0 ${BAR_COUNT} ${VIEWBOX_HEIGHT}`}
          preserveAspectRatio="none"
          aria-hidden="true"
        >
          <defs>
            <clipPath id={clipId}>
              <rect
                ref={clipRef}
                x="0"
                y="0"
                width={initialPosition}
                height={VIEWBOX_HEIGHT}
              />
            </clipPath>
          </defs>
          <path className="waveform__bars" d={WAVEFORM_PATH} />
          <path
            className="waveform__bars waveform__bars--played"
            d={WAVEFORM_PATH}
            clipPath={`url(#${clipId})`}
          />
          <line
            ref={cursorRef}
            className="waveform__cursor"
            x1={initialPosition}
            x2={initialPosition}
            y1="0"
            y2={VIEWBOX_HEIGHT}
          />
        </svg>
      </button>
    );
  },
);
