#requires -Version 5.1

[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$SourceRuntime,

    [Parameter(Mandatory)]
    [string]$DestinationRuntime
)

$ErrorActionPreference = "Stop"

function Get-LocalTranscriptSha256 {
    param([Parameter(Mandatory)][string]$FilePath)

    $algorithm = [Security.Cryptography.SHA256]::Create()
    $stream = [IO.File]::Open(
        $FilePath,
        [IO.FileMode]::Open,
        [IO.FileAccess]::Read,
        [IO.FileShare]::Read
    )
    try {
        $bytes = $algorithm.ComputeHash($stream)
        return ([BitConverter]::ToString($bytes)).Replace("-", "").ToLowerInvariant()
    } finally {
        $stream.Dispose()
        $algorithm.Dispose()
    }
}

$source = [IO.Path]::GetFullPath($SourceRuntime)
$destination = [IO.Path]::GetFullPath($DestinationRuntime)
$destinationParent = Split-Path -Parent $destination
if (
    -not (Test-Path -LiteralPath $source -PathType Container) -or
    -not $destinationParent
) {
    throw "The bundled processing runtime or installation directory is unavailable."
}
foreach ($required in @(
    "local-transcript-worker.exe",
    "ffmpeg.exe",
    "ffprobe.exe",
    "runtime-manifest.json"
)) {
    if (-not (Test-Path -LiteralPath (Join-Path $source $required) -PathType Leaf)) {
        throw "The bundled processing runtime is missing $required."
    }
}
$manifest = Get-Content -LiteralPath (Join-Path $source "runtime-manifest.json") -Raw |
    ConvertFrom-Json
if (
    [string]$manifest.product -ne "SayTrace Runtime" -or
    [string]$manifest.app_identifier -ne "com.localtranscript.desktop" -or
    [string]$manifest.architecture -ne "x64"
) {
    throw "The bundled processing runtime does not match SayTrace."
}
foreach ($record in $manifest.payload) {
    $relative = [string]$record.path
    if (
        [IO.Path]::IsPathRooted($relative) -or
        $relative -eq ".." -or
        $relative.Replace("\", "/").StartsWith("../", [StringComparison]::Ordinal)
    ) {
        throw "The runtime manifest contains an unsafe path."
    }
    $candidate = [IO.Path]::GetFullPath((Join-Path $source $relative))
    $sourcePrefix = "$($source.TrimEnd('\','/'))$([IO.Path]::DirectorySeparatorChar)"
    if (-not $candidate.StartsWith($sourcePrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "The runtime manifest path escapes its payload."
    }
    $item = Get-Item -LiteralPath $candidate -ErrorAction Stop
    if ($item.Length -ne [long]$record.size) {
        throw "The bundled runtime failed its size check: $relative"
    }
    $hash = Get-LocalTranscriptSha256 -FilePath $candidate
    if ($hash -ne ([string]$record.sha256).ToLowerInvariant()) {
        throw "The bundled runtime failed its SHA-256 check: $relative"
    }
}

[IO.Directory]::CreateDirectory($destinationParent) | Out-Null
$staging = "$destination.installing-$PID"
$backup = "$destination.previous-$PID"
foreach ($temporary in @($staging, $backup)) {
    if (Test-Path -LiteralPath $temporary) {
        Remove-Item -LiteralPath $temporary -Recurse -Force
    }
}
try {
    Copy-Item -LiteralPath $source -Destination $staging -Recurse
    foreach ($required in @(
        "local-transcript-worker.exe",
        "ffmpeg.exe",
        "ffprobe.exe",
        "runtime-manifest.json"
    )) {
        if (-not (Test-Path -LiteralPath (Join-Path $staging $required) -PathType Leaf)) {
            throw "The processing runtime copy is incomplete."
        }
    }
    if (Test-Path -LiteralPath $destination) {
        Move-Item -LiteralPath $destination -Destination $backup
    }
    try {
        Move-Item -LiteralPath $staging -Destination $destination
    } catch {
        if (Test-Path -LiteralPath $backup) {
            Move-Item -LiteralPath $backup -Destination $destination
        }
        throw
    }
    if (Test-Path -LiteralPath $backup) {
        Remove-Item -LiteralPath $backup -Recurse -Force
    }
} finally {
    foreach ($temporary in @($staging, $backup)) {
        if (Test-Path -LiteralPath $temporary) {
            Remove-Item -LiteralPath $temporary -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
}
