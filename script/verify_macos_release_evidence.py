#!/usr/bin/env python3
"""Validate the human-observed physical-Mac release acceptance record."""

from __future__ import annotations

import argparse
from pathlib import Path

from macos_release_evidence import (  # noqa: F401
    REQUIRED_CHECKS,
    installer_record,
    parse_timestamp,
    sha256,
    validate_evidence,
)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence", required=True, type=Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--installer", required=True, type=Path)
    args = parser.parse_args()
    validate_evidence(
        args.evidence,
        version=args.version,
        source_revision=args.source_revision,
        installer=args.installer,
    )
    print("Verified physical-Mac release acceptance evidence.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
