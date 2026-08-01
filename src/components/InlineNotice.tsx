import { CheckCircle2, Info, TriangleAlert, X } from "lucide-react";

interface InlineNoticeProps {
  tone?: "info" | "success" | "warning";
  children: React.ReactNode;
  onDismiss?: () => void;
}

export function InlineNotice({
  tone = "info",
  children,
  onDismiss,
}: InlineNoticeProps) {
  const Icon =
    tone === "success"
      ? CheckCircle2
      : tone === "warning"
        ? TriangleAlert
        : Info;
  return (
    <div className={`inline-notice inline-notice--${tone}`} role="status">
      <Icon size={18} strokeWidth={1.8} />
      <span>{children}</span>
      {onDismiss ? (
        <button type="button" aria-label="Dismiss" onClick={onDismiss}>
          <X size={16} />
        </button>
      ) : null}
    </div>
  );
}
