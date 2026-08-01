# SayTrace worker

This directory is the Python 3.13 ML sidecar. It communicates only over inherited
stdin/stdout pipes; stdout is reserved for framed protocol messages and logs go to
stderr. The core package has no runtime dependencies, so setup, protocol, and health
paths can be tested without downloading models.

## Runtime boundary

Normal inference must be launched without `--allow-model-downloads`:

```powershell
local-transcript-worker.exe `
  --model-root C:\path\to\models `
  --allowed-root C:\path\to\library `
  --ffmpeg C:\path\to\ffmpeg.exe
```

This mode sets the Hugging Face/Transformers offline flags, disables telemetry, and
blocks DNS and IP socket connections in-process. Model setup is a separate, explicit
launch with `--allow-model-downloads`. `model.install` accepts a Hugging Face token in
that process only, downloads the exact revision and allow-listed files to a staging
directory, verifies every file with SHA-256, atomically publishes it, and does not
persist the token.

The manifest is `model-manifest.json`. Every model is pinned by a 40-character
repository revision and every required byte is pinned by size and SHA-256.

## Framing

Each frame starts with the 16-byte big-endian header `>4sBBHQ`:

- magic `LTW1`
- major version `1`
- kind `1` for JSON or `2` for audio
- 16-bit flags
- 64-bit payload length

Audio payloads begin with a big-endian 32-bit metadata JSON length, followed by that
JSON and signed 16-bit little-endian PCM. Control frames are limited to 4 MiB and
audio frames to 64 MiB. Every worker output contains a monotonic `sequence`.

JSON requests use:

```json
{
  "protocol_version": "1.0",
  "type": "request",
  "request_id": "request-1",
  "command": "ping",
  "payload": {}
}
```

Commands are `ping`, `health`, `model.status`, `model.verify`, `model.install`,
`live.start`, `live.stop`, `pipeline.run`, `pipeline.cancel`, and `shutdown`.

`live.start` payload:

```json
{
  "session_id": "session-1",
  "streams": {
    "mic": "microphone",
    "desktop": "loopback"
  }
}
```

The host then sends audio frames carrying `session_id`, `stream_id`, `sequence`,
`start_ms`, `sample_rate`, `channels`, and `sample_format: "s16le"`. Live output events
are `draft_revision`, `device_warning`, and `live_error`. A draft revision includes
the full committed text, replaceable unstable suffix, and `replace_from_token`.

`pipeline.run` additionally requires top-level `job_id` and the pipeline version from
the startup `hello` event. Its payload contains an existing approved workspace,
approved source paths, a `diarization_asset_id`, optional ephemeral voice profiles,
an optional calibrated matching policy, and an optional `resume` checkpoint:

```json
{
  "protocol_version": "1.0",
  "type": "request",
  "request_id": "request-2",
  "command": "pipeline.run",
  "job_id": "job-1",
  "pipeline_version": "2026.07.28.1",
  "payload": {
    "workspace_path": "C:\\library\\meetings\\job-1",
    "sources": [
      {
        "asset_id": "mic",
        "path": "C:\\library\\meetings\\job-1\\mic.wav",
        "source_type": "microphone",
        "isolated_speaker": "You"
      },
      {
        "asset_id": "desktop",
        "path": "C:\\library\\meetings\\job-1\\desktop.wav",
        "source_type": "loopback"
      }
    ],
    "diarization_asset_id": "desktop",
    "profiles": [],
    "resume": {}
  }
}
```

The command response is only an acceptance acknowledgement. Results arrive as
`job_started`, `job_progress`, bounded `pipeline_batch` events, and finally
`job_complete` or `job_error`. Checkpoints reference fsynced, atomically published
JSON artifacts inside that job's workspace. The canonical final artifact contains
immutable model text; Rust remains the sole SQLite writer and stores user edits
separately. Both index and final artifacts include a compact `speaker_matches`
mapping keyed by cluster with profile ID, display name, categorical state,
similarity, runner-up margin, and calibration ID—never voice embeddings. Turn UUIDs
are deterministic within a job and namespaced by `job_id`, preventing cross-meeting
collisions for identical timestamps.

Only the private terminal `job_complete.payload.result` may include transient
`speaker_candidates` for explicit profile confirmation. Each candidate contains
`cluster_label`, positive `clean_duration_ms`, and `embedding_base64`: a normalized
vector encoded as standard Base64 over little-endian float32 bytes. Candidates are
bounded and omitted when no clean embedding is available. Embeddings never appear in
progress/batch events, checkpoints, index artifacts, or the canonical final artifact;
the Rust host validates, DPAPI-encrypts, and persists any explicitly confirmed vector.

ASR commits deterministic 10-minute core batches with 5 seconds of audio overlap,
so restart continues at the next incomplete batch with context. GPU OOM recovery
releases model state and retries CUDA FP16 at batch sizes 8 then 2, CUDA
`int8_float16`, and finally CPU int8. CTranslate2 and PyTorch select CUDA
independently, allowing diarization to fall back to CPU when its CUDA backend
cannot initialize. Both backends ship inside the normal SayTrace installer.

## Development

```powershell
uv run --frozen pytest tests
uv run --frozen ruff check src tests
uv run --frozen mypy src
.\scripts\build.ps1
```

Unit tests use fake ASR/alignment/diarization/embedding implementations and never
download model data.
