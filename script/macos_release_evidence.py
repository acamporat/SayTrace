"""Shared exact-installer physical-acceptance validation for macOS releases."""

from __future__ import annotations

import hashlib
import json
import re
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

REQUIRED_CHECKS = (
    "installer_sha256_verified_on_test_mac",
    "clean_mac_install_and_first_run",
    "microphone_only_repeated_capture",
    "system_audio_only_repeated_capture",
    "combined_capture_repeated_capture",
    "permission_grant_denial_reset_revocation",
    "combined_wav_nonempty_and_channels_verified",
    "pause_resume_stop_writer_finalization",
    "long_combined_capture_soak",
    "av_clock_alignment",
    "no_new_diagnostic_crash_report",
)
EVIDENCE_KEYS = {
    "schema_version",
    "product",
    "version",
    "source_revision",
    "installer",
    "performed_at_utc",
    "hardware",
    "checks",
    "operator_confirmation",
}
HARDWARE_KEYS = {"architecture", "model_identifier", "macos_version"}
MAC_MODEL_PATTERN = re.compile(
    r"(?:Mac|MacBookAir|MacBookPro|Macmini|MacStudio|MacPro|iMac)[0-9]+,[0-9]+"
)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def installer_record(path: Path) -> dict[str, object]:
    if path.is_symlink() or not path.is_file():
        raise ValueError("release installer must be an ordinary file")
    resolved = path.resolve(strict=True)
    return {
        "name": resolved.name,
        "size": resolved.stat().st_size,
        "sha256": sha256(resolved),
    }


def parse_timestamp(raw: object) -> datetime:
    if not isinstance(raw, str) or not raw.endswith("Z"):
        raise ValueError("performed_at_utc must be an ISO-8601 UTC timestamp")
    try:
        value = datetime.fromisoformat(raw[:-1] + "+00:00")
    except ValueError as error:
        raise ValueError("performed_at_utc is invalid") from error
    if value.tzinfo is None:
        raise ValueError("performed_at_utc must include a timezone")
    return value.astimezone(UTC)


def validate_evidence(
    path: Path,
    *,
    version: str,
    source_revision: str,
    installer: Path,
    now: datetime | None = None,
) -> dict[str, Any]:
    if path.is_symlink() or not path.is_file():
        raise ValueError("acceptance evidence must be an ordinary JSON file")
    data = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(data, dict):
        raise TypeError("acceptance evidence must be a JSON object")
    if set(data) != EVIDENCE_KEYS:
        raise ValueError("acceptance evidence top-level inventory is not exact")
    expected_identity = {
        "schema_version": 2,
        "product": "SayTrace macOS physical acceptance",
        "version": version,
        "source_revision": source_revision,
    }
    for key, expected in expected_identity.items():
        if data.get(key) != expected:
            raise ValueError(f"acceptance evidence {key!r} does not match the release")

    recorded_installer = data.get("installer")
    if not isinstance(recorded_installer, dict):
        raise TypeError("acceptance evidence installer record is missing")
    if recorded_installer != installer_record(installer):
        raise ValueError(
            "acceptance evidence does not match the exact release installer"
        )

    performed = parse_timestamp(data.get("performed_at_utc"))
    current = (now or datetime.now(UTC)).astimezone(UTC)
    if performed > current + timedelta(minutes=5):
        raise ValueError("acceptance evidence timestamp is in the future")
    if performed < current - timedelta(days=30):
        raise ValueError("acceptance evidence is older than 30 days")

    hardware = data.get("hardware")
    if not isinstance(hardware, dict) or set(hardware) != HARDWARE_KEYS:
        raise TypeError("acceptance evidence hardware record is missing or malformed")
    if hardware.get("architecture") != "arm64":
        raise ValueError("acceptance evidence must come from Apple Silicon")
    model_identifier = hardware.get("model_identifier")
    if not isinstance(model_identifier, str) or not MAC_MODEL_PATTERN.fullmatch(
        model_identifier
    ):
        raise ValueError("acceptance evidence requires a real Mac model identifier")
    macos_version = hardware.get("macos_version")
    if not isinstance(macos_version, str) or not re.fullmatch(
        r"(?:1[5-9]|[2-9][0-9])(?:\.[0-9]+){0,2}", macos_version
    ):
        raise ValueError("acceptance evidence requires macOS 15 or newer")

    checks = data.get("checks")
    if not isinstance(checks, dict):
        raise TypeError("acceptance evidence checks are missing")
    unexpected = set(checks) - set(REQUIRED_CHECKS)
    missing = set(REQUIRED_CHECKS) - set(checks)
    if missing or unexpected:
        raise ValueError(
            "acceptance evidence check inventory is not exact: "
            f"missing={sorted(missing)}, unexpected={sorted(unexpected)}"
        )
    failed = [name for name in REQUIRED_CHECKS if checks.get(name) != "passed"]
    if failed:
        raise ValueError(f"physical-Mac acceptance checks have not passed: {failed}")
    if data.get("operator_confirmation") is not True:
        raise ValueError("operator_confirmation must be true")
    return data
