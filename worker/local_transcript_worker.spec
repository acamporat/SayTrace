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


windows_common_packages = (
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

# MLX Whisper loads model code and assets dynamically, while Numba discovers
# compiled support modules at runtime. PyInstaller's maintained hooks and the
# pinned hidden imports below cover the remaining scientific stack without a
# second collect_all traversal of every package namespace.
macos_packages = (
    "mlx",
    "mlx_whisper",
    "numba",
)

if is_windows:
    collected_packages = (*windows_common_packages, *windows_packages)
    excluded_packages = []
else:
    # Let PyInstaller's platform hooks collect the imported NumPy/PyTorch
    # runtime surface. collect_all(torch) traverses distributed, compiler,
    # training, testing, and CUDA-only namespaces that the local inference
    # worker never executes. TorchCodec is deliberately omitted: SayTrace
    # supplies decoded waveforms to pyannote, and TorchCodec's macOS binaries
    # require incompatible shared FFmpeg 4-7 libraries.
    collected_packages = macos_packages
    excluded_packages = [
        "av",
        "ctranslate2",
        "faster_whisper",
        "pytorch_lightning",
        "pytorch_metric_learning",
        "pytest",
        "tokenizers",
        "torchcodec",
        "whisperx",
    ]
    hiddenimports += [
        "pyannote.audio.models.embedding.wespeaker",
        "pyannote.audio.models.segmentation.PyanNet",
        "pyannote.audio.pipelines.clustering",
        "pyannote.audio.pipelines.speaker_diarization",
        "pyannote.audio.pipelines.speaker_verification",
        "safetensors.mlx",
        "safetensors.torch",
    ]
    # Pyannote reads this package-data file unconditionally while importing
    # its telemetry module, even when metrics are disabled for offline use.
    datas += collect_data_files(
        "pyannote.audio",
        includes=["telemetry/config.yaml"],
    )

for package in collected_packages:
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
    excludes=excluded_packages,
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
