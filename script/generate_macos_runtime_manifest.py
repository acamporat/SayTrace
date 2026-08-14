#!/usr/bin/env python3
"""Create the integrity manifest for a staged Apple Silicon worker runtime."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import stat
from datetime import UTC, datetime
from pathlib import Path, PurePosixPath
from typing import Any

REQUIRED_ENTRYPOINTS = ("local-transcript-worker", "ffmpeg", "ffprobe")
RUNTIME_MANIFEST_NAME = "runtime-manifest.json"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def safe_runtime_relative_path(runtime_root: Path, path: Path) -> str:
    raw = path.relative_to(runtime_root).as_posix()
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
        raise ValueError(f"Runtime contains an unsafe payload path: {raw!r}")
    return raw


def enumerate_payload_files(runtime_root: Path, manifest_path: Path) -> list[Path]:
    canonical_root = runtime_root.resolve(strict=True)
    files: list[Path] = []

    def visit(directory: Path) -> None:
        for path in sorted(directory.iterdir(), key=lambda candidate: candidate.name):
            metadata = path.lstat()
            if path == manifest_path:
                if not stat.S_ISREG(metadata.st_mode):
                    raise ValueError(
                        "The embedded runtime manifest must be an ordinary file."
                    )
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
                if not resolved.is_relative_to(canonical_root):
                    raise ValueError(
                        f"Runtime payload link escapes the staging root: {path}"
                    )
                if not resolved.is_file():
                    raise ValueError(
                        f"Runtime payload links must resolve to files: {path}"
                    )
                files.append(path)
                continue
            if stat.S_ISREG(metadata.st_mode):
                files.append(path)
                continue
            raise ValueError(
                f"Runtime contains an unsupported filesystem entry: {path}"
            )

    visit(canonical_root)
    return files


def payload_records(runtime_root: Path, manifest_path: Path) -> list[dict[str, Any]]:
    canonical_root = runtime_root.resolve(strict=True)
    canonical_manifest = manifest_path.parent.resolve(strict=True) / manifest_path.name
    records: list[dict[str, Any]] = []
    comparison_paths: set[str] = set()
    for path in enumerate_payload_files(canonical_root, canonical_manifest):
        resolved = path.resolve(strict=True)
        if not resolved.is_relative_to(canonical_root):
            raise ValueError(f"Runtime payload link escapes the staging root: {path}")
        relative = safe_runtime_relative_path(canonical_root, path)
        comparison = relative.lower()
        if comparison in comparison_paths:
            raise ValueError(
                f"Runtime contains a case-insensitive duplicate payload path: {relative}"
            )
        comparison_paths.add(comparison)
        records.append(
            {
                "path": relative,
                "size": path.stat().st_size,
                "sha256": sha256(path),
            }
        )
    return sorted(records, key=lambda record: str(record["path"]))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--runtime-root", required=True, type=Path)
    parser.add_argument("--model-manifest", required=True, type=Path)
    parser.add_argument("--app-version", required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--ffmpeg-version-line", required=True)
    parser.add_argument("--component-codesigned", action="store_true")
    parser.add_argument(
        "--worker-handshake",
        choices=("not_performed_by_packager", "passed"),
        default="not_performed_by_packager",
    )
    parser.add_argument(
        "--model-inference",
        choices=("not_performed_by_packager", "passed"),
        default="not_performed_by_packager",
    )
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    runtime_root = args.runtime_root.resolve(strict=True)
    output_argument = (
        args.output if args.output.is_absolute() else Path.cwd() / args.output
    )
    output_parent = output_argument.parent.resolve(strict=True)
    output = output_parent / output_argument.name
    if output.parent != runtime_root or output.name != RUNTIME_MANIFEST_NAME:
        raise ValueError(
            "The embedded runtime manifest must be written at the runtime root."
        )
    if output.is_symlink() or (output.exists() and not output.is_file()):
        raise ValueError("The embedded runtime manifest must be an ordinary file.")
    for name in REQUIRED_ENTRYPOINTS:
        candidate = runtime_root / name
        if not candidate.is_file() or not os.access(candidate, os.X_OK):
            raise ValueError(f"Missing executable runtime entrypoint: {candidate}")

    model_manifest_path = args.model_manifest.resolve(strict=True)
    model_manifest = json.loads(model_manifest_path.read_text(encoding="utf-8"))
    model_revisions = {
        str(model["key"]): str(model["revision"]) for model in model_manifest["models"]
    }

    manifest = {
        "schema_version": 1,
        "product": "SayTrace Runtime",
        "app_identifier": "com.localtranscript.desktop",
        "runtime_version": args.app_version,
        "variant": "apple-mlx",
        "architecture": "arm64",
        "minimum_system_version": "15.0",
        "install_scope": "app_bundle",
        "install_relative_path": "runtime",
        "worker_protocol_version": "1.0",
        "pipeline_version": str(model_manifest["pipeline_version"]),
        "source_revision": args.source_revision,
        "generated_at_utc": datetime.now(UTC).isoformat().replace("+00:00", "Z"),
        "component_codesigned": args.component_codesigned,
        "cpu_fallback_declared": True,
        "runtime_validation": {
            "worker_handshake": args.worker_handshake,
            "mlx_metal": "required_on_apple_silicon",
            "torch_mps": "preferred_with_cpu_fallback",
            "model_inference": args.model_inference,
        },
        "ffmpeg": {
            "version_line": args.ffmpeg_version_line,
            "sha256": sha256(runtime_root / "ffmpeg"),
            "ffprobe_sha256": sha256(runtime_root / "ffprobe"),
        },
        "model_manifest": {
            "bundled_models": False,
            "sha256": sha256(model_manifest_path),
            "revisions": model_revisions,
        },
        "payload": payload_records(runtime_root, output),
    }
    output.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
