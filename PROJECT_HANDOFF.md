# SayTrace project handoff

**Last updated:** 2026-10-22  
**Evidence labels:** **Verified here** means directly inspected in this workspace. **Documented intent** means stated by project documentation but not confirmed in a working build. **Unknown** means unavailable or not verified.

## Purpose and scope

**Verified here:** The README describes SayTrace, a Windows-first desktop app for private local meeting capture, transcription, and conservative speaker identification. It describes separate microphone/system audio tracks, disposable live captions, and a canonical transcript rebuilt from saved media. Optional features include whole-desktop recording and transcript-grounded local Q&A.

**Documented intent:** React/TypeScript UI, Tauri/Rust trusted local storage and recording boundary, and a Python worker for local ML inference. Audio, screen video, transcripts, and models are intended to remain in local app data. Ollama Q&A/vision uses loopback only; no cloud transcription service. Whole-desktop recording requires a separate acknowledgement. See README.md and its linked architecture/privacy/design documents for requirements and rationale.

The README says internal identifiers and data paths retain `local-transcript` for migration compatibility. Preserve that compatibility unless a documented migration decision says otherwise. Licensing is Apache-2.0 for source with attribution requirements in NOTICE; model/runtime assets have separate terms (THIRD_PARTY_NOTICES.md).

## Actual location and repository state (2026-10-02 snapshot; see recovery below)

- **Verified here:** Project root is `F:\PROJECTS\LocalMeetingTranscription` (the active workspace root). `C:\Users\acamp\Documents\LocalMeetingTranscription` does not exist in this environment.
- **Verified here:** Root currently contains `.gitignore`, `index.html`, `package.json`, `package-lock.json`, `README.md`, `LICENSE`, `NOTICE`, `THIRD_PARTY_NOTICES.md`, `tsconfig*.json`, and `vite.config.ts`, plus `.git`. No `src/`, `src-tauri/`, `worker/`, `docs/`, `AGENTS.md`, or existing handoff file was present when inspected.
- **Verified here:** `index.html` points to `/src/main.tsx`, and `vite.config.ts` refers to `src/test/setup.ts`; those targets were not present in the listed workspace. Thus this appears to be an incomplete project tree for the documented app; do not assume it is buildable.
- **Verified here:** `.git/config` names remote `https://github.com/acamporat/SayTrace.git` and has branch configuration for `main` and `codex/readme-screenshots`. `.git/FETCH_HEAD` and `.git/ORIG_HEAD` contain commit `c0fc5fb1aadc2d1e9d0dceb6fc4fc07601adc788`.
- **Blocked/unknown:** Normal `git status`, `git log`, and `git rev-parse` fail with “not a git repository.” The `.git` directory contains oddly suffixed `HEAD (# Name clash 2026-08-13 i7odkdC #)` and `index (# Name clash 2026-08-13 jllc79C #)` instead of the expected exact filenames. Do not infer current branch, clean/dirty state, latest commit, or use Git writes until the repository metadata is repaired/recovered. The commit in FETCH_HEAD is not evidence of the checked-out revision.
- No deployed installer, device state, server state, or release artifact was inspected. **Unknown:** whether any version is deployed or installed elsewhere.

## Architecture and setup (from README; not validated in this tree)

- Windows 11 x64; Node.js 22/npm; Rust 1.88+ with MSVC target; Python 3.13 via `uv`; FFmpeg/FFprobe for development imports.
- UI: React + TypeScript + Vite. Desktop and trusted local boundary: Tauri 2 + Rust. Inference: Python worker. Ask/optional vision: local Ollama at `http://127.0.0.1:11434`; README names pinned `qwen3:4b` as starter model.
- Model revisions/files/hashes are intended to be pinned in `worker/model-manifest.json`. Community-1 requires accepting its terms and a Hugging Face token during setup; README says the token is discarded after setup. Never place credentials in this file or source control.
- README-listed development commands (run only after the missing app/worker tree is recovered):

  ```powershell
  npm ci
  npm run dev
  uv sync --project worker --group dev
  uv run --project worker pytest worker/tests
  uv run --project worker ruff check worker/src worker/tests
  uv run --project worker mypy --config-file worker/pyproject.toml worker/src
  uv sync --project worker --extra ml --group dev
  npm run tauri:dev
  ```

- README verification commands: `npm test`, `npm run build`, `cargo fmt --manifest-path src-tauri/Cargo.toml --check`, `cargo test --manifest-path src-tauri/Cargo.toml`, and `uv run --project worker pytest`. These are documented commands, **not checks performed in this handoff turn**.
- README says release installer bundles the locked Python worker, CUDA runtime with CPU fallback, LGPL-compatible FFmpeg, and WebView2 setup. Model weights remain a first-run download. No packaging or deployment configuration was present to confirm these claims here.
- Credential source: no credential values or credential-bearing URLs recorded. README describes a transient Hugging Face token supplied by the user during Community-1 setup; exact UI/secret-store implementation is unavailable in this tree.

## Work state and next steps

- **Verified here, 2026-10-02:** Inspected root listing, README, package metadata, Vite config, index, ignore rules, and third-party notices. No application build, test, installer, or runtime check was run. Git checks failed due to malformed/missing expected metadata filenames.
- **Not recoverable from this conversation:** No earlier implementation tasks or decisions beyond the user's handoff request are present in this conversation. No access to other conversations is claimed. The README references `docs/architecture.md`, `docs/accuracy-and-privacy.md`, and `docs/design-spec.md`, but those files are absent from this workspace.
- **Prioritized next steps:**
  1. Recover/restore the complete repository tree and valid Git metadata; inspect actual current branch, status, and history before editing application files. Preserve existing local data and do not treat FETCH_HEAD as the worktree revision.
  2. Locate the README-linked architecture, accuracy/privacy, and accepted design documents, then reconcile them with actual source and dated commits.
  3. Once the source and dependencies are present, run the documented checks and record exact results; then verify packaged Windows setup and runtime behavior separately.
  4. Revisit the README claims of default-on screen capture, acknowledgment, local model setup, and compatibility paths against current implementation and accepted design. No pending user approval or unresolved product choice was visible in this chat.

## Optional ChatGPT meeting assistant — 2026-10-13

**Verified here:** Missing `src`, `src-tauri`, `worker`, `docs`, `scripts`, `packaging`, and `assets` directories were restored from the complete `D:\LocalMeetingTranscription` copy. Existing root files and malformed Git metadata were preserved. This restores a buildable source tree; the D: copy's precise commit identity is **Unknown**. Git still reports not a repository; no commit/push is claimed.

**Implemented:** The Ask panel defaults to Local AI and now offers ChatGPT / Codex as an optional provider, browser sign-in/sign-out, a model selector, and explicit per-meeting text-sharing consent. Provider/meeting changes revoke that consent. Backend rejects missing consent before reading meeting context or starting an inference request. Local requests retain Ollama behavior; there is no automatic cloud fallback.

**Implemented:** `codex_agent.rs` drives the installed Codex CLI's stdio app-server. Its persistent process hosts the browser callback, uses SayTrace's separate `%LOCALAPPDATA%\com.localtranscript.desktop\chatgpt-agent` home, strips inherited provider/authentication environment variables, and starts hidden on Windows. It enforces bounded protocol messages/timeouts, disables tools/features, rejects server tool requests, and uses ephemeral read-only threads in an isolated workspace. Tokens stay in Codex-managed local storage, outside the renderer. ChatGPT is required; API-key authentication is not accepted for this provider.

**Privacy:** Cloud questions send the question, selected transcript excerpts (including speaker names), and recent meeting chat history to OpenAI. Audio, video, transcription, and speaker inference stay local. Both providers share grounded prompt/answer formatting and timestamp citation validation; cloud answers are marked `chatgpt:<model>` in existing SQLite history. README, Settings privacy text, and accuracy/privacy documentation describe the optional network boundary. Official integration reference: https://learn.chatgpt.com/docs/app-server.

**Verified here:** Frontend production build passed; UI suite 35 passed. Rust lib suite 131 passed, 2 ignored after supplying the installed FFmpeg directory on development PATH. An explicit isolated Codex smoke passed: initialize/account-read return a signed-out account without reading desktop auth. Playwright with installed Edge (Browser plugin unavailable) verified the preview at `http://127.0.0.1:1420`, 1280x800: correct title/content, provider switching and disclosure, disabled cloud submission/sign-in in browser-only preview, no page errors, and screenshot reviewed. Real ChatGPT authentication/model access/credit use/cloud answer remain **Unknown** until the user signs in and submits a question.

**Verified here before upgrade:** Existing SQLite integrity `ok`; 9 meetings, 850 transcript turns, 2 chat messages, 6 voice profiles. Recovery snapshot: `%LOCALAPPDATA%\SayTraceRecovery\2026-10-13-before-chatgpt` holds the previous executable, SQLite copy, and hardlink snapshot of the installed runtime. No meetings or models were deleted.

**Verified here — package/install:** `npm run tauri:build -- --bundles nsis` produced the unsigned Windows release installer at `F:\PROJECTS\LocalMeetingTranscription\artifacts\app\2026-10-13-chatgpt\SayTrace_0.2.0_x64-setup.exe`; SHA-256 `BDDDC418752674EE7DA768CBCEF961A20F41B109D40092F69E87ACF1F5884F57`. Existing SayTrace was open; installer safety hook initially blocked the upgrade. After confirming no active recording/processing in the database, the app was closed normally and its idle orphaned worker stopped. Retry with `/S` exited 0. Existing installed runtime remained complete; recovery restoration was unnecessary.

**Verified here — installed behavior:** Launched `%LOCALAPPDATA%\SayTrace\local-transcript.exe` and inspected its actual WebView via a temporary localhost debugging port. Opened a ready meeting, selected ChatGPT, verified enabled Continue with ChatGPT / disabled question submission while signed out, no page errors, and live backend status `available=true`, `signedIn=false`. The backend refused a consentless cloud question with `cloud_consent_required`. Sign-in RPC returned a valid HTTPS `auth.openai.com` authorization URL; this test did not open/complete browser login and was canceled with SayTrace's sign-out command. No cloud inference or meeting upload occurred. Closed that test launch and relaunched the installed app normally without debugging arguments.

**Verified here — data preservation:** SQLite integrity remains `ok`: 9 meetings, 2 chat messages, 6 voice profiles. A transcription that was running when the earlier backup was taken completed in the original app before upgrade: it added 304 turns (850 → 1,154), a playback asset, and 12 screen snapshots. Its scratch cleanup accounts for lower library file/byte totals; do not call total file counts unchanged. All 850 earlier turns retain their text/timestamps/edits; all earlier media asset IDs remain, and all 50 earlier media files match their stored size/SHA-256, with none missing or changed. Model directory counts/bytes remain 53 files / 5,044,395,458 bytes. Browser login completion, actual model access, credit accounting, and a successful cloud answer still require the user's sign-in and explicit question.


## Speaker filtering, final checks, and recovery — 2026-10-22

**Implemented:** Clicking a named, review, or unknown speaker card filters visual transcript turns; Show all speakers clears it. Search/replace follow the visible speaker. Action buttons remain independent; meeting changes, removed speakers, and assistant citations clear stale filters.

**Implemented:** Final worker processing reconciles only isolated <=350 ms fragments inside one aligned word with strong same-speaker support on both sides. Brief separate interjections, overlaps, recurrent speakers, and ambiguous boundaries are preserved. Repair/review warnings are saved with canonical artifacts; raw intermediate artifacts remain unchanged. Existing completed meetings were not reprocessed. Real-recording accuracy improvement remains **Unknown**; fixture coverage is not a measured diarization benchmark.

**Implemented:** Settings exposes Save/Restore speaker backup via native JSON file dialogs. Versioned encrypted profile samples, speaker labels/matches, and merge rules are included. Automatic before/after-change snapshots are bounded to 50 files under app-data backups/speakers. Startup and completed processing also snapshot. Restore applies merges immediately to visual transcript attribution, validates encrypted samples/meeting ownership, rejects conflicting IDs, and is transactional. Normal merges now preserve saved voice samples. Original Windows user profile/DPAPI keys and matching meeting records are required; see docs/speaker-recovery.md.

**Verified here:** UI suite 41 passed; Rust lib suite 141 passed / 2 optional live tests ignored; worker suite 51 passed; Ruff passed; mypy passed with the locked ML environment installed. cargo fmt check passed. Production frontend and Windows NSIS builds passed. Installed app filtering for named/unknown speakers and settings backup controls passed via actual WebView CDP, with zero page errors. Screenshots reviewed in local qa/ (private meeting content, not published). Native file-dialog interaction has not been automated; backend restore tests exercise transaction behavior.

**Verified here — recovery:** F:\PROJECTS\SayTraceRecovery\2026-10-22-speakers contains consistent SQLite backup (integrity ok), previous executable, verified 8,136-file runtime copy, hash manifest, original Git metadata, an exported speaker-matches.json, and RESTORE.md rollback instructions. Exported speaker backup hash matched its automatic source. Post-app-install database rows exactly match baseline: 14 meetings, 1,933 turns, 47,606 words, 44 meeting speakers, 10 profiles, 12 samples, 5 merge rules, 108 media assets. Media/model files were not modified.

**Verified here — repository recovery:** Restored HEAD and missing refs directory after preserving malformed .git externally; fetched established acamporat/SayTrace remote and rebuilt the index against origin/main a501530 without changing working files. Existing recovered app functionality (screen/video and optional ChatGPT changes documented above) is preserved together with this update. No repository visibility change.

**Verified here — real backup compatibility:** Restored speaker-matches.json into a disposable copy of the recovery SQLite using the actual Rust restoration implementation. All 10 profiles, 44 speakers, 12 DPAPI-encrypted samples, 5 merge rules, 1,933 transcript speaker assignments, and 47,606 word assignments were unchanged. Original recovery files and live library were not modified. Temporary test removed after passing; no personal database or exported speaker data is committed.

**Verified here — final package/install:** Windows NSIS app installed successfully (exit 0), final installer artifacts/app/2026-10-22-speakers/SayTrace_0.2.0_x64-setup.exe SHA256 009AF06DAE3D8DC50ADE6D987106DFE19C317C61B41B7C44ECDA7978203A9BE8. Rebuilt PyInstaller worker includes local_transcript_worker.speaker_review in its executable archive. Established release-runtime.ps1 produced the NVIDIA payload from commit 238e9f161d200362572d9db89ac2ddd7bcf2b993. Runtime handshake, required ML imports, PyTorch CUDA, and CTranslate2 CUDA passed; all 8,248 payload files (5,356,112,395 bytes) passed size/SHA256 validation. Installed worker SHA256 A102A4E33262F23E51A1F2DF371F2A9707596F05E17A7878097E8CE395CA722D matches the built executable. Installed app restart_worker returned ready. No real meeting inference or diarization-accuracy benchmark was run.

**Verified here — final preservation/release:** After the runtime switch, actual installed WebView filtering for named/unknown speakers, clearing the filter, and Settings controls passed with zero page errors. Database integrity remains ok and all baseline rows listed above remain exactly unchanged. Prior runtime additionally retained at C:\Users\acamp\AppData\Local\SayTrace\runtime-before-speakers-20261022; external full recovery copy remains available. Closed the temporary CDP test launch and reopened the installed app normally. Feature/source recovery commit 238e9f1 pushed to established origin/main; final handoff update follows. This is a locally installed unsigned development update, not a newly published GitHub release.
