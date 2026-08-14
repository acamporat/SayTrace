# Third-party notices

SayTrace depends on open-source software and separately downloaded model weights. Release packaging must preserve the license text and attribution for the exact versions in the lockfiles and model manifest.

Key runtime components include:

- Tauri, React, Vite, TypeScript, Rust crates, and Python packages under their respective repository licenses.
- 7-Zip's standalone `7za.exe`, used inside the self-extracting setup and redistributed with its LGPL/BSD license text and a link to [7-zip.org](https://www.7-zip.org/).
- FFmpeg from an LGPL-compatible build, accompanied by its build configuration, source offer, and required notices.
- `faster-whisper-large-v3` and `distil-large-v3.5-ct2` under the licenses declared by their pinned model repositories.
- `facebook/wav2vec2-base-960h` under Apache-2.0.
- `pyannote/speaker-diarization-community-1` and `pyannote/wespeaker-voxceleb-resnet34-LM` under CC-BY-4.0, subject to the model repositories' access conditions.
- On Apple Silicon macOS, Apple MLX and MLX Whisper provide local inference;
  the pinned quantized Whisper model repositories are declared in
  `worker/model-manifest.macos.json` and remain subject to their own notices and
  license terms.
- `mlx-whisper` 0.4.3 and `primePy` 1.3 are MIT-licensed. Their published
  distributions omit license files, so reviewed upstream license texts and
  source attribution are pinned under `third_party/licenses/python/` and copied
  into every official release notice bundle.

The authoritative repository/revision/license lists are
`worker/model-manifest.json` and `worker/model-manifest.macos.json`. A production
release process must generate a complete notice bundle from the resolved Node,
Cargo, Python, FFmpeg, and model locks rather than treating this summary as
exhaustive.
