#!/usr/bin/env python3
"""Record a notarized macOS installer awaiting exact-artifact acceptance."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from macos_release_evidence import REQUIRED_CHECKS as REQUIRED_ACCEPTANCE_CHECKS


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def file_record(path: Path) -> dict[str, Any]:
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"Prepared release input must be an ordinary file: {path}")
    resolved = path.resolve(strict=True)
    return {
        "name": resolved.name,
        "size": resolved.stat().st_size,
        "sha256": sha256(resolved),
    }


def accepted_notary_result(path: Path, *, expected_name: str) -> str:
    result = json.loads(path.resolve(strict=True).read_text(encoding="utf-8"))
    if not isinstance(result, dict) or result.get("status") != "Accepted":
        raise ValueError("Prepared release notary result was not accepted by Apple.")
    if result.get("name") != expected_name:
        raise ValueError("Prepared release notary result has the wrong artifact name.")
    submission_id = result.get("id")
    if not isinstance(submission_id, str) or not re.fullmatch(
        r"[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}",
        submission_id,
    ):
        raise ValueError("Prepared release notary result has no valid submission ID.")
    return submission_id.lower()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--dmg", required=True, type=Path)
    parser.add_argument("--app-notary-payload", required=True, type=Path)
    parser.add_argument("--app-notary-result", required=True, type=Path)
    parser.add_argument("--dmg-notary-result", required=True, type=Path)
    parser.add_argument("--runtime-manifest", required=True, type=Path)
    parser.add_argument("--ffmpeg-source", required=True, type=Path)
    parser.add_argument("--ffmpeg-source-signature", required=True, type=Path)
    parser.add_argument("--release-notes", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--acceptance-template-output", required=True, type=Path)
    args = parser.parse_args()

    if not re.fullmatch(
        r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", args.version
    ):
        raise ValueError("Prepared release version must be stable semantic versioning.")
    if not re.fullmatch(r"[0-9a-f]{40}", args.source_revision):
        raise ValueError("Prepared release source revision must be a full Git commit.")

    inputs = {
        "installer": file_record(args.dmg),
        "app_notary_payload": file_record(args.app_notary_payload),
        "app_notary_result": file_record(args.app_notary_result),
        "dmg_notary_result": file_record(args.dmg_notary_result),
        "ffmpeg_source": file_record(args.ffmpeg_source),
        "ffmpeg_source_signature": file_record(args.ffmpeg_source_signature),
        "release_notes": file_record(args.release_notes),
    }
    runtime_manifest_path = args.runtime_manifest.resolve(strict=True)
    if args.runtime_manifest.is_symlink() or not runtime_manifest_path.is_file():
        raise ValueError("Prepared runtime manifest must be an ordinary file.")
    runtime = json.loads(runtime_manifest_path.read_text(encoding="utf-8"))
    validation = (
        runtime.get("runtime_validation") if isinstance(runtime, dict) else None
    )
    if (
        not isinstance(runtime, dict)
        or runtime.get("runtime_version") != args.version
        or runtime.get("source_revision") != args.source_revision
        or runtime.get("component_codesigned") is not True
        or not isinstance(validation, dict)
        or validation.get("worker_handshake") != "passed"
        or validation.get("model_inference") != "passed"
    ):
        raise ValueError(
            "Prepared release runtime identity or validation is incomplete."
        )

    app_notarization_id = accepted_notary_result(
        args.app_notary_result, expected_name=args.app_notary_payload.name
    )
    dmg_notarization_id = accepted_notary_result(
        args.dmg_notary_result, expected_name=args.dmg.name
    )
    output = args.output.resolve(strict=False)
    template_output = args.acceptance_template_output.resolve(strict=False)
    if output.parent != template_output.parent:
        raise ValueError("Prepared metadata outputs must share one directory.")
    output.parent.mkdir(parents=True, exist_ok=True)
    prepared = {
        "schema_version": 1,
        "product": "SayTrace macOS prepared release",
        "version": args.version,
        "source_revision": args.source_revision,
        "prepared_at_utc": datetime.now(UTC).isoformat().replace("+00:00", "Z"),
        "state": "awaiting_exact_installer_physical_acceptance",
        "distribution": {
            "official": False,
            "developer_id_signed": True,
            "notarized": True,
            "stapled": True,
            "app_notarization_id": app_notarization_id,
            "dmg_notarization_id": dmg_notarization_id,
        },
        "runtime_manifest": {
            "name": runtime_manifest_path.name,
            "sha256": sha256(runtime_manifest_path),
        },
        "assets": inputs,
    }
    acceptance = {
        "schema_version": 2,
        "product": "SayTrace macOS physical acceptance",
        "version": args.version,
        "source_revision": args.source_revision,
        "installer": inputs["installer"],
        "performed_at_utc": "REPLACE_WITH_ISO_8601_UTC_TIMESTAMP",
        "hardware": {
            "architecture": "arm64",
            "model_identifier": "REPLACE_WITH_MAC_MODEL_IDENTIFIER",
            "macos_version": "REPLACE_WITH_MACOS_VERSION",
        },
        "checks": {name: "not_run" for name in REQUIRED_ACCEPTANCE_CHECKS},
        "operator_confirmation": False,
    }
    output.write_text(json.dumps(prepared, indent=2) + "\n", encoding="utf-8")
    template_output.write_text(
        json.dumps(acceptance, indent=2) + "\n", encoding="utf-8"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
