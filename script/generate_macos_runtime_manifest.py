#!/usr/bin/env python3
"""Create the integrity manifest for a staged Apple Silicon worker runtime."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

REQUIRED_ENTRYPOINTS = ("local-transcript-worker", "ffmpeg", "ffprobe")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def payload_records(runtime_root: Path) -> list[dict[str, Any]]:
    canonical_root = runtime_root.resolve(strict=True)
    records: list[dict[str, Any]] = []
    for path in sorted(runtime_root.rglob("*")):
        if not path.is_file() or path.name == "runtime-manifest.json":
            continue
        resolved = path.resolve(strict=True)
        if not resolved.is_relative_to(canonical_root):
            raise ValueError(f"Runtime payload link escapes the staging root: {path}")
        records.append(
            {
                "path": path.relative_to(runtime_root).as_posix(),
                "size": path.stat().st_size,
                "sha256": sha256(path),
            }
        )
    return records


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--runtime-root", required=True, type=Path)
    parser.add_argument("--model-manifest", required=True, type=Path)
    parser.add_argument("--app-version", required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--ffmpeg-version-line", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    runtime_root = args.runtime_root.resolve(strict=True)
    output = args.output.resolve(strict=False)
    if output.parent != runtime_root:
        raise ValueError(
            "The embedded runtime manifest must be written at the runtime root."
        )
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
        "component_codesigned": False,
        "cpu_fallback_declared": True,
        "runtime_validation": {
            "worker_handshake": "not_performed_by_packager",
            "mlx_metal": "required_on_apple_silicon",
            "torch_mps": "preferred_with_cpu_fallback",
            "model_inference": "not_performed_by_packager",
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
        "payload": payload_records(runtime_root),
    }
    output.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
