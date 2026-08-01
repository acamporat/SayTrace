interface SpeakerAvatarProps {
  initials: string;
  color: string;
  size?: "small" | "normal" | "large";
}

export function SpeakerAvatar({
  initials,
  color,
  size = "normal",
}: SpeakerAvatarProps) {
  return (
    <span
      className={`speaker-avatar speaker-avatar--${size}`}
      style={{ backgroundColor: color }}
      aria-hidden="true"
    >
      {initials}
    </span>
  );
}
