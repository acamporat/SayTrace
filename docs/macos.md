# macOS development

The macOS port targets Apple Silicon on macOS 15 or newer. Speech recognition
runs locally through MLX/Metal; diarization and speaker embeddings use PyTorch
MPS when supported and fall back to CPU. NVIDIA CUDA is not used on macOS.

The normal local `.app` is Rust-release-optimized and uses the project's Python
environment and Homebrew FFmpeg. A release-packaging path can stage a
self-contained Python/MLX worker, but signing, notarization, and a
redistributable FFmpeg payload remain release gates.

## Prerequisites

- Apple Silicon Mac with macOS 15+
- Xcode with its command-line tools and macOS SDK selected
- Node.js 22 and npm
- Rust 1.88+ with the `aarch64-apple-darwin` target
- `uv` for the Python 3.13 worker
- FFmpeg and FFprobe

One Homebrew-based setup is:

```bash
xcode-select --install
brew install node@22 rustup uv ffmpeg
export PATH="/opt/homebrew/opt/node@22/bin:/opt/homebrew/opt/rustup/bin:$PATH"
rustup default stable
rustup target add aarch64-apple-darwin
```

If full Xcode is installed but another developer directory is selected, use:

```bash
sudo xcode-select --switch /Applications/Xcode.app/Contents/Developer
```

## Set up the repository

From the repository root:

```bash
npm ci
uv sync --project worker --extra ml --group dev
```

The `ml` extra installs MLX plus the local speaker-processing dependencies. For
protocol and unit-test work without ML inference, omit `--extra ml`.

On first use, SayTrace downloads only the revision-pinned model files listed in
`worker/model-manifest.macos.json`. The diarization model is gated: accept its
terms on Hugging Face and enter a read token in the setup screen. SayTrace uses
that token for setup and does not persist it.

## Build, test, and run

Run the normal checks:

```bash
npm test
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo test --manifest-path src-tauri/Cargo.toml
uv run --project worker pytest
uv run --project worker ruff check worker/src worker/tests
uv run --project worker mypy --config-file worker/pyproject.toml worker/src
```

Build the optimized local app bundle and launch it:

```bash
./script/build_and_run.sh
```

The script stops an existing instance, builds
`src-tauri/target/release/bundle/macos/SayTrace.app` with the explicitly local
`development-runtime` feature, and opens the fresh bundle. Rust code and the
WebView host therefore run with release optimizations while the app can still
use `worker/.venv` and Homebrew media tools. It seals the complete bundle with
the audio-input entitlement and the first available Apple Development
identity. That gives the app a stable code requirement so microphone and
screen-capture permission records survive normal rebuilds. Set
`SAYTRACE_CODESIGN_IDENTITY` to a certificate name or SHA-1 hash to choose a
specific identity.

If no Apple Development identity is installed, the script falls back to an
ad-hoc signature and prints a warning. Ad-hoc designated requirements contain
the binary's changing CDHash, so System Settings may continue to show an older
SayTrace entry as enabled while TCC rejects the newly rebuilt executable.
The Codex **Run** action invokes the same script. Optional modes are:

```bash
./script/build_and_run.sh --verify
./script/build_and_run.sh --dev
./script/build_and_run.sh --debug
./script/build_and_run.sh --logs
./script/build_and_run.sh --telemetry
```

`--dev` builds an unoptimized bundle for short edit/build cycles. `--debug`
builds the same debug bundle and launches its executable under LLDB. The
default, `--performance`, `--verify`, `--logs`, and `--telemetry` paths all use
the optimized build. The `development-runtime` feature is never enabled by the
distribution command below; packaged releases still fail closed unless their
embedded runtime passes validation.

Performance intervals are emitted under the
`com.localtranscript.desktop` subsystem and the `PointsOfInterest` category.
Use the `--telemetry` mode for unified logs or Instruments' Points of Interest
template for stage timings. Events contain only stage names, counts, and
durations—not recording titles, transcript text, audio, or filesystem paths.

The browser-only interface remains available with `npm run dev`, but it does
not exercise the Rust host, audio capture, Keychain, or MLX worker.

## Audio capture lifecycle

On macOS, microphone and system audio are two output handlers on one
ScreenCaptureKit `SCStream`. The recording coordinator owns the stream and is
the only code allowed to start, stop, remove handlers, or release it. Shutdown
first rejects new callbacks, then stops the native stream, drains callbacks,
and finally closes the WAV writers.

This ownership is deliberate. Do not split the two sources into independently
started streams or rely on `SCStream`'s Rust `Drop` implementation to stop an
active or partially started stream. A native start that has not completed by
30 seconds is detached from the UI startup path; the app reports a failed
recording instead of blocking while joining the native call. Finalization also
stops waiting after 15 seconds. Either timeout, or any native stream that must
be quarantined after a failed stop, poisons capture for the rest of the process;
the user must restart SayTrace before retrying.

Before treating capture as release-ready on a physical Mac, validate:

- repeated microphone-only, system-only, and combined start/stop cycles;
- permission grant, denial, reset, and revocation paths;
- non-empty, correctly channeled WAV output from both combined outputs;
- pause/resume and stop with no callback writes after writer finalization;
- a long combined-capture soak with stable memory and A/V clock alignment; and
- no new `local-transcript*.ips` report in `~/Library/Logs/DiagnosticReports`.

## macOS permissions

The first recording triggers macOS consent prompts. Grant SayTrace access in
**System Settings > Privacy & Security** for:

- **Microphone**, for microphone capture;
- **Screen & System Audio Recording**, for system audio capture; and
- requested files or folders when importing or exporting outside the app data
  directory.

Quit and reopen SayTrace after changing a permission. If a development rebuild
leaves a stale consent entry, reset it and let macOS prompt again:

```bash
tccutil reset Microphone com.localtranscript.desktop
tccutil reset ScreenCapture com.localtranscript.desktop
```

Voice-profile secrets are stored through macOS Keychain. Audio, transcripts,
models, and the SQLite library remain ordinary local files, so use FileVault if
whole-library encryption at rest is required.

## Package a release candidate

Build and stage the arm64 PyInstaller worker plus FFmpeg and FFprobe:

```bash
uv sync --project worker --extra ml --group build
./script/build_worker_macos.sh
npm run tauri:build -- --config src-tauri/tauri.macos.release.conf.json
```

The runtime is staged under `build/macos-runtime/runtime` with a SHA-256 payload
manifest, then embedded at `Contents/Resources/runtime` by the release-only
Tauri configuration. Model weights remain a verified first-run download.

The staging script fails closed if it detects GPL/nonfree configuration, a
non-arm64 executable, or libraries outside macOS system paths. Supply
self-contained redistributable LGPL-compatible FFmpeg/FFprobe binaries, review
their license obligations, then sign every nested executable and library with a
Developer ID. Regenerate `runtime-manifest.json` over those final signed bytes,
embed the runtime, sign the outer app, and submit the final app or DMG for
notarization; signing nested code after manifest generation invalidates the
payload hashes.

The complete PyInstaller build, nested-code signing, notarization, and
microphone/system-audio performance and permission acceptance on a clean Mac
remain release gates.
