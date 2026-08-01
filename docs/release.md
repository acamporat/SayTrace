# Windows release packaging

SayTrace ships as one self-extracting Windows setup executable. It
contains:

- the normal Tauri per-user NSIS installer;
- the locked Python worker;
- NVIDIA CUDA 12.8 libraries with CPU fallback; and
- LGPL-compatible FFmpeg and FFprobe.

Users do not download or select a separate runtime pack. First-run setup opens
directly at the explicit model-download step. Model bytes remain separate because
Community-1 requires the user to accept its terms and provide a Hugging Face
token. Setup downloads only the revision-pinned files in
`worker/model-manifest.json`, verifies size and SHA-256, and discards the token.

## Install locations

The setup verifies its appended 7z archive by SHA-256, extracts it to a private
temporary directory, and opens the Tauri installer. The checked-in Tauri
configuration uses NSIS `currentUser` mode, so installation does not require
elevation. Its post-install hook verifies every runtime file against
`runtime-manifest.json`, stages the copy, and atomically replaces the installed
runtime:

```text
<SayTrace installation>\runtime\
  local-transcript-worker.exe
  ffmpeg.exe
  ffprobe.exe
  runtime-manifest.json
  ...worker and CUDA libraries
```

Rust resolves that installer-owned directory through Tauri's `resource_dir()`.
The runtime is not copied into the user's data library.

Media, transcripts, models, profiles, jobs, and settings use Tauri's
non-roaming `app_local_data_dir()`:

```text
%LOCALAPPDATA%\com.localtranscript.desktop\
```

Reinstalling or updating the application replaces its bundled runtime while
preserving that data directory. The app and worker process guard prevents
install, update, or uninstall while SayTrace is running.

## Release inputs

Release engineering requires:

- Node.js, Rust, NSIS, and the normal Tauri Windows prerequisites;
- the pinned x64 `7za.exe` and matching 7-Zip license text used by the
  self-extracting setup;
- the locked PyInstaller `onedir` worker produced by
  `worker/scripts/build.ps1`;
- an x64 LGPL-compatible FFmpeg distribution; and
- a real HTTPS supplier build/source page for that FFmpeg distribution.

The NVIDIA worker is locked to PyTorch's official CUDA 12.8 wheels on Windows.
Pass the same FFmpeg 7.x directory used for the release to the worker build so
TorchCodec discovery and collection are tested against the shipped libraries:

```powershell
.\worker\scripts\build.ps1 `
  -OutputDirectory C:\release-inputs\worker-nvidia `
  -FfmpegDirectory C:\release-inputs\ffmpeg-lgpl-7
```

The runtime staging step:

- rejects a mislabeled NVIDIA payload without CUDA DLLs;
- requires the worker, FFmpeg, and FFprobe entrypoints;
- rejects FFmpeg configurations containing `--enable-gpl` or
  `--enable-nonfree`;
- requires FFmpeg to identify an LGPL license;
- records supplier provenance and license notices;
- records worker protocol, pipeline, and model revisions; and
- hashes every payload file.

Create a validated runtime resource directory without creating a second
end-user installer:

```powershell
.\scripts\release-runtime.ps1 `
  -Variant Nvidia `
  -WorkerBundle C:\release-inputs\worker-nvidia\local-transcript-worker `
  -FfmpegDirectory C:\release-inputs\ffmpeg-lgpl-7 `
  -FfmpegSourceUrl https://github.com/BtbN/FFmpeg-Builds/releases/tag/autobuild-2026-07-30-13-32 `
  -PayloadOnly `
  -PayloadOutputDirectory C:\release-inputs\local-transcript-runtime `
  -OutputDirectory C:\release-inputs\runtime-manifest `
  -AllowDirty
```

Build the combined unsigned development installer:

```powershell
.\scripts\release-app.ps1 `
  -RuntimePayloadDirectory C:\release-inputs\local-transcript-runtime `
  -SevenZipPath C:\release-inputs\7zip\x64\7za.exe `
  -SevenZipLicensePath C:\release-inputs\7zip\License.txt `
  -OutputDirectory C:\artifacts\local-transcript `
  -AllowDirty
```

Unsigned artifacts are explicitly labeled `UNSIGNED`. Release scripts refuse to
overwrite existing artifacts and refuse dirty production signing inputs.

## Signing

Signing uses a certificate with the Code Signing EKU and an accessible private
key in `Cert:\CurrentUser\My`. PFX passwords are never accepted on the command
line or written to generated configuration.

Set:

```powershell
$env:LOCAL_TRANSCRIPT_SIGNTOOL_PATH = 'C:\Program Files (x86)\Windows Kits\10\bin\<sdk>\x64\signtool.exe'
$env:LOCAL_TRANSCRIPT_SIGN_CERT_THUMBPRINT = '<40-hex-character-thumbprint>'
$env:LOCAL_TRANSCRIPT_TIMESTAMP_URL = 'http://timestamp.digicert.com'
```

Stage a signed payload first:

```powershell
.\scripts\release-runtime.ps1 `
  -Variant Nvidia `
  -WorkerBundle C:\release-inputs\worker-nvidia\local-transcript-worker `
  -FfmpegDirectory C:\release-inputs\ffmpeg-lgpl-7 `
  -FfmpegSourceUrl https://github.com/BtbN/FFmpeg-Builds/releases/tag/autobuild-2026-07-30-13-32 `
  -PayloadOnly `
  -PayloadOutputDirectory C:\release-inputs\local-transcript-runtime `
  -OutputDirectory C:\release-inputs\runtime-manifest `
  -Sign
```

Then sign the combined application installer:

```powershell
.\scripts\release-app.ps1 `
  -RuntimePayloadDirectory C:\release-inputs\local-transcript-runtime `
  -SevenZipPath C:\release-inputs\7zip\x64\7za.exe `
  -SevenZipLicensePath C:\release-inputs\7zip\License.txt `
  -Sign
```

The app release refuses signing when the runtime manifest does not declare signed
worker, FFmpeg, and FFprobe entrypoints. Tauri signs the application binaries and
inner NSIS installer; the release script signs the final self-extracting setup
after its archive is appended. The bootstrap verifies the archive SHA-256, 7-Zip
verifies extraction integrity, and the NSIS hook re-verifies the runtime manifest
before installation.

## Verification

Validate checked-in source and packaging contracts:

```powershell
.\scripts\release-verify.ps1
```

Validate the staged runtime payload:

```powershell
.\scripts\release-verify.ps1 `
  -ManifestPath C:\release-inputs\runtime-manifest\Local-Transcript-Runtime-Nvidia-0.1.0-windows-x64-UNSIGNED.payload-manifest.json `
  -PayloadRoot C:\release-inputs\local-transcript-runtime
```

Exercise the packaged worker handshake and both CUDA backends:

```powershell
.\scripts\verify-worker-runtime.ps1 `
  -WorkerExecutable C:\release-inputs\local-transcript-runtime\local-transcript-worker.exe `
  -FfmpegExecutable C:\release-inputs\local-transcript-runtime\ffmpeg.exe `
  -RequireNvidia
```

Validate the one-file setup release manifest:

```powershell
.\scripts\release-verify.ps1 `
  -ManifestPath C:\artifacts\local-transcript\Local-Transcript-0.1.0-Nvidia-windows-x64-UNSIGNED.release-manifest.json
```

Before publishing, test on a clean Windows 11 x64 non-admin account with no
system Python, CUDA toolkit, FFmpeg, Node.js, or Rust. Verify:

- installation and first launch;
- bundled-runtime detection without a separate download;
- packaged worker handshake and GPU health;
- CPU fallback;
- model setup and token disposal;
- final processing with outbound networking blocked;
- application repair, update, rollback, and uninstall; and
- preservation of the user's media and transcript library.

The setup accepts `/S` and forwards it to the inner NSIS installer for automated
clean-machine validation. Interactive users double-click the same single EXE;
they never locate or select a runtime payload.

The worker handshake and GPU health smoke prove that the packaged dependencies
load. They do not replace real model inference, long-recording capture soak,
speaker-calibration benchmarks, or clean-machine lifecycle validation.
