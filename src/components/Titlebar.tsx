import { Minus, Square, X } from "lucide-react";
import { windowAction } from "../lib/tauri";

function isMacPlatform() {
  return (
    typeof navigator !== "undefined" &&
    navigator.platform.toLocaleLowerCase().startsWith("mac")
  );
}

export function Titlebar() {
  const macOS = isMacPlatform();

  return (
    <header
      className={`titlebar titlebar--${macOS ? "macos" : "default"}`}
      data-platform={macOS ? "macos" : "default"}
      data-tauri-drag-region
    >
      <div className="titlebar__brand" data-tauri-drag-region>
        <span className="titlebar__logo" aria-hidden="true">
          <span />
          <span />
          <span />
          <span />
          <span />
        </span>
        <span data-tauri-drag-region>SayTrace</span>
      </div>
      {macOS ? (
        <span className="titlebar__native-controls-space" aria-hidden="true" />
      ) : (
        <div className="titlebar__controls" aria-label="Window controls">
          <button
            type="button"
            aria-label="Minimize"
            onClick={() => void windowAction("minimize")}
          >
            <Minus size={16} strokeWidth={1.8} />
          </button>
          <button
            type="button"
            aria-label="Maximize"
            onClick={() => void windowAction("toggleMaximize")}
          >
            <Square size={12} strokeWidth={1.7} />
          </button>
          <button
            className="titlebar__close"
            type="button"
            aria-label="Close"
            onClick={() => void windowAction("close")}
          >
            <X size={17} strokeWidth={1.7} />
          </button>
        </div>
      )}
    </header>
  );
}
