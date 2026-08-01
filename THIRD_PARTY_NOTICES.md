# Third-party notices

SayTrace depends on open-source software and separately downloaded model weights. Release packaging must preserve the license text and attribution for the exact versions in the lockfiles and model manifest.

Key runtime components include:

- Tauri, React, Vite, TypeScript, Rust crates, and Python packages under their respective repository licenses.
- 7-Zip's standalone `7za.exe`, used inside the self-extracting setup and redistributed with its LGPL/BSD license text and a link to [7-zip.org](https://www.7-zip.org/).
- FFmpeg from an LGPL-compatible build, accompanied by its build configuration, source offer, and required notices.
- `faster-whisper-large-v3` and `distil-large-v3.5-ct2` under the licenses declared by their pinned model repositories.
- `facebook/wav2vec2-base-960h` under Apache-2.0.
- `pyannote/speaker-diarization-community-1` and `pyannote/wespeaker-voxceleb-resnet34-LM` under CC-BY-4.0, subject to the model repositories' access conditions.

The authoritative repository/revision/license list is `worker/model-manifest.json`. A production release process must generate a complete notice bundle from the resolved Node, Cargo, Python, FFmpeg, and model locks rather than treating this summary as exhaustive.
