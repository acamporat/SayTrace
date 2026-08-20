# Architecture

SayTrace is split into three trust boundaries.

```text
React renderer
    │ narrow typed Tauri commands and events
    ▼
Tauri / Rust core ─── SQLite, media library, recording, exports
    │ private framed stdin and structured stdout
    ▼
Python ML worker ─── local models only
```

## Ownership

- React renders the approved interface and owns only ephemeral presentation state. It has no arbitrary shell, SQL, or filesystem authority.
- Rust owns all durable data, path validation, device capture, recording manifests, jobs, exports, backups, and voice-profile encryption.
- Python owns inference only. It does not open SQLite and receives only canonical paths under approved app-data roots.

## Runtime data

Runtime files live below Tauri's local app-data directory
(`%LOCALAPPDATA%\com.localtranscript.desktop` on Windows and the matching
Application Support container on macOS; the legacy internal identifier is
retained for migration compatibility):

```text
local-transcript.sqlite3
library/media/<asset-id>/
library/recordings/<meeting-id>/
library/artifacts/<meeting-id>/<model-output-id>.json
library/work/<job-id>/
exports/
backups/
models/<model-id>/<revision>/
runtime/
cache/
logs/
temp/
```

Rust is SQLite's only writer. The database uses WAL, foreign keys, FTS5, a busy timeout, and the online backup API. Large media and model files remain outside the database.

## Recording data flow

1. Dedicated capture threads use MMCSS-priority WASAPI microphone/render-endpoint
   loopback on Windows or one guarded ScreenCaptureKit microphone/system-audio
   stream on macOS.
2. When separately authorized, Windows uses a thread-owned FFmpeg `gdigrab`
   process while macOS attaches a hardware H.264 recording output to that same
   guarded ScreenCaptureKit owner. macOS records the main display. Both paths
   save low-frame-rate, cursor-inclusive segments; pause finalizes the current
   segment and resume starts another.
3. A loss-intolerant writer queue persists separate recoverable PCM segments and
   appends each checkpoint to a flushed manifest journal.
4. A separate bounded, droppable queue resamples copies for live draft inference.
5. Meter, caption, and screen-state events—not raw media—cross the WebView boundary.
6. Stop closes the authoritative sources before any final processing begins.
7. The final pipeline rereads the audio sources and replaces the disposable draft.
   Screen video is consolidated independently and never enters the audio mix.
8. After the canonical transcript commits, Rust selects bounded transcript-cued
   moments, extracts durable JPEGs, and may ask an explicitly compatible local
   Ollama vision model for active-speaker evidence.

Capture never waits for inference. If the worker stalls, draft chunks may be coalesced or dropped while recording continues.

Screen capture and post-processing are deliberately outside the Python worker's
authority. Rust owns the capture process, timestamps, media paths, SQLite writes,
and Ollama loopback call. The vision result is evidence with model and event
provenance, not a confirmed identity: repeated observations can create a review
suggestion, while user and voice-confirmed assignments take precedence.

## Durable jobs

Jobs move through:

```text
queued → running → completed
             ├─→ retry_wait → queued
             ├─→ cancel_requested → cancelled
             ├─→ interrupted → queued
             └─→ failed
```

Steps are idempotent and write validated `.partial` artifacts before atomic
rename and database commit. Only one accelerator-heavy step runs at once, which
also bounds Apple unified-memory pressure. Expired leases are requeued after an
unclean shutdown. Queue workers sleep on change notifications and retry
deadlines instead of polling SQLite while idle.

## Worker protocol

- The worker performs a versioned `hello` handshake before accepting work.
- Rust-to-worker frames contain a length, message kind, and JSON or PCM payload.
- Worker stdout is structured protocol output only; stderr is structured logging.
- Every command request includes a `request_id`; durable pipeline requests also
  include `job_id` and `pipeline_version`, while streamed audio and events use
  monotonic sequence numbers where ordering matters.
- Messages and live queues are bounded, heartbeats are supervised, and no network port is opened.
- Heavy inference backends are lease-protected in a bounded resident cache.
  macOS prewarms final ASR and diarization beneath recording time when live
  captions are disabled when the unified-memory policy permits both; idle
  entries are evicted after a hardware-adaptive two to ten minutes and can be
  released explicitly. On Macs with 16 GB or less, the policy prewarms only
  final ASR and unloads each heavyweight backend after its stage to limit peak
  unified-memory use.
- Protocol and pipeline compatibility are checked during the worker `hello`
  handshake.
- Model files are checked against their pinned size and SHA-256 after download
  and before use.
- Runtime payload sizes and SHA-256 hashes are validated before the application
  accepts a packaged worker. Platform release tooling also enforces the
  applicable nested-code signing policy.

## Security

- Managed paths are canonicalized and constrained below approved roots.
- Asset playback resolves opaque database IDs inside those roots and serves at
  most 8 MiB per `206 Partial Content` response, including requests without a
  Range header; invalid or multi-ranges are rejected.
- Voice embeddings are encrypted by Rust with Windows DPAPI CurrentUser scope
  or an AES-256 key held in the current user's macOS Keychain.
- Hugging Face credentials are used only for explicit model provisioning and are not stored after download.
- Offline worker launches set Hugging Face and pyannote offline/telemetry-disable environment variables.
- Audio and transcripts are local ordinary files; full-library at-rest
  encryption is delegated to BitLocker on Windows or FileVault on macOS.
