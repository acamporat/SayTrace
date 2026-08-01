#requires -Version 7.2

[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$BundleRoot,

    [Parameter(Mandatory)]
    [string]$SevenZipPath,

    [Parameter(Mandatory)]
    [string]$OutputFile
)

$ErrorActionPreference = "Stop"
$repositoryRoot = Split-Path -Parent $PSScriptRoot
$bundle = [IO.Path]::GetFullPath($BundleRoot)
$sevenZip = [IO.Path]::GetFullPath($SevenZipPath)
$output = [IO.Path]::GetFullPath($OutputFile)
if (-not (Test-Path -LiteralPath $bundle -PathType Container)) {
    throw "Setup bundle root does not exist: $bundle"
}
if (-not (Test-Path -LiteralPath $sevenZip -PathType Leaf)) {
    throw "7-Zip executable does not exist: $sevenZip"
}
if (Test-Path -LiteralPath $output) {
    throw "Refusing to overwrite setup bundle: $output"
}
foreach ($required in @(
    "Local-Transcript-App-Installer.exe",
    "install-runtime.ps1",
    "runtime\runtime-manifest.json",
    "licenses\7zip\License.txt"
)) {
    if (-not (Test-Path -LiteralPath (Join-Path $bundle $required) -PathType Leaf)) {
        throw "Setup bundle input is missing $required."
    }
}

$bootstrapManifest = Join-Path $repositoryRoot "packaging\setup-bootstrap\Cargo.toml"
$bootstrapLock = Join-Path $repositoryRoot "packaging\setup-bootstrap\Cargo.lock"
if (-not (Test-Path -LiteralPath $bootstrapLock -PathType Leaf)) {
    throw "The setup bootstrap Cargo.lock is missing."
}
& cargo build --manifest-path $bootstrapManifest --release --locked
if ($LASTEXITCODE -ne 0) {
    throw "The SayTrace setup bootstrap could not be built."
}
$bootstrap = Join-Path $repositoryRoot "packaging\setup-bootstrap\target\release\local-transcript-setup-bootstrap.exe"
if (-not (Test-Path -LiteralPath $bootstrap -PathType Leaf)) {
    throw "The setup bootstrap output is missing."
}

$temporaryRoot = Join-Path ([IO.Path]::GetTempPath()) "LocalTranscriptSetupBundle-$([Guid]::NewGuid().ToString('N'))"
[IO.Directory]::CreateDirectory($temporaryRoot) | Out-Null
$archive = Join-Path $temporaryRoot "payload.7z"
try {
    Push-Location $bundle
    try {
        & $sevenZip a -t7z $archive ".\*" -mx=5 -m0=lzma2 -ms=on -mmt=on
        if ($LASTEXITCODE -ne 0) {
            throw "7-Zip could not create the SayTrace setup payload."
        }
    } finally {
        Pop-Location
    }

    $bootstrapInfo = Get-Item -LiteralPath $bootstrap
    $extractorInfo = Get-Item -LiteralPath $sevenZip
    $archiveInfo = Get-Item -LiteralPath $archive
    $extractorHash = [Convert]::FromHexString(
        (Get-FileHash -LiteralPath $sevenZip -Algorithm SHA256).Hash
    )
    $archiveHash = [Convert]::FromHexString(
        (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash
    )
    $magic = [Text.Encoding]::ASCII.GetBytes("LTRSFXBUNDLE0001")
    $outputDirectory = Split-Path -Parent $output
    [IO.Directory]::CreateDirectory($outputDirectory) | Out-Null
    $destination = [IO.File]::Open(
        $output,
        [IO.FileMode]::CreateNew,
        [IO.FileAccess]::Write,
        [IO.FileShare]::None
    )
    try {
        foreach ($sourcePath in @($bootstrap, $sevenZip, $archive)) {
            $source = [IO.File]::OpenRead($sourcePath)
            try {
                $source.CopyTo($destination, 4MB)
            } finally {
                $source.Dispose()
            }
        }
        $extractorOffset = [uint64]$bootstrapInfo.Length
        $archiveOffset = $extractorOffset + [uint64]$extractorInfo.Length
        $destination.Write($magic)
        $destination.Write([BitConverter]::GetBytes($extractorOffset))
        $destination.Write([BitConverter]::GetBytes([uint64]$extractorInfo.Length))
        $destination.Write([BitConverter]::GetBytes($archiveOffset))
        $destination.Write([BitConverter]::GetBytes([uint64]$archiveInfo.Length))
        $destination.Write($extractorHash)
        $destination.Write($archiveHash)
        $destination.Flush($true)
    } finally {
        $destination.Dispose()
    }
    Write-Host "Self-extracting setup: $output"
    Write-Host "Compressed payload: $archive"
} catch {
    if (Test-Path -LiteralPath $output) {
        Remove-Item -LiteralPath $output -Force
    }
    throw
} finally {
    $resolvedTemporary = [IO.Path]::GetFullPath($temporaryRoot)
    $systemTemporary = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    if (
        $resolvedTemporary.StartsWith($systemTemporary, [StringComparison]::OrdinalIgnoreCase) -and
        (Split-Path -Leaf $resolvedTemporary).StartsWith(
            "LocalTranscriptSetupBundle-",
            [StringComparison]::Ordinal
        )
    ) {
        Remove-Item -LiteralPath $resolvedTemporary -Recurse -Force -ErrorAction SilentlyContinue
    }
}
