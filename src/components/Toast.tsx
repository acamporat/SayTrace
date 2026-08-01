import { CheckCircle2, Info, TriangleAlert, X } from "lucide-react";

export interface ToastMessage {
  id: number;
  message: string;
  tone: "success" | "info" | "warning";
}

export function Toast({
  toast,
  onDismiss,
}: {
  toast: ToastMessage;
  onDismiss: () => void;
}) {
  const Icon =
    toast.tone === "success"
      ? CheckCircle2
      : toast.tone === "warning"
        ? TriangleAlert
        : Info;
  return (
    <div className={`toast toast--${toast.tone}`} role="status">
      <Icon size={19} />
      <span>{toast.message}</span>
      <button type="button" aria-label="Dismiss notification" onClick={onDismiss}>
        <X size={16} />
      </button>
    </div>
  );
}
