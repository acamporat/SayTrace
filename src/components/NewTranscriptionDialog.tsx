import { FileAudio2, Mic2, X } from "lucide-react";
import { useEffect, useState } from "react";
import type { AudioDevice } from "../types";

interface NewTranscriptionDialogProps {
  devices: AudioDevice[];
  onClose: () => void;
  onImport: () => void;
  onRecord: (
    microphoneDeviceId: string,
    outputDeviceId: string,
    microphoneIsPersonal: boolean,
    liveCaptions: boolean,
  ) => void;
}

export function NewTranscriptionDialog({
  devices,
  onClose,
  onImport,
  onRecord,
}: NewTranscriptionDialogProps) {
  const microphones = devices.filter((device) => device.kind === "input");
  const outputs = devices.filter((device) => device.kind === "output");
  const [microphoneDeviceId, setMicrophoneDeviceId] = useState(
    microphones.find((device) => device.isDefault)?.id ??
      microphones[0]?.id ??
      "",
  );
  const [outputDeviceId, setOutputDeviceId] = useState(
    outputs.find((device) => device.isDefault)?.id ?? outputs[0]?.id ?? "",
  );
  const [microphoneIsPersonal, setMicrophoneIsPersonal] = useState(true);
  const [liveCaptions, setLiveCaptions] = useState(true);

  useEffect(() => {
    if (!microphones.some((device) => device.id === microphoneDeviceId)) {
      setMicrophoneDeviceId(
        microphones.find((device) => device.isDefault)?.id ??
          microphones[0]?.id ??
          "",
      );
    }
    if (!outputs.some((device) => device.id === outputDeviceId)) {
      setOutputDeviceId(
        outputs.find((device) => device.isDefault)?.id ?? outputs[0]?.id ?? "",
      );
    }
  }, [devices, microphoneDeviceId, outputDeviceId]);

  return (
    <div className="dialog-backdrop" role="presentation" onMouseDown={onClose}>
      <section
        className="new-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="new-dialog-title"
        onMouseDown={(event) => event.stopPropagation()}
      >
        <button
          className="icon-button new-dialog__close"
          type="button"
          aria-label="Close"
          onClick={onClose}
        >
          <X size={19} />
        </button>
        <h2 id="new-dialog-title">New transcription</h2>
        <p>Choose an audio or video file, or capture a meeting now.</p>
        <div className="new-dialog__choices">
          <button type="button" onClick={onImport}>
            <span className="choice-icon">
              <FileAudio2 size={25} strokeWidth={1.7} />
            </span>
            <span>
              <strong>Import media</strong>
              <small>Audio and video files</small>
            </span>
          </button>
          <button
            type="button"
            onClick={() =>
              onRecord(
                microphoneDeviceId,
                outputDeviceId,
                microphoneIsPersonal,
                liveCaptions,
              )
            }
            disabled={!microphoneDeviceId || !outputDeviceId}
          >
            <span className="choice-icon">
              <Mic2 size={25} strokeWidth={1.7} />
            </span>
            <span>
              <strong>Record a meeting</strong>
              <small>Microphone and system audio</small>
            </span>
          </button>
        </div>
        <div className="new-dialog__devices">
          <label>
            Microphone
            <select
              aria-label="Microphone input device"
              value={microphoneDeviceId}
              onChange={(event) => setMicrophoneDeviceId(event.target.value)}
            >
              {microphones.map((device) => (
                <option key={device.id} value={device.id}>
                  {device.name}
                  {device.isDefault ? " (Default)" : ""}
                </option>
              ))}
            </select>
          </label>
          <label>
            System audio output
            <select
              aria-label="System audio output device"
              value={outputDeviceId}
              onChange={(event) => setOutputDeviceId(event.target.value)}
            >
              {outputs.map((device) => (
                <option key={device.id} value={device.id}>
                  {device.name}
                  {device.isDefault ? " (Default)" : ""}
                </option>
              ))}
            </select>
          </label>
        </div>
        <label className="new-dialog__personal-mic">
          <input
            type="checkbox"
            checked={microphoneIsPersonal}
            onChange={(event) => setMicrophoneIsPersonal(event.target.checked)}
          />
          <span>
            <strong>This microphone is only me</strong>
            <small>
              Turn this off for a room microphone so local speakers are
              separated.
            </small>
          </span>
        </label>
        <label className="new-dialog__personal-mic">
          <input
            type="checkbox"
            checked={liveCaptions}
            onChange={(event) => setLiveCaptions(event.target.checked)}
          />
          <span>
            <strong>Show live draft captions</strong>
            <small>
              Draft text is disposable; the final transcript always starts from
              saved media.
            </small>
          </span>
        </label>
        <p className="new-dialog__privacy">
          Your media stays on this device and is processed locally.
        </p>
      </section>
    </div>
  );
}
