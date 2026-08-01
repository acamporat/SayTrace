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

Runtime files live below Tauri's non-roaming local app-data directory
(`%LOCALAPPDATA%\com.localtranscript.desktop` on Windows; the legacy internal
identifier is retained for migration compatibility):

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

1. Dedicated MMCSS-priority WASAPI threads capture the microphone and selected render-endpoint loopback.
2. A loss-intolerant writer queue persists separate recoverable PCM segments and
   appends each checkpoint to a flushed manifest journal.
3. A separate bounded, droppable queue resamples copies for live draft inference.
4. Meter and caption events—not raw PCM—cross the WebView boundary.
5. Stop closes the authoritative sources before any final processing begins.
6. The final pipeline rereads the sources and replaces the disposable draft.

Capture never waits for inference. If the worker stalls, draft chunks may be coalesced or dropped while recording continues.

## Durable jobs

Jobs move through:

```text
queued → running → completed
             ├─→ retry_wait → queued
             ├─→ cancel_requested → cancelled
             ├─→ interrupted → queued
             └─→ failed
```

Steps are idempotent and write validated `.partial` artifacts before atomic rename and database commit. Only one GPU-heavy step runs at once. Expired leases are requeued after an unclean shutdown.

## Worker protocol

- The worker performs a versioned `hello` handshake before accepting work.
- Rust-to-worker frames contain a length, message kind, and JSON or PCM payload.
- Worker stdout is structured protocol output only; stderr is structured logging.
- Every command request includes a `request_id`; durable pipeline requests also
  include `job_id` and `pipeline_version`, while streamed audio and events use
  monotonic sequence numbers where ordering matters.
- Messages and live queues are bounded, heartbeats are supervised, and no network port is opened.
- Protocol and pipeline compatibility are checked during the worker `hello`
  handshake.
- Model files are checked against their pinned size and SHA-256 after download
  and before use.
- Runtime payload hashes and Authenticode signatures are checked by the release
  tooling. The application does not currently re-hash the installed runtime
  before launch.

## Security

- Managed paths are canonicalized and constrained below approved roots.
- Asset playback resolves opaque database IDs inside those roots and serves at
  most 8 MiB per `206 Partial Content` response, including requests without a
  Range header; invalid or multi-ranges are rejected.
- Voice embeddings are encrypted by Rust with Windows DPAPI CurrentUser scope.
- Hugging Face credentials are used only for explicit model provisioning and are not stored after download.
- Offline worker launches set Hugging Face and pyannote offline/telemetry-disable environment variables.
- Audio and transcripts are local ordinary files; full-library at-rest encryption is delegated to BitLocker.
