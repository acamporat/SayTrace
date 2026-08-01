# SayTrace

SayTrace is a Windows-first desktop application for private meeting capture, audio/video transcription, and conservative speaker identification. Microphone and system audio are recorded locally as separate sources, live captions are treated as disposable drafts, and the canonical transcript is rebuilt from the saved media after recording stops.

The application does not use a cloud transcription service. It enables network
access only during explicit model setup for the revision-pinned files declared by
the installed release. Local inference and model-status refreshes do not contact
an update or model feed.

## Architecture

- **React + TypeScript:** the desktop interface and ephemeral presentation state.
- **Tauri + Rust:** the trusted filesystem, SQLite, import/export, recording, encryption, and job boundary.
- **Python worker:** local live and final ML inference over private inherited pipes.

See [Architecture](docs/architecture.md), [Accuracy and privacy](docs/accuracy-and-privacy.md), and the [accepted design specification](docs/design-spec.md).

## Development prerequisites

- Windows 11 x64
- Node.js 22 and npm
- Rust 1.88+ with the MSVC Windows target
- Python 3.13 managed through [uv](https://docs.astral.sh/uv/)
- FFmpeg and FFprobe for development imports

The normal production installer includes the locked Python worker, NVIDIA CUDA
runtime with CPU fallback, and LGPL-compatible FFmpeg. End users do not install
Python, FFmpeg, a CUDA toolkit, or a separate SayTrace runtime pack.
Model files remain an explicit one-time first-run download because Community-1
requires the user to accept its terms.

## Start the interface

```powershell
npm ci
npm run dev
```

The browser preview uses the approved demonstration meeting so visual and interaction tests are deterministic. Packaged Tauri builds do not load demonstration meetings.

To run the desktop shell:

```powershell
npm run tauri:dev
```

## Worker development

The default worker environment contains only protocol and test dependencies; it does not download multi-gigabyte models.

```powershell
uv sync --project worker --group dev
uv run --project worker pytest worker/tests
uv run --project worker ruff check worker/src worker/tests
uv run --project worker mypy --config-file worker/pyproject.toml worker/src
```

Install the full local inference dependencies only on a supported ML build machine:

```powershell
uv sync --project worker --extra ml --group build
```

Model repositories, immutable revisions, required files, and file hashes are declared in `worker/model-manifest.json`. Community-1 is gated: first-run setup requires the user to accept its terms and provide a Hugging Face token. The token is discarded after setup.

## Verification

```powershell
npm test
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo test --manifest-path src-tauri/Cargo.toml
uv run --project worker pytest
```

The release gate also requires:

- a two-hour microphone/loopback synchronization soak;
- device removal, pause/resume, disk-full, and forced-termination recovery;
- blocked-network final transcription;
- calibrated speaker-name false-accept testing;
- clean Windows VM installation without system Python or CUDA;
- screenshot comparison against both checked-in concepts.

## Privacy boundary

- Audio, video, transcripts, and model files stay in the local application-data library.
- Voice embeddings are protected with Windows DPAPI for the current user.
- Audio and transcript files are not separately encrypted by the application; use BitLocker for whole-library at-rest encryption.
- The worker runs in explicit offline mode after model setup and has no listening network port.
- Weak or ambiguous speaker matches remain `Unknown`.

## License and attribution

SayTrace source code is licensed under the [Apache License 2.0](LICENSE).
You may use, modify, and redistribute it, including commercially, provided you
follow the license terms. Redistributions and derivative works must preserve
the attribution notices required by the license and the included [NOTICE](NOTICE)
crediting `acamporat`.

Model weights, FFmpeg, and third-party dependencies remain under their own
licenses and terms; see [Third-party notices](THIRD_PARTY_NOTICES.md). The
`SayTrace` name and branding are not granted for unrestricted trademark use by
the source-code license.

The internal application identifier, worker executable name, and data paths
retain their original `local-transcript` values for migration compatibility
with existing installations. They are implementation identifiers, not the
public product name.
