import {
  forwardRef,
  useCallback,
  useEffect,
  useImperativeHandle,
  useRef,
} from "react";

interface LevelMeterProps {
  level?: number;
  label?: string;
}

export interface LevelMeterHandle {
  setLevel: (level: number) => void;
}

function clampLevel(level: number) {
  return Math.max(0, Math.min(1, Number.isFinite(level) ? level : 0));
}

export const LevelMeter = forwardRef<LevelMeterHandle, LevelMeterProps>(
  function LevelMeter({ level = 0, label = "Input level" }, forwardedRef) {
    const meterRef = useRef<HTMLDivElement>(null);
    const fillRef = useRef<HTMLSpanElement>(null);

    const setLevel = useCallback((nextLevel: number) => {
      const normalized = clampLevel(nextLevel);
      if (fillRef.current) {
        fillRef.current.style.transform = `scaleX(${normalized})`;
      }
      if (meterRef.current) {
        meterRef.current.setAttribute("aria-valuenow", String(Math.round(normalized * 100)));
      }
    }, []);

    useImperativeHandle(forwardedRef, () => ({ setLevel }), [setLevel]);

    useEffect(() => {
      setLevel(level);
    }, [level, setLevel]);

    return (
      <div
        ref={meterRef}
        className="level-meter"
        role="meter"
        aria-label={label}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={Math.round(clampLevel(level) * 100)}
      >
        <span
          ref={fillRef}
          className="level-meter__fill"
          style={{ transform: `scaleX(${clampLevel(level)})` }}
          aria-hidden="true"
        />
      </div>
    );
  },
);
