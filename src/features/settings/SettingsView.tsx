import {
  Archive,
  Check,
  ChevronRight,
  Cpu,
  Download,
  FolderOpen,
  Gauge,
  HardDrive,
  Info,
  Languages,
  RefreshCw,
  ShieldCheck,
  Sparkles,
} from "lucide-react";
import type { ModelPackStatus } from "../../types";
import type { AppStatus } from "../../types";

interface SettingsViewProps {
  modelStatus: ModelPackStatus;
  workerStatus: AppStatus["worker"];
  onOpenSetup: () => void;
  onBackup: () => void;
  onCheckModels: () => void;
  onOpenLibrary: () => void;
  onRestartWorker: () => void;
}

export function SettingsView({
  modelStatus,
  workerStatus,
  onOpenSetup,
  onBackup,
  onCheckModels,
  onOpenLibrary,
  onRestartWorker,
}: SettingsViewProps) {
  const modelFilesReady =
    modelStatus.runtime === "ready" &&
    modelStatus.liveModel === "ready" &&
    modelStatus.finalModel === "ready" &&
    modelStatus.diarizationModel === "ready";
  const offlineReady = modelFilesReady && workerStatus.state === "ready";

  return (
    <main className="workspace page-workspace settings-workspace">
      <header className="page-header">
        <div>
          <h1>Settings</h1>
          <p>Recording, processing, storage, and privacy preferences.</p>
        </div>
      </header>

      <div className="settings-layout">
        <section className="settings-section">
          <h2>Transcription</h2>
          <div className="settings-row">
            <span className="settings-row__icon">
              <Languages size={20} />
            </span>
            <div>
              <strong>Language</strong>
              <p>English is optimized and supported in this release.</p>
            </div>
            <span className="settings-value">English</span>
          </div>
          <div className="settings-row">
            <span className="settings-row__icon">
              <Sparkles size={20} />
            </span>
            <div>
              <strong>Live draft captions</strong>
              <p>
                Show provisional text while recording; choose per meeting before
                capture starts.
              </p>
            </div>
            <span className="settings-value">On by default</span>
          </div>
          <div className="settings-row">
            <span className="settings-row__icon">
              <Gauge size={20} />
            </span>
            <div>
              <strong>Auto-scroll transcript</strong>
              <p>Follow playback and the newest live captions.</p>
            </div>
            <span className="settings-value">On by default</span>
          </div>
        </section>

        <section className="settings-section">
          <h2>Models and performance</h2>
          <div className="model-summary">
            <span
              className={`model-summary__status ${
                offlineReady ? "" : "model-summary__status--missing"
              }`}
            >
              {offlineReady ? <Check size={17} /> : <Download size={16} />}
              {offlineReady
                ? "Ready offline"
                : modelFilesReady
                  ? "Worker check needed"
                  : "Setup needed"}
            </span>
            <strong>
              {offlineReady
                ? "Local transcription models installed"
                : modelFilesReady
                  ? "Local model files are installed"
                  : "Local model setup is incomplete"}
            </strong>
            <p>
              {offlineReady
                ? "Live captions, final transcription, alignment, and speaker identification are available without internet."
                : modelFilesReady
                  ? "Restart the local worker to confirm this runtime is available for offline processing."
                  : "Finish the runtime and one-time model setup before transcribing offline."}
            </p>
            <dl>
              <div>
                <dt>Processor</dt>
                <dd>{modelStatus.device}</dd>
              </div>
              <div>
                <dt>Model storage</dt>
                <dd>{modelStatus.diskRequiredGb.toFixed(1)} GB</dd>
              </div>
            </dl>
            <button type="button" onClick={onOpenSetup}>
              Manage model setup <ChevronRight size={17} />
            </button>
          </div>
          <div className="settings-row">
            <span className="settings-row__icon">
              <Cpu size={20} />
            </span>
            <div>
              <strong>GPU acceleration</strong>
              <p>Use the NVIDIA GPU when available, with automatic CPU fallback.</p>
            </div>
            <span className="settings-value">Automatic</span>
          </div>
          <div className="settings-row">
            <span className="settings-row__icon">
              <RefreshCw size={20} />
            </span>
            <div>
              <strong>Local worker</strong>
              <p>
                {workerStatus.state === "ready"
                  ? "Ready over private inherited pipes."
                  : `Worker state: ${workerStatus.state}.`}
              </p>
            </div>
            <button
              className="settings-value"
              type="button"
              onClick={onRestartWorker}
            >
              Restart
            </button>
          </div>
          <button
            className="settings-action"
            type="button"
            onClick={onCheckModels}
          >
            <RefreshCw size={18} /> Refresh model status
          </button>
        </section>

        <section className="settings-section">
          <h2>Storage and recovery</h2>
          <div className="settings-row">
            <span className="settings-row__icon">
              <FolderOpen size={20} />
            </span>
            <div>
              <strong>Managed library</strong>
              <p>%LOCALAPPDATA%\com.localtranscript.desktop\library</p>
            </div>
            <span className="settings-value">App data</span>
          </div>
          <div className="settings-row">
            <span className="settings-row__icon">
              <Download size={20} />
            </span>
            <div>
              <strong>Copy imports into library</strong>
              <p>Keep a managed original so meetings remain available.</p>
            </div>
            <span className="settings-value">On</span>
          </div>
          <div className="settings-row">
            <span className="settings-row__icon">
              <HardDrive size={20} />
            </span>
            <div>
              <strong>Available storage</strong>
              <p>{modelStatus.diskAvailableGb.toFixed(1)} GB available</p>
            </div>
            <button
              className="settings-value"
              type="button"
              onClick={onOpenLibrary}
            >
              Review files
            </button>
          </div>
          <button className="settings-action" type="button" onClick={onBackup}>
            <Archive size={18} /> Create library backup
          </button>
        </section>

        <section className="settings-section settings-section--privacy">
          <h2>Privacy</h2>
          <div className="privacy-callout">
            <ShieldCheck size={22} />
            <div>
              <strong>Designed for offline use</strong>
              <p>
                Recordings, transcripts, and voice profiles remain on this
                device. Processing never needs a listening network port.
              </p>
            </div>
          </div>
          <p className="bitlocker-note">
            <Info size={17} /> For full-library encryption, enable BitLocker on
            this Windows drive.
          </p>
        </section>
      </div>
    </main>
  );
}
