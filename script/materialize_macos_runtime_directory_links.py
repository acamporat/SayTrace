#!/usr/bin/env python3
"""Replace safe internal directory symlinks in a staged macOS runtime.

PyInstaller emits the conventional directory links inside Python.framework.
The signed SayTrace runtime manifest intentionally inventories only ordinary
files and file symlinks, so those directory aliases are copied into ordinary
directories before hashing. Every link is validated as relative, resolvable,
and contained by the staging root before anything is changed.
"""

from __future__ import annotations

import argparse
import os
import shutil
import stat
import uuid
from pathlib import Path


def runtime_links(root: Path) -> list[tuple[Path, Path]]:
    canonical_root = root.resolve(strict=True)
    links: list[tuple[Path, Path]] = []

    def visit(directory: Path) -> None:
        for path in sorted(directory.iterdir(), key=lambda candidate: candidate.name):
            metadata = path.lstat()
            if stat.S_ISDIR(metadata.st_mode):
                visit(path)
                continue
            if not stat.S_ISLNK(metadata.st_mode):
                continue
            raw_target = Path(os.readlink(path))
            if raw_target.is_absolute():
                raise ValueError(f"Runtime payload link must be relative: {path}")
            try:
                resolved = path.resolve(strict=True)
            except (FileNotFoundError, RuntimeError) as error:
                raise ValueError(f"Runtime payload link is invalid: {path}") from error
            if not resolved.is_relative_to(canonical_root):
                raise ValueError(
                    f"Runtime payload link escapes the staging root: {path}"
                )
            if not (resolved.is_file() or resolved.is_dir()):
                raise ValueError(
                    f"Runtime payload link has an unsupported target: {path}"
                )
            if resolved.is_dir() and path.is_relative_to(resolved):
                raise ValueError(
                    "Runtime payload directory link target contains the link "
                    f"itself: {path}"
                )
            links.append((path, resolved))

    visit(canonical_root)
    return links


def materialize_directory_links(root: Path) -> int:
    canonical_root = root.resolve(strict=True)
    materialized = 0
    while True:
        links = runtime_links(canonical_root)
        directory_links = [
            (path, resolved) for path, resolved in links if resolved.is_dir()
        ]
        if not directory_links:
            return materialized
        path, resolved = max(directory_links, key=lambda item: len(item[0].parts))
        temporary = path.with_name(f".{path.name}.saytrace-copy-{uuid.uuid4().hex}")
        try:
            shutil.copytree(resolved, temporary, symlinks=False)
            path.unlink()
            temporary.replace(path)
        except BaseException:
            if temporary.exists():
                shutil.rmtree(temporary)
            raise
        materialized += 1


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runtime", required=True, type=Path)
    args = parser.parse_args()
    runtime = args.runtime
    if runtime.is_symlink() or not runtime.is_dir():
        raise ValueError("Runtime root must be an ordinary directory.")
    count = materialize_directory_links(runtime)
    print(f"Materialized {count} internal runtime directory links.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
