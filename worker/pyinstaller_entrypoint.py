"""PyInstaller launcher that preserves package import semantics."""

import hashlib
import multiprocessing
import os
import sys
from pathlib import Path


def configure_macos_matplotlib_cache(
    *,
    home: Path | None = None,
    runtime_root: Path | None = None,
) -> Path | None:
    """Persist Matplotlib's font cache for a stable macOS onedir runtime.

    PyInstaller's generic Matplotlib hook deliberately creates a throwaway
    cache for onefile bundles. SayTrace ships an onedir worker, so keying a
    private cache by the resolved runtime path is safe and avoids rescanning
    every system font each time Pyannote is imported. Any filesystem failure
    leaves PyInstaller's isolated temporary cache in place.
    """

    if sys.platform != "darwin" or not getattr(sys, "frozen", False):
        return None
    selected_root = runtime_root
    if selected_root is None:
        raw_root = getattr(sys, "_MEIPASS", None)
        if not isinstance(raw_root, str) or not raw_root:
            return None
        selected_root = Path(raw_root)
    try:
        resolved_root = selected_root.resolve(strict=True)
        cache_key = hashlib.sha256(os.fsencode(str(resolved_root))).hexdigest()[:16]
        cache_root = (
            (home or Path.home())
            / "Library"
            / "Caches"
            / "com.localtranscript.desktop"
            / "matplotlib"
            / cache_key
        )
        cache_root.mkdir(mode=0o700, parents=True, exist_ok=True)
        cache_root.chmod(0o700)
    except OSError:
        return None
    os.environ["MPLCONFIGDIR"] = str(cache_root)
    return cache_root


if __name__ == "__main__":
    configure_macos_matplotlib_cache()

    # PyTorch, Pyannote, and their scientific dependencies may create helper
    # processes. PyInstaller re-enters this executable for those children, so
    # dispatch them before importing the worker's heavyweight module graph.
    multiprocessing.freeze_support()

    from local_transcript_worker.__main__ import main

    raise SystemExit(main())
