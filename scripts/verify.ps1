[CmdletBinding()]
param(
    [switch]$SkipInstall,
    [switch]$SkipRust,
    [switch]$SkipWorker
)

$ErrorActionPreference = 'Stop'
$workspace = Split-Path -Parent $PSScriptRoot

Push-Location $workspace
try {
    if (-not $SkipInstall) {
        npm ci
    }
    npm test
    npm run build

    if (-not $SkipRust) {
        cargo fmt --manifest-path "$workspace\src-tauri\Cargo.toml" --check
        cargo test --manifest-path "$workspace\src-tauri\Cargo.toml"
    }

    if (-not $SkipWorker) {
        # The desktop development shell and the worker unit tests intentionally
        # share worker/.venv. Keep an already-installed `ml` extra intact while
        # adding the lightweight verification tools; an exact sync here used to
        # silently remove faster-whisper before the next desktop run.
        uv sync --project "$workspace\worker" --group dev --inexact
        uv run --project "$workspace\worker" --no-sync ruff check "$workspace\worker"
        uv run --project "$workspace\worker" --no-sync mypy
        uv run --project "$workspace\worker" --no-sync pytest
    }
}
finally {
    Pop-Location
}
