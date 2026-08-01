import {
  Check,
  ChevronLeft,
  ChevronRight,
  Cpu,
  Download,
  ExternalLink,
  Eye,
  EyeOff,
  HardDrive,
  LockKeyhole,
  ShieldCheck,
  Sparkles,
} from "lucide-react";
import { useEffect, useState } from "react";
import { openExternalUrl } from "../../lib/tauri";
import type {
  ModelPackStatus,
  ModelSetupKey,
  ModelSetupProgressEvent,
} from "../../types";

interface ModelSetupViewProps {
  status: ModelPackStatus;
  progress?: ModelSetupProgressEvent;
  onBack: () => void;
  onInstall: (token: string) => Promise<void>;
}

const modelOrder: ModelSetupKey[] = [
  "live_asr_en",
  "final_asr_en",
  "alignment_en",
  "diarization",
  "speaker_embedding",
];

const modelLabels: Record<ModelSetupKey, string> = {
  live_asr_en: "Live captions",
  final_asr_en: "Final transcript",
  alignment_en: "Word alignment",
  diarization: "Speaker separation",
  speaker_embedding: "Voice matching",
};

const phaseLabels: Record<ModelSetupProgressEvent["phase"], string> = {
  checking: "Checking existing files",
  downloading: "Downloading pinned files",
  verifying: "Verifying file integrity",
  publishing: "Publishing verified model",
  complete: "Model ready",
  failed: "Setup needs attention",
};

const communityAccessUrl =
  "https://huggingface.co/pyannote/speaker-diarization-community-1";
const tokenSettingsUrl = "https://huggingface.co/settings/tokens";

function setupErrorMessage(error: unknown) {
  if (error instanceof Error && error.message) return error.message;
  if (typeof error === "string" && error.trim()) return error;
  if (
    error &&
    typeof error === "object" &&
    "message" in error &&
    typeof error.message === "string" &&
    error.message.trim()
  ) {
    return error.message.replace(/^worker failed:\s*/i, "");
  }
  return "Model setup could not complete. Check the access steps above, then retry.";
}

export function ModelSetupView({
  status,
  progress,
  onBack,
  onInstall,
}: ModelSetupViewProps) {
  const [token, setToken] = useState("");
  const [showToken, setShowToken] = useState(false);
  const [installing, setInstalling] = useState(false);
  const [installError, setInstallError] = useState<string>();
  const runtimeReady = status.runtime === "ready";
  const modelsInstalled =
    runtimeReady &&
    status.liveModel === "ready" &&
    status.finalModel === "ready" &&
    status.diarizationModel === "ready";
  const [installed, setInstalled] = useState(modelsInstalled);

  useEffect(() => {
    setInstalled(modelsInstalled);
  }, [modelsInstalled]);

  const perModelProgress = progress
    ? Math.min(
        100,
        Math.max(
          0,
          (progress.completed_steps / Math.max(1, progress.total_steps)) * 100,
        ),
      )
    : 0;
  const modelIndex = progress
    ? Math.max(0, modelOrder.indexOf(progress.key))
    : 0;
  const overallProgress = progress
    ? Math.min(
        100,
        ((modelIndex + perModelProgress / 100) / modelOrder.length) * 100,
      )
    : 0;

  async function install() {
    setInstalling(true);
    setInstallError(undefined);
    try {
      await onInstall(token);
      setInstalled(true);
      setToken("");
    } catch (error) {
      setInstallError(setupErrorMessage(error));
    } finally {
      setInstalling(false);
    }
  }

  return (
    <main className="workspace setup-workspace">
      <button className="setup-back" type="button" onClick={onBack}>
        <ChevronLeft size={18} /> Back to settings
      </button>
      <div className="setup-shell">
        <header>
          <span className="setup-icon">
            <Sparkles size={29} />
          </span>
          <h1>{installed ? "Models are ready" : "Set up local transcription"}</h1>
          <p>
            Download the language and speaker models once, then transcribe
            without an internet connection.
          </p>
        </header>

        <div className="setup-capabilities">
          <div>
            <Download size={21} />
            <strong>One-time download</strong>
            <span>Revision-pinned model packs</span>
          </div>
          <div>
            <ShieldCheck size={21} />
            <strong>Private processing</strong>
            <span>Media never leaves this PC</span>
          </div>
          <div>
            <Cpu size={21} />
            <strong>GPU accelerated</strong>
            <span>{status.device}</span>
          </div>
        </div>

        {installed ? (
          <section className="setup-complete">
            <span>
              <Check size={26} />
            </span>
            <div>
              <h2>Everything is installed</h2>
              <p>
                Live captions, final transcription, alignment, diarization, and
                voice matching are available offline.
              </p>
            </div>
            <dl>
              <div>
                <dt>Live captions</dt>
                <dd>distil-large-v3.5</dd>
              </div>
              <div>
                <dt>Final transcript</dt>
                <dd>large-v3</dd>
              </div>
              <div>
                <dt>Speaker separation</dt>
                <dd>Community-1</dd>
              </div>
            </dl>
            <button className="primary-button" type="button" onClick={onBack}>
              Continue to SayTrace <ChevronRight size={18} />
            </button>
          </section>
        ) : !runtimeReady ? (
          <section className="setup-form setup-runtime-required">
            <h2>This installation needs repair</h2>
            <p>
              Local processing is included automatically with the Windows
              installer, but its files are missing or incomplete on this PC.
            </p>
            <div className="setup-runtime-handoff">
              <Cpu size={22} />
              <span>
                <strong>Reinstall SayTrace</strong>
                <small>
                  Close the app and run the complete SayTrace installer
                  again. It restores the NVIDIA runtime and CPU fallback without
                  deleting your library.
                </small>
              </span>
            </div>
            <button className="secondary-button" type="button" onClick={onBack}>
              Back to settings
            </button>
            <small>
              You do not need to find or install a separate runtime pack.
            </small>
          </section>
        ) : (
          <section className="setup-form">
            <h2>Connect for the one-time model download</h2>
            <p>
              Community-1 requires a free Hugging Face account and acceptance of
              its model terms.
            </p>
            <ol>
              <li>Open Community-1 and sign in or create a free account.</li>
              <li>Complete “Agree and access repository” on that page.</li>
              <li>Create a read token from the same account and paste it below.</li>
            </ol>
            <div className="setup-access-actions">
              <button
                className="secondary-button"
                type="button"
                onClick={() => void openExternalUrl(communityAccessUrl)}
              >
                1. Open Community-1 access <ExternalLink size={15} />
              </button>
              <button
                className="secondary-button"
                type="button"
                onClick={() => void openExternalUrl(tokenSettingsUrl)}
              >
                2. Create read token <ExternalLink size={15} />
              </button>
            </div>
            <label>
              <span>Hugging Face access token</span>
              <div>
                <LockKeyhole size={18} />
                <input
                  aria-label="Hugging Face access token"
                  type={showToken ? "text" : "password"}
                  value={token}
                  onChange={(event) => setToken(event.target.value)}
                  placeholder="hf_••••••••••••••••"
                />
                <button
                  type="button"
                  aria-label={showToken ? "Hide token" : "Show token"}
                  onClick={() => setShowToken((value) => !value)}
                >
                  {showToken ? <EyeOff size={18} /> : <Eye size={18} />}
                </button>
              </div>
            </label>
            <div className="setup-storage">
              <HardDrive size={19} />
              <span>
                <strong>{status.diskRequiredGb.toFixed(1)} GB required</strong>
                {status.diskAvailableGb.toFixed(1)} GB available
              </span>
            </div>
            {progress ? (
              <div
                className="setup-progress"
                data-phase={progress.phase}
                role="status"
                aria-live="polite"
              >
                <div className="setup-progress-heading">
                  <span>
                    <strong>{modelLabels[progress.key]}</strong>
                    {phaseLabels[progress.phase]}
                  </span>
                  <b>{Math.round(perModelProgress)}%</b>
                </div>
                <progress
                  aria-label={`${modelLabels[progress.key]} progress`}
                  max={100}
                  value={perModelProgress}
                />
                <div className="setup-progress-overall">
                  <span>
                    Overall setup · model {modelIndex + 1} of {modelOrder.length}
                  </span>
                  <b>{Math.round(overallProgress)}%</b>
                </div>
                <progress
                  aria-label="Overall model setup progress"
                  max={100}
                  value={overallProgress}
                />
              </div>
            ) : null}
            {installError ? (
              <div className="setup-install-error" role="alert">
                {installError}
              </div>
            ) : null}
            <button
              className="primary-button"
              type="button"
              disabled={!token.trim() || installing}
              onClick={() => void install()}
            >
              {installing ? "Installing model packs…" : "Download and install"}
            </button>
            <small>
              The token is used for setup only and is discarded after verified
              downloads complete.
            </small>
          </section>
        )}
      </div>
    </main>
  );
}
