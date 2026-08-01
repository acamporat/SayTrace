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
        uv sync --project "$workspace\worker" --group dev
        uv run --project "$workspace\worker" ruff check "$workspace\worker"
        uv run --project "$workspace\worker" mypy
        uv run --project "$workspace\worker" pytest
    }
}
finally {
    Pop-Location
}
