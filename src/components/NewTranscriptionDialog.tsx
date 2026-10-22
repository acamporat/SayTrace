import {
  AlertTriangle,
  CheckCircle2,
  FileVideo2,
  Info,
  Mic2,
  Monitor,
  X,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
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
    captureScreen: boolean,
    autoScreenshots: boolean,
    visualSpeakerAttribution: boolean,
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
  const [captureScreen, setCaptureScreen] = useState(true);
  const [autoScreenshots, setAutoScreenshots] = useState(true);
  const [visualSpeakerAttribution, setVisualSpeakerAttribution] =
    useState(true);
  const [showRecordConfirmation, setShowRecordConfirmation] = useState(false);
  const [screenCaptureAcknowledged, setScreenCaptureAcknowledged] =
    useState(false);
  const confirmationTitleRef = useRef<HTMLHeadingElement>(null);

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

  useEffect(() => {
    if (showRecordConfirmation) {
      confirmationTitleRef.current?.focus();
    }
  }, [showRecordConfirmation]);

  function openRecordConfirmation() {
    setScreenCaptureAcknowledged(false);
    setShowRecordConfirmation(true);
  }

  function startRecording() {
    onRecord(
      microphoneDeviceId,
      outputDeviceId,
      microphoneIsPersonal,
      liveCaptions,
      captureScreen,
      captureScreen && autoScreenshots,
      captureScreen && visualSpeakerAttribution,
    );
  }

  return (
    <div className="dialog-backdrop" role="presentation" onMouseDown={onClose}>
      <section
        className={`new-dialog${
          showRecordConfirmation ? " new-dialog--confirmation" : ""
        }`}
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

        {showRecordConfirmation ? (
          <div className="recording-confirmation">
            <h2
              id="new-dialog-title"
              ref={confirmationTitleRef}
              tabIndex={-1}
            >
              Review before recording
            </h2>
            <p className="recording-confirmation__intro">
              Confirm exactly what SayTrace will save before the meeting starts.
            </p>

            {captureScreen ? (
              <>
                <div className="recording-confirmation__state is-on">
                  <span className="recording-confirmation__icon">
                    <Monitor size={27} strokeWidth={1.8} aria-hidden="true" />
                  </span>
                  <span>
                    <strong>Screen recording is ON</strong>
                    <small>
                      The entire desktop across all connected displays will be
                      recorded and stored locally, including visible
                      notifications and other apps.
                    </small>
                  </span>
                </div>
                <ul
                  className="recording-confirmation__features"
                  aria-label="Screen analysis settings"
                >
                  <li className={autoScreenshots ? "is-on" : "is-off"}>
                    {autoScreenshots ? (
                      <CheckCircle2 size={18} aria-hidden="true" />
                    ) : (
                      <Info size={18} aria-hidden="true" />
                    )}
                    <span>
                      <strong>
                        Inline relevant screenshots are {autoScreenshots ? "ON" : "OFF"}
                      </strong>
                      <small>
                        {autoScreenshots
                          ? "Visual moments connected to the conversation can appear in the transcript."
                          : "No screenshots will be selected for the transcript."}
                      </small>
                    </span>
                  </li>
                  <li
                    className={visualSpeakerAttribution ? "is-on" : "is-off"}
                  >
                    {visualSpeakerAttribution ? (
                      <CheckCircle2 size={18} aria-hidden="true" />
                    ) : (
                      <Info size={18} aria-hidden="true" />
                    )}
                    <span>
                      <strong>
                        Visual speaker suggestions are {visualSpeakerAttribution ? "ON" : "OFF"}
                      </strong>
                      <small>
                        {visualSpeakerAttribution
                          ? "Meeting-app names, avatars, and active-speaker borders can suggest who spoke."
                          : "Meeting-app visual cues will not be used for speaker suggestions."}
                      </small>
                    </span>
                  </li>
                </ul>
                <label className="recording-confirmation__acknowledgement">
                  <input
                    type="checkbox"
                    checked={screenCaptureAcknowledged}
                    onChange={(event) =>
                      setScreenCaptureAcknowledged(event.target.checked)
                    }
                  />
                  <span>
                    I understand that every connected display—not just the
                    meeting window—will be recorded.
                  </span>
                </label>
                <div className="recording-confirmation__actions">
                  <button
                    type="button"
                    className="secondary-button"
                    onClick={() => setShowRecordConfirmation(false)}
                  >
                    Back to settings
                  </button>
                  <button
                    type="button"
                    className="primary-button"
                    disabled={
                      !screenCaptureAcknowledged ||
                      !microphoneDeviceId ||
                      !outputDeviceId
                    }
                    onClick={startRecording}
                  >
                    Start recording with screen
                  </button>
                </div>
              </>
            ) : (
              <>
                <div
                  className="recording-confirmation__state is-off"
                  role="alert"
                >
                  <span className="recording-confirmation__icon">
                    <AlertTriangle
                      size={27}
                      strokeWidth={1.8}
                      aria-hidden="true"
                    />
                  </span>
                  <span>
                    <strong>Screen recording is OFF</strong>
                    <small>
                      No screen video or inline screenshots will exist for this
                      meeting. Visual meeting-app cues cannot be used to suggest
                      speakers.
                    </small>
                  </span>
                </div>
                <button
                  type="button"
                  className="recording-confirmation__enable-screen"
                  onClick={() => {
                    setCaptureScreen(true);
                    setAutoScreenshots(true);
                    setVisualSpeakerAttribution(true);
                    setScreenCaptureAcknowledged(false);
                  }}
                >
                  <Monitor size={20} aria-hidden="true" />
                  Turn on screen recording, screenshots, and visual cues
                </button>
                <p className="recording-confirmation__audio-note">
                  If you intentionally want only microphone and system audio,
                  you can continue without screen context.
                </p>
                <div className="recording-confirmation__actions">
                  <button
                    type="button"
                    className="secondary-button"
                    onClick={() => setShowRecordConfirmation(false)}
                  >
                    Back to settings
                  </button>
                  <button
                    type="button"
                    className="recording-confirmation__audio-only"
                    disabled={!microphoneDeviceId || !outputDeviceId}
                    onClick={startRecording}
                  >
                    Continue with audio only
                  </button>
                </div>
              </>
            )}
            <p className="new-dialog__privacy">
              Your media stays on this device and is processed locally.
            </p>
          </div>
        ) : (
          <>
            <h2 id="new-dialog-title">New transcription</h2>
            <p>
              Upload a recorded audio or video file, or capture a meeting now.
            </p>
            <div className="new-dialog__choices">
              <button type="button" onClick={onImport}>
                <span className="choice-icon">
                  <FileVideo2 size={25} strokeWidth={1.7} />
                </span>
                <span>
                  <strong>Upload a recording</strong>
                  <small>
                    Video: transcript, screenshots, and visual cues. Audio:
                    transcript only.
                  </small>
                </span>
              </button>
              <button
                type="button"
                onClick={openRecordConfirmation}
                disabled={!microphoneDeviceId || !outputDeviceId}
              >
                <span className="choice-icon">
                  <Mic2 size={25} strokeWidth={1.7} />
                </span>
                <span>
                  <strong>Record a meeting</strong>
                  <small>
                    {captureScreen
                      ? "Audio + all connected displays (review before starting)"
                      : "Audio only (screen is currently off)"}
                  </small>
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
                onChange={(event) =>
                  setMicrophoneIsPersonal(event.target.checked)
                }
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
                  Draft text is disposable; the final transcript always starts
                  from saved media.
                </small>
              </span>
            </label>
            <section
              className={`new-dialog__screen-capture${
                captureScreen ? " is-enabled" : ""
              }`}
              aria-labelledby="screen-capture-title"
            >
              <label className="new-dialog__screen-toggle">
                <input
                  type="checkbox"
                  checked={captureScreen}
                  aria-controls="screen-capture-options"
                  aria-expanded={captureScreen}
                  onChange={(event) => setCaptureScreen(event.target.checked)}
                />
                <span className="choice-icon choice-icon--compact">
                  <Monitor size={20} strokeWidth={1.8} />
                </span>
                <span>
                  <strong id="screen-capture-title">
                    Record all connected displays
                  </strong>
                  <small>
                    On by default. Whole-desktop video is stored locally.
                  </small>
                </span>
              </label>
              {captureScreen ? (
                <div
                  id="screen-capture-options"
                  className="screen-capture-options"
                >
                  <div
                    className="screen-capture-disclosure"
                    id="screen-capture-disclosure"
                  >
                    <Info size={18} aria-hidden="true" />
                    <p>
                      Everything visible across all connected displays—including
                      notifications and other apps—will be recorded and stored
                      on this device. You must confirm this again before
                      recording starts.
                    </p>
                  </div>
                  <div className="screen-capture-features">
                    <label>
                      <input
                        type="checkbox"
                        checked={autoScreenshots}
                        onChange={(event) =>
                          setAutoScreenshots(event.target.checked)
                        }
                      />
                      <span>
                        <strong>
                          Add relevant screenshots to the transcript
                        </strong>
                        <small>
                          Capture local visual context when speech refers to
                          shared content.
                        </small>
                      </span>
                    </label>
                    <label>
                      <input
                        type="checkbox"
                        checked={visualSpeakerAttribution}
                        onChange={(event) =>
                          setVisualSpeakerAttribution(event.target.checked)
                        }
                      />
                      <span>
                        <strong>
                          Use meeting-app visual cues to suggest speakers
                        </strong>
                        <small>
                          Suggestions from names, avatars, or active-speaker
                          borders remain in Review until confirmed.
                        </small>
                      </span>
                    </label>
                  </div>
                </div>
              ) : (
                <div
                  id="screen-capture-options"
                  className="screen-capture-disabled-note"
                  role="note"
                >
                  Screen video, inline screenshots, and visual speaker cues will
                  be unavailable. SayTrace will warn you again before starting.
                </div>
              )}
            </section>
            <p className="new-dialog__privacy">
              Your media stays on this device and is processed locally.
            </p>
          </>
        )}
      </section>
    </div>
  );
}
