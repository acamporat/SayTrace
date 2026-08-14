#!/usr/bin/env python3
"""Generate public provenance metadata for a SayTrace macOS artifact."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from macos_release_evidence import REQUIRED_CHECKS, validate_evidence

REQUIRED_ACCEPTANCE_CHECKS = REQUIRED_CHECKS


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def file_record(path: Path) -> dict[str, Any]:
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"Release asset must be an ordinary file: {path}")
    resolved = path.resolve(strict=True)
    return {
        "name": resolved.name,
        "size": resolved.stat().st_size,
        "sha256": sha256(resolved),
    }


def accepted_notary_result(path: Path, *, expected_name: str) -> str:
    resolved = path.resolve(strict=True)
    if path.is_symlink() or not resolved.is_file():
        raise ValueError("Notary result must be an ordinary JSON file.")
    result = json.loads(resolved.read_text(encoding="utf-8"))
    if not isinstance(result, dict) or result.get("status") != "Accepted":
        raise ValueError("Notary result was not accepted by Apple.")
    submission_id = result.get("id")
    if not isinstance(submission_id, str) or not re.fullmatch(
        r"[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}",
        submission_id,
    ):
        raise ValueError("Notary result is missing a valid submission ID.")
    submitted_name = result.get("name")
    if submitted_name != expected_name:
        raise ValueError("Notary result name does not match the submitted artifact.")
    return submission_id.lower()


def run_trust_check(command: list[str]) -> str:
    result = subprocess.run(
        command,
        check=True,
        capture_output=True,
        text=True,
    )
    return f"{result.stdout}\n{result.stderr}"


def verify_developer_id_artifact(path: Path, *, kind: str) -> str:
    resolved = path.resolve(strict=True)
    if kind == "app":
        if path.is_symlink() or not resolved.is_dir() or resolved.suffix != ".app":
            raise ValueError("Official manifest requires a macOS app bundle.")
        run_trust_check(
            ["/usr/bin/codesign", "--verify", "--deep", "--strict", str(resolved)]
        )
        assessment = [
            "/usr/sbin/spctl",
            "--assess",
            "--type",
            "execute",
            "--verbose=4",
            str(resolved),
        ]
    else:
        if path.is_symlink() or not resolved.is_file() or resolved.suffix != ".dmg":
            raise ValueError("Official manifest requires a DMG installer.")
        run_trust_check(["/usr/bin/codesign", "--verify", "--strict", str(resolved)])
        assessment = [
            "/usr/sbin/spctl",
            "--assess",
            "--type",
            "open",
            "--context",
            "context:primary-signature",
            "--verbose=4",
            str(resolved),
        ]
    details = run_trust_check(
        ["/usr/bin/codesign", "--display", "--verbose=4", str(resolved)]
    )
    if "Authority=Developer ID Application:" not in details:
        raise ValueError(
            "Artifact is not signed by a Developer ID Application identity."
        )
    run_trust_check(["/usr/bin/xcrun", "stapler", "validate", str(resolved)])
    run_trust_check(assessment)
    team_identifiers = [
        line.removeprefix("TeamIdentifier=").strip()
        for line in details.splitlines()
        if line.startswith("TeamIdentifier=")
    ]
    if len(team_identifiers) != 1 or not team_identifiers[0]:
        raise ValueError("Artifact has no unambiguous Developer ID team.")
    return team_identifiers[0]


def physical_acceptance_record(
    path: Path, *, version: str, source_revision: str, installer: Path
) -> dict[str, Any]:
    data = validate_evidence(
        path,
        version=version,
        source_revision=source_revision,
        installer=installer,
    )
    resolved = path.resolve(strict=True)
    return {
        "evidence": file_record(resolved),
        "performed_at_utc": data.get("performed_at_utc"),
        "hardware": data.get("hardware"),
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--dmg", required=True, type=Path)
    parser.add_argument("--runtime-manifest", required=True, type=Path)
    parser.add_argument("--ffmpeg-source", required=True, type=Path)
    parser.add_argument("--ffmpeg-source-signature", required=True, type=Path)
    parser.add_argument("--official", action="store_true")
    parser.add_argument("--app", type=Path)
    parser.add_argument("--app-notary-result", type=Path)
    parser.add_argument("--app-notary-payload", type=Path)
    parser.add_argument("--dmg-notary-result", type=Path)
    parser.add_argument("--acceptance-evidence", type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    official_inputs = (
        args.app,
        args.app_notary_result,
        args.app_notary_payload,
        args.dmg_notary_result,
        args.acceptance_evidence,
    )
    if args.official and not all(official_inputs):
        parser.error("official manifests require app, notary, and acceptance evidence")
    if not args.official and any(official_inputs):
        parser.error("candidate manifests cannot include official trust evidence")

    runtime_manifest_path = args.runtime_manifest.resolve(strict=True)
    if args.runtime_manifest.is_symlink() or not runtime_manifest_path.is_file():
        raise ValueError("Runtime manifest must be an ordinary file.")
    runtime_manifest = json.loads(runtime_manifest_path.read_text(encoding="utf-8"))
    if str(runtime_manifest.get("runtime_version")) != args.version:
        raise ValueError("Runtime manifest version does not match the release version.")
    if str(runtime_manifest.get("source_revision")) != args.source_revision:
        raise ValueError(
            "Runtime manifest source revision does not match the release revision."
        )
    if runtime_manifest.get("component_codesigned") is not True:
        raise ValueError("Runtime manifest does not attest to signed components.")
    validation = runtime_manifest.get("runtime_validation")
    if not isinstance(validation, dict):
        raise TypeError("Runtime manifest validation record is missing.")
    if args.official and (
        validation.get("worker_handshake") != "passed"
        or validation.get("model_inference") != "passed"
    ):
        raise ValueError("Official runtime validation has not passed.")

    app_notarization_id: str | None = None
    dmg_notarization_id: str | None = None
    acceptance: dict[str, Any] | None = None
    if args.official:
        assert args.app is not None
        assert args.app_notary_result is not None
        assert args.app_notary_payload is not None
        assert args.dmg_notary_result is not None
        assert args.acceptance_evidence is not None
        app_payload = args.app_notary_payload.resolve(strict=True)
        if (
            args.app_notary_payload.is_symlink()
            or not app_payload.is_file()
            or app_payload.suffix != ".zip"
        ):
            raise ValueError("App notarization payload must be an ordinary ZIP file.")
        app_notarization_id = accepted_notary_result(
            args.app_notary_result, expected_name=app_payload.name
        )
        dmg_notarization_id = accepted_notary_result(
            args.dmg_notary_result, expected_name=args.dmg.resolve(strict=True).name
        )
        acceptance = physical_acceptance_record(
            args.acceptance_evidence,
            version=args.version,
            source_revision=args.source_revision,
            installer=args.dmg,
        )
        app_team = verify_developer_id_artifact(args.app, kind="app")
        dmg_team = verify_developer_id_artifact(args.dmg, kind="dmg")
        if app_team != dmg_team:
            raise ValueError("App and DMG Developer ID teams do not match.")

    output = args.output.resolve(strict=False)
    output.parent.mkdir(parents=True, exist_ok=True)
    manifest = {
        "schema_version": 1,
        "product": "SayTrace",
        "version": args.version,
        "tag": f"v{args.version}",
        "platform": "macOS",
        "architecture": "arm64",
        "minimum_system_version": "15.0",
        "source_revision": args.source_revision,
        "generated_at_utc": datetime.now(UTC).isoformat().replace("+00:00", "Z"),
        "distribution": {
            "official": args.official,
            "developer_id_signed": args.official,
            "notarized": args.official,
            "stapled": args.official,
            "app_notarization_id": app_notarization_id,
            "dmg_notarization_id": dmg_notarization_id,
        },
        "runtime": {
            "manifest_sha256": sha256(runtime_manifest_path),
            "worker_handshake": runtime_manifest["runtime_validation"][
                "worker_handshake"
            ],
            "model_inference": runtime_manifest["runtime_validation"][
                "model_inference"
            ],
            "models_bundled": False,
        },
        "assets": {
            "installer": file_record(args.dmg),
            "ffmpeg_source": file_record(args.ffmpeg_source),
            "ffmpeg_source_signature": file_record(args.ffmpeg_source_signature),
        },
    }
    if acceptance is not None:
        manifest["physical_acceptance"] = acceptance
    output.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
