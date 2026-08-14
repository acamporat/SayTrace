#!/usr/bin/env python3
"""Verify one signed Apple Silicon executable for the supported macOS target."""

from __future__ import annotations

import argparse
import os
import stat
import subprocess
from pathlib import Path

from verify_macos_runtime import verify_macho_compatibility


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--path", required=True, type=Path)
    args = parser.parse_args()

    metadata = args.path.lstat()
    if not stat.S_ISREG(metadata.st_mode):
        raise ValueError("macOS executable must be an ordinary file.")
    path = args.path.resolve(strict=True)
    if not os.access(path, os.X_OK):
        raise ValueError("macOS executable is not executable.")

    description = subprocess.run(
        ["/usr/bin/file", "-b", str(path)],
        check=True,
        capture_output=True,
    ).stdout
    marker = b"Mach-O" if isinstance(description, bytes) else "Mach-O"
    if marker not in description:
        raise ValueError("macOS executable is not a Mach-O file.")

    verify_macho_compatibility(path, description)
    subprocess.run(
        ["/usr/bin/codesign", "--verify", "--strict", str(path)],
        check=True,
        capture_output=True,
    )
    print(f"Verified signed Apple Silicon macOS executable: {path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
