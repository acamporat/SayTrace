interface LevelMeterProps {
  level: number;
  barCount?: number;
}

export function LevelMeter({ level, barCount = 82 }: LevelMeterProps) {
  return (
    <div className="level-meter" aria-label={`Input level ${Math.round(level * 100)}%`}>
      {Array.from({ length: barCount }, (_, index) => (
        <span
          key={index}
          className={index / barCount < level ? "is-on" : ""}
        />
      ))}
    </div>
  );
}
