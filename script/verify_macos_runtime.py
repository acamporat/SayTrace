#!/usr/bin/env python3
"""Independently verify a staged or embedded SayTrace macOS runtime."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import stat
import subprocess
from pathlib import Path, PurePosixPath

REQUIRED_ENTRYPOINTS = ("local-transcript-worker", "ffmpeg", "ffprobe")
RUNTIME_MANIFEST_NAME = "runtime-manifest.json"
MINIMUM_SYSTEM_VERSION = (15, 0)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def safe_relative_path(raw: str) -> PurePosixPath:
    try:
        raw.encode("utf-8")
    except UnicodeEncodeError as error:
        raise ValueError("Runtime contains a non-UTF-8 payload path.") from error
    relative = PurePosixPath(raw)
    if (
        not raw
        or relative.is_absolute()
        or "\\" in raw
        or ":" in raw
        or any(part in ("", ".", "..") for part in relative.parts)
    ):
        raise ValueError(f"Unsafe runtime manifest path: {raw!r}")
    return relative


def parse_macos_build_versions(output: str) -> list[tuple[int, ...]]:
    """Return macOS deployment targets from vtool's load-command output."""
    versions: list[tuple[int, ...]] = []
    command: str | None = None
    build_platform: str | None = None
    for raw_line in output.splitlines():
        line = raw_line.strip()
        if line.startswith("cmd "):
            command = line.removeprefix("cmd ")
            build_platform = None
            continue
        if command == "LC_BUILD_VERSION" and line.startswith("platform "):
            build_platform = line.removeprefix("platform ")
            continue
        if command == "LC_BUILD_VERSION" and line.startswith("minos "):
            if build_platform != "MACOS":
                rendered = build_platform or "missing"
                raise ValueError(
                    f"Runtime Mach-O targets a non-macOS platform ({rendered})."
                )
            raw_version = line.removeprefix("minos ")
        elif command == "LC_VERSION_MIN_MACOSX" and line.startswith("version "):
            raw_version = line.removeprefix("version ")
        else:
            continue
        if not all(part.isdigit() for part in raw_version.split(".")):
            raise ValueError(f"Invalid Mach-O deployment target: {raw_version}")
        versions.append(tuple(int(part) for part in raw_version.split(".")))
        command = None
        build_platform = None
    return versions


def verify_macho_compatibility(path: Path, description: str | bytes) -> None:
    arm64_marker = b"arm64" if isinstance(description, bytes) else "arm64"
    if arm64_marker not in description:
        raise ValueError(f"Runtime Mach-O does not contain arm64 code: {path}")
    result = subprocess.run(
        ["/usr/bin/vtool", "-arch", "arm64", "-show-build", str(path)],
        check=True,
        capture_output=True,
    )
    build_output = result.stdout
    if isinstance(build_output, bytes):
        # vtool's load-command fields are ASCII, but its path header can contain
        # arbitrary filesystem bytes. Preserve those bytes without weakening the
        # strict parsing of the platform and minimum-version tokens below.
        build_output = build_output.decode("utf-8", errors="surrogateescape")
    versions = parse_macos_build_versions(build_output)
    if not versions:
        raise ValueError(f"Runtime Mach-O has no macOS deployment target: {path}")
    target = (*MINIMUM_SYSTEM_VERSION, 0)[:3]
    unsupported = [version for version in versions if (*version, 0, 0)[:3] > target]
    if unsupported:
        rendered = ", ".join(".".join(map(str, version)) for version in unsupported)
        raise ValueError(
            "Runtime Mach-O requires a newer macOS version than 15.0 "
            f"({path}: {rendered})"
        )


def enumerate_runtime_files(runtime: Path, manifest_path: Path) -> dict[str, Path]:
    files: dict[str, Path] = {}
    comparison_paths: set[str] = set()

    def record(path: Path) -> None:
        raw = path.relative_to(runtime).as_posix()
        safe_relative_path(raw)
        comparison = raw.lower()
        if comparison in comparison_paths:
            raise ValueError(
                f"Runtime contains a case-insensitive duplicate payload path: {raw}"
            )
        comparison_paths.add(comparison)
        files[raw] = path

    def visit(directory: Path) -> None:
        for path in sorted(directory.iterdir(), key=lambda candidate: candidate.name):
            metadata = path.lstat()
            if path == manifest_path:
                if not stat.S_ISREG(metadata.st_mode):
                    raise ValueError("Runtime manifest must be an ordinary file.")
                continue
            if stat.S_ISDIR(metadata.st_mode):
                visit(path)
                continue
            if stat.S_ISLNK(metadata.st_mode):
                try:
                    resolved = path.resolve(strict=True)
                except (FileNotFoundError, RuntimeError) as error:
                    raise ValueError(
                        f"Runtime payload link is invalid: {path}"
                    ) from error
                if not resolved.is_relative_to(runtime):
                    raise ValueError(f"Runtime payload link escapes its root: {path}")
                if not resolved.is_file():
                    raise ValueError(
                        f"Runtime payload links must resolve to files: {path}"
                    )
                record(path)
                continue
            if stat.S_ISREG(metadata.st_mode):
                record(path)
                continue
            raise ValueError(
                f"Runtime contains an unsupported filesystem entry: {path}"
            )

    visit(runtime)
    return files


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--runtime", required=True, type=Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--source-revision", required=True)
    args = parser.parse_args()

    runtime = args.runtime.resolve(strict=True)
    if not runtime.is_dir():
        raise ValueError("Runtime root is not a directory.")
    manifest_path = runtime / RUNTIME_MANIFEST_NAME
    manifest_metadata = manifest_path.lstat()
    if not stat.S_ISREG(manifest_metadata.st_mode):
        raise ValueError("Runtime manifest must be an ordinary file.")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    expected_identity = {
        "schema_version": 1,
        "product": "SayTrace Runtime",
        "app_identifier": "com.localtranscript.desktop",
        "runtime_version": args.version,
        "variant": "apple-mlx",
        "architecture": "arm64",
        "minimum_system_version": "15.0",
        "source_revision": args.source_revision,
        "component_codesigned": True,
    }
    for key, expected in expected_identity.items():
        if manifest.get(key) != expected:
            raise ValueError(f"Runtime manifest {key!r} does not match the release.")

    records = manifest.get("payload")
    if not isinstance(records, list) or not records:
        raise ValueError("Runtime manifest payload is empty.")
    declared: set[str] = set()
    comparison_paths: set[str] = set()
    for record in records:
        if not isinstance(record, dict):
            raise TypeError("Runtime manifest payload record is malformed.")
        raw_path = str(record.get("path", ""))
        relative = safe_relative_path(raw_path)
        comparison_path = raw_path.lower()
        if raw_path in declared or comparison_path in comparison_paths:
            raise ValueError(f"Duplicate runtime manifest path: {raw_path}")
        declared.add(raw_path)
        comparison_paths.add(comparison_path)
        candidate = (runtime / Path(*relative.parts)).resolve(strict=True)
        if not candidate.is_relative_to(runtime) or not candidate.is_file():
            raise ValueError(f"Runtime payload escapes its root: {raw_path}")
        if candidate.stat().st_size != int(record.get("size", -1)):
            raise ValueError(f"Runtime payload size mismatch: {raw_path}")
        if sha256(candidate) != str(record.get("sha256", "")).lower():
            raise ValueError(f"Runtime payload digest mismatch: {raw_path}")

    actual = enumerate_runtime_files(runtime, manifest_path)
    if set(actual) != declared:
        raise ValueError("Runtime payload enumeration does not match its manifest.")
    for name in REQUIRED_ENTRYPOINTS:
        path = runtime / name
        if name not in declared or not path.is_file() or not os.access(path, os.X_OK):
            raise ValueError(f"Runtime entrypoint is missing or not executable: {name}")

    verified_code: set[Path] = set()
    for path in (actual[relative] for relative in sorted(actual)):
        resolved = path.resolve(strict=True)
        if resolved in verified_code:
            continue
        description = subprocess.run(
            ["/usr/bin/file", "-b", str(path)],
            check=True,
            capture_output=True,
        ).stdout
        macho_marker = b"Mach-O" if isinstance(description, bytes) else "Mach-O"
        if macho_marker not in description:
            continue
        verify_macho_compatibility(path, description)
        subprocess.run(
            ["/usr/bin/codesign", "--verify", "--strict", str(path)],
            check=True,
            capture_output=True,
        )
        verified_code.add(resolved)

    print(f"Verified {len(declared)} signed runtime payload files.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
