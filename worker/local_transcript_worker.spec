# -*- mode: python ; coding: utf-8 -*-
"""PyInstaller onedir build for the Python 3.13 sidecar."""

import os
import platform
import sys
from pathlib import Path

from PyInstaller.utils.hooks import (
    collect_all,
    collect_data_files,
    collect_dynamic_libs,
    collect_submodules,
)

project_root = Path(SPECPATH)
source_root = project_root / "src"
entrypoint = project_root / "pyinstaller_entrypoint.py"
is_windows = sys.platform == "win32"
is_macos_arm64 = sys.platform == "darwin" and platform.machine() == "arm64"

if not (is_windows or is_macos_arm64):
    raise RuntimeError(
        "The packaged worker is supported only on Windows x64 or Apple Silicon macOS."
    )

ffmpeg_bin = os.environ.get("LOCAL_TRANSCRIPT_BUILD_FFMPEG_BIN")
ffmpeg_dll_directory = None
if ffmpeg_bin:
    if is_windows:
        ffmpeg_dll_directory = os.add_dll_directory(ffmpeg_bin)
    os.environ["PATH"] = os.pathsep.join((ffmpeg_bin, os.environ.get("PATH", "")))

datas = [
    (str(project_root / "model-manifest.json"), "local_transcript_worker"),
    (str(project_root / "model-manifest.macos.json"), "local_transcript_worker"),
]
binaries = []
hiddenimports = []


def runtime_data_only(items):
    build_only_suffixes = (".c", ".cc", ".cmake", ".cpp", ".cuh", ".h", ".hpp", ".lib")
    result = []
    for source, destination in items:
        normalized = source.replace("\\", "/").lower()
        if normalized.endswith(build_only_suffixes):
            continue
        if any(part in normalized for part in ("/include/", "/test/", "/tests/", "/testing/")):
            continue
        result.append((source, destination))
    return result


def runtime_hidden_imports_only(items):
    excluded = (".benchmarks", ".sample", ".samples", ".test", ".tests", ".testing")
    return [name for name in items if not any(part in name for part in excluded)]


common_packages = (
    "huggingface_hub",
    "numpy",
    "pyannote.audio",
    "pyannote.core",
    "safetensors",
    "torch",
    "torchaudio",
    "torchcodec",
    "torchvision",
)

windows_packages = (
    "av",
    "ctranslate2",
    "faster_whisper",
    "tokenizers",
    "whisperx",
)

# MLX and pyannote both load portions of their stacks dynamically. Explicitly
# collect those packages so the arm64 onedir runtime does not depend on the
# developer virtual environment after it is copied into the app bundle.
macos_packages = (
    "asteroid_filterbanks",
    "lightning",
    "mlx",
    "mlx_whisper",
    "numba",
    "pyannote.database",
    "pyannote.metrics",
    "pyannote.pipeline",
    "pytorch_lightning",
    "pytorch_metric_learning",
    "scipy",
    "tiktoken",
    "torch_audiomentations",
    "torch_pitch_shift",
    "torchmetrics",
)

platform_packages = windows_packages if is_windows else macos_packages
for package in (*common_packages, *platform_packages):
    package_datas, package_binaries, package_hiddenimports = collect_all(package)
    datas += runtime_data_only(package_datas)
    binaries += package_binaries
    hiddenimports += runtime_hidden_imports_only(package_hiddenimports)

if is_windows:
    # WhisperX v3 alignment imports the Hugging Face Wav2Vec2 implementation through
    # Transformers' lazy module registry. Collect that model family explicitly
    # instead of every unrelated text, vision, and multimodal model.
    datas += runtime_data_only(collect_data_files("transformers"))
    binaries += collect_dynamic_libs("transformers")
    hiddenimports += collect_submodules("transformers.models.wav2vec2")

analysis = Analysis(
    [str(entrypoint)],
    pathex=[str(source_root)],
    binaries=binaries,
    datas=datas,
    hiddenimports=hiddenimports,
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    excludes=[],
    noarchive=False,
    optimize=1,
)
pyz = PYZ(analysis.pure)

exe = EXE(
    pyz,
    analysis.scripts,
    [],
    exclude_binaries=True,
    name="local-transcript-worker",
    debug=False,
    bootloader_ignore_signals=False,
    strip=False,
    upx=False,
    # Keep inherited stdio handles; the Rust supervisor launches this console binary hidden.
    console=True,
    disable_windowed_traceback=False,
)

collect = COLLECT(
    exe,
    analysis.binaries,
    analysis.datas,
    strip=False,
    upx=False,
    name="local-transcript-worker",
)
