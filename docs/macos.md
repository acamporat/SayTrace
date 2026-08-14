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
- Node.js 22 and npm 10.9.8 (pinned in `package.json`)
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

## macOS release workflow

The release flow has three distinct states: a local candidate, an immutable
notarized installer awaiting physical acceptance, and a finalized release. A
candidate or prepared installer is not an official release. Only the output of
the finalization step under `artifacts/macos/v<version>` is publishable.

### Official-release prerequisites

In addition to the development prerequisites above, official preparation and
finalization require:

- a clean Git worktree at the exact release commit;
- a private, release-operator-owned checkout whose repository and output
  directories are not group- or world-writable, with no other build or file
  mutation process running in that checkout;
- a current Apple Developer Program membership;
- a valid **Developer ID Application** certificate and its private key in the
  signing Mac's Keychain;
- a validated `notarytool` Keychain profile;
- enough free space for the build script to create the locked, macOS-15-targeted
  `worker/.venv-macos` release environment;
  and
- all revision-pinned models in the normal SayTrace model directory, or in the
  directory named by `SAYTRACE_RELEASE_MODEL_ROOT`, so packaged model inference
  can be tested before notarization.

Create an interactive Keychain profile if needed:

```bash
xcrun notarytool store-credentials saytrace-notary
```

The candidate and official build commands create `worker/.venv-macos`
automatically with uv-managed Python 3.13.12 and macOS-15 MLX wheels. This is
separate from the host-oriented `worker/.venv` used for development, preventing
newer Homebrew or MLX binaries from silently raising the installer's minimum
system version. Release verification inspects every embedded Mach-O deployment
target and rejects anything newer than macOS 15.

The release script validates that trust boundary, rejects symlinked output
components, and holds an exclusive checkout-local release lock. These checks
assume another process running as the same macOS user is not deliberately racing
path replacement; use a dedicated local checkout rather than a shared CI worktree.

Keep Apple credentials in Keychain; do not store them in the repository or a
shell script. `SAYTRACE_NOTARY_PROFILE` contains only the Keychain profile name.
The build selects the installed Developer ID Application identity automatically,
or an exact certificate can be selected with `SAYTRACE_CODESIGN_IDENTITY`.

Model files are a release-time inference-test input, not a bundled payload.
The DMG contains the self-contained worker and media runtime but no model
weights. Each installation downloads only the revision-pinned, hash-verified
weights during explicit first-run setup.

### 1. Build a local candidate

Use candidate mode to exercise packaging before consuming notarization
submissions:

```bash
./script/build_macos_release.sh --candidate
```

If the exact FFmpeg and worker outputs have already been built and verified,
they can be reused for a faster local iteration:

```bash
./script/build_macos_release.sh \
  --candidate \
  --skip-ffmpeg-build \
  --skip-worker-build
```

Candidate mode requires an Apple Development identity. It builds an arm64 app,
verifies the embedded runtime, generates the legal-notice payload, signs the app
and DMG for local testing, and writes:

```text
artifacts/macos/candidates/v0.3.0/
  SayTrace-0.3.0-macos-arm64-UNNOTARIZED.dmg
  SayTrace-0.3.0-macos-arm64.release-manifest.json
  SayTrace-0.3.0-ffmpeg-8.1.2-source.tar.xz
  SayTrace-0.3.0-ffmpeg-8.1.2-source.tar.xz.asc
  RELEASE_NOTES.md
  SHA256SUMS.txt
```

The `UNNOTARIZED` candidate is for local integration testing only. Do not call
it official, distribute it as a stable release, tag it, or upload it to a GitHub
Release.

### 2. Prepare the exact notarized installer

From the clean release commit, run official mode without either skip flag:

```bash
export SAYTRACE_NOTARY_PROFILE=saytrace-notary
./script/build_macos_release.sh
```

Set `SAYTRACE_RELEASE_MODEL_ROOT` first if the verified models are not in the
default SayTrace application-support directory. Official mode rebuilds and
verifies the redistributable FFmpeg and worker, requires a successful packaged
model-inference gate, signs all nested code and the outer app with Developer ID,
notarizes and staples both the app and DMG, and runs Gatekeeper assessment.

The same command generates the embedded legal-notice payload from the locked
Node, Rust, and Python dependency graphs plus the exact redistributable FFmpeg
build. It records sanitized FFmpeg source provenance and includes the available
license and notice files for every resolved dependency. A Python package without
distributed legal text stops the build unless its exact package/version has a
reviewed, hash-pinned entry in
`third_party/python-license-overrides.json`. Review the generated inventory in
`build/release-resources/legal` before publication.

The immutable preparation output is:

```text
artifacts/macos/prepared/v0.3.0/
  SayTrace-0.3.0-macos-arm64.dmg
  SayTrace-0.3.0-ffmpeg-8.1.2-source.tar.xz
  SayTrace-0.3.0-ffmpeg-8.1.2-source.tar.xz.asc
  RELEASE_NOTES.md
  prepared-release.json
  physical-acceptance-template.json
  .release-state/
```

The script refuses to overwrite an existing prepared directory. Preserve every
file exactly as generated. `prepared-release.json` records the installer hash,
source revision, runtime-manifest hash, and Apple submission IDs, but marks the
distribution as not official because physical acceptance has not happened.
This directory is not publishable.

### 3. Accept that exact DMG on a physical Mac

Copy the prepared DMG and its generated
`physical-acceptance-template.json` to a clean Apple Silicon test Mac without
renaming or modifying the DMG. Use the generated template, not the generic
example under `docs/releases`, because it is prefilled with the exact DMG name,
byte size, SHA-256 digest, version, and source revision.

On the test Mac, independently verify the installer and record the hardware:

```bash
shasum -a 256 SayTrace-0.3.0-macos-arm64.dmg
sysctl -n hw.model
sw_vers -productVersion
```

The computed digest must exactly match the template. Install from that DMG and
perform every listed check, including clean first run, repeated microphone-only,
system-audio-only, and combined capture, permission grant/denial/reset/revocation,
WAV channel and finalization checks, the combined-capture soak and A/V clock
check, and the diagnostic-crash-report check. In a copied acceptance JSON:

- replace the UTC timestamp and hardware placeholders;
- change every check, including
  `installer_sha256_verified_on_test_mac`, from `not_run` to `passed`; and
- set `operator_confirmation` to `true`.

Acceptance must describe Apple Silicon on macOS 15 or newer and be no more than
30 days old when finalized. Testing a rebuilt, renamed, or otherwise different
DMG does not qualify the prepared installer.

### 4. Finalize the accepted bytes

Return the completed acceptance JSON to the release Mac. Keep the prepared
directory unchanged, check out the same clean source commit, and run:

```bash
export SAYTRACE_NOTARY_PROFILE=saytrace-notary
export SAYTRACE_MACOS_ACCEPTANCE_EVIDENCE=/absolute/path/to/completed-acceptance.json
./script/finalize_macos_release.sh
```

Finalization fails closed unless the acceptance record matches the prepared DMG
by name, size, and SHA-256 digest. It also rechecks the prepared inventory and
Apple submission status, verifies the signed FFmpeg source inputs, mounts the
exact DMG read-only, and revalidates Developer ID signatures, stapling,
Gatekeeper acceptance, bundle identity, architecture, and the embedded runtime.
It then writes the release atomically to:

```text
artifacts/macos/v0.3.0/
  SayTrace-0.3.0-macos-arm64.dmg
  SayTrace-0.3.0-macos-arm64.release-manifest.json
  SayTrace-0.3.0-ffmpeg-8.1.2-source.tar.xz
  SayTrace-0.3.0-ffmpeg-8.1.2-source.tar.xz.asc
  SayTrace-0.3.0-macos-physical-acceptance.json
  RELEASE_NOTES.md
  SHA256SUMS.txt
```

Only this finalized seven-file directory is eligible for a stable tag or public
release. Do not publish files from `candidates`, `prepared`, `.release-state`,
`build`, or the Tauri bundle directory. Finalization does not create a Git tag
or GitHub Release; those remain separate, explicit publication actions after
the finalized checksums and manifest have been reviewed.

### Current distribution blocker

The current release Mac has an Apple Development identity but no Developer ID
Application identity or configured `notarytool` profile, so it can produce a
candidate but cannot complete official preparation. An Apple
Development-signed candidate does not satisfy the official flow. Even after
those Apple credentials are available and notarization succeeds, the release
must remain unpublished until the exact prepared DMG passes the physical Mac
checks and `finalize_macos_release.sh` creates the final directory.
