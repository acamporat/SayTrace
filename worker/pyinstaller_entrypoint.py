"""PyInstaller launcher that preserves package import semantics."""

from local_transcript_worker.__main__ import main

if __name__ == "__main__":
    raise SystemExit(main())
