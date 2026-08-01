#requires -Version 7.2

[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$RuntimePayloadDirectory,

    [Parameter()]
    [string]$Version = "",

    [Parameter()]
    [string]$OutputDirectory = "",

    [Parameter()]
    [switch]$Sign,

    [Parameter()]
    [string]$SignToolPath = $env:LOCAL_TRANSCRIPT_SIGNTOOL_PATH,

    [Parameter()]
    [string]$CertificateThumbprint = $env:LOCAL_TRANSCRIPT_SIGN_CERT_THUMBPRINT,

    [Parameter()]
    [string]$TimestampUrl = $(if ($env:LOCAL_TRANSCRIPT_TIMESTAMP_URL) {
        $env:LOCAL_TRANSCRIPT_TIMESTAMP_URL
    } else {
        "http://timestamp.digicert.com"
    }),

    [Parameter()]
    [string]$SevenZipPath = $env:LOCAL_TRANSCRIPT_7ZIP_PATH,

    [Parameter()]
    [string]$SevenZipLicensePath = $env:LOCAL_TRANSCRIPT_7ZIP_LICENSE_PATH,

    [Parameter()]
    [switch]$SkipTests,

    [Parameter()]
    [switch]$AllowDirty
)

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "release-common.ps1")

$repositoryRoot = Get-ReleaseRepositoryRoot
$tauriConfigPath = Join-Path $repositoryRoot "src-tauri\tauri.conf.json"
$packagePath = Join-Path $repositoryRoot "package.json"
$cargoPath = Join-Path $repositoryRoot "src-tauri\Cargo.toml"
$releaseConfigPath = Join-Path $repositoryRoot "packaging\tauri.release.conf.json"
$bundledRuntimeHooksPath = Join-Path $repositoryRoot "packaging\app-installer-bundled-runtime-hooks.nsh"
$runtimeInstallScriptPath = Join-Path $repositoryRoot "packaging\install-runtime.ps1"
$setupBundleScriptPath = Join-Path $repositoryRoot "scripts\build-setup-bundle.ps1"
$runtimeRoot = [IO.Path]::GetFullPath($RuntimePayloadDirectory)
if (-not (Test-Path -LiteralPath $runtimeRoot -PathType Container)) {
    throw "Runtime payload directory does not exist: $runtimeRoot"
}
foreach ($requiredRuntimeFile in @(
    "local-transcript-worker.exe",
    "ffmpeg.exe",
    "ffprobe.exe",
    "runtime-manifest.json"
)) {
    if (-not (Test-Path -LiteralPath (Join-Path $runtimeRoot $requiredRuntimeFile) -PathType Leaf)) {
        throw "Runtime payload is missing $requiredRuntimeFile."
    }
}
$runtimeManifestPath = Join-Path $runtimeRoot "runtime-manifest.json"
$runtimeManifest = Get-Content -LiteralPath $runtimeManifestPath -Raw | ConvertFrom-Json
if (
    [string]$runtimeManifest.product -ne "SayTrace Runtime" -or
    [string]$runtimeManifest.architecture -ne "x64" -or
    [string]$runtimeManifest.variant -notin @("nvidia", "cpu")
) {
    throw "Runtime payload manifest does not identify a compatible SayTrace Windows x64 runtime."
}

$tauriConfig = Get-Content -LiteralPath $tauriConfigPath -Raw | ConvertFrom-Json
$package = Get-Content -LiteralPath $packagePath -Raw | ConvertFrom-Json
$cargoVersionLine = Select-String -LiteralPath $cargoPath -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1
if (-not $cargoVersionLine) {
    throw "Could not read the Rust package version."
}
$cargoVersion = $cargoVersionLine.Matches[0].Groups[1].Value
if (-not $Version) {
    $Version = [string]$tauriConfig.version
}
if (
    $Version -ne [string]$tauriConfig.version -or
    $Version -ne [string]$package.version -or
    $Version -ne $cargoVersion
) {
    throw "Version mismatch: requested=$Version, tauri=$($tauriConfig.version), npm=$($package.version), cargo=$cargoVersion."
}
if (
    [string]$runtimeManifest.runtime_version -ne $Version -or
    [string]$runtimeManifest.app_identifier -ne [string]$tauriConfig.identifier
) {
    throw "Runtime payload version or application identity does not match this SayTrace release."
}
if ($Version -notmatch "^\d+\.\d+\.\d+(?:[-+].*)?$") {
    throw "App version must be semantic version text such as 1.2.3."
}

$configuredTargets = @($tauriConfig.bundle.targets)
if ("nsis" -notin $configuredTargets) {
    throw "The checked-in Tauri configuration does not enable the NSIS bundle target."
}
if ([string]$tauriConfig.bundle.windows.nsis.installMode -ne "currentUser") {
    throw "The checked-in Tauri NSIS installer is not configured for currentUser installation."
}
$workingTreeDirty = Test-ReleaseWorkingTreeDirty -RepositoryRoot $repositoryRoot
if ($Sign -and $workingTreeDirty) {
    throw "Signed production artifacts require a clean committed working tree; -AllowDirty is valid only for unsigned development builds."
}
if ($Sign -and -not [bool]$runtimeManifest.component_authenticode) {
    throw "Signed app releases require a runtime payload whose worker and media entrypoints were signed before manifest generation."
}
if (-not $AllowDirty -and $workingTreeDirty) {
    throw "The working tree is not clean. Commit the release inputs or pass -AllowDirty for an explicitly non-production build."
}

$sevenZipCandidates = @(
    $SevenZipPath,
    (Join-Path $repositoryRoot "artifacts\build-inputs\7zip-26.02\extra\x64\7za.exe")
) | Where-Object { $_ }
$resolvedSevenZip = $sevenZipCandidates |
    ForEach-Object { [IO.Path]::GetFullPath($_) } |
    Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } |
    Select-Object -First 1
if (-not $resolvedSevenZip) {
    throw "7za.exe is required to build the one-file setup. Set -SevenZipPath or LOCAL_TRANSCRIPT_7ZIP_PATH."
}
$sevenZipLicenseCandidates = @(
    $SevenZipLicensePath,
    (Join-Path (Split-Path -Parent (Split-Path -Parent $resolvedSevenZip)) "License.txt"),
    (Join-Path (Split-Path -Parent $resolvedSevenZip) "License.txt")
) | Where-Object { $_ }
$resolvedSevenZipLicense = $sevenZipLicenseCandidates |
    ForEach-Object { [IO.Path]::GetFullPath($_) } |
    Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } |
    Select-Object -First 1
if (-not $resolvedSevenZipLicense) {
    throw "The 7-Zip license file is required beside the one-file setup builder."
}

$signing = $null
if ($Sign) {
    if (-not $CertificateThumbprint) {
        throw "Signing was requested, but no certificate thumbprint was supplied."
    }
    $signing = Assert-ReleaseSigningReady `
        -SignToolPath $SignToolPath `
        -CertificateThumbprint $CertificateThumbprint
}

if (-not $OutputDirectory) {
    $OutputDirectory = Join-Path $repositoryRoot "artifacts\app\$Version"
}
$resolvedOutput = [IO.Path]::GetFullPath($OutputDirectory)
[IO.Directory]::CreateDirectory($resolvedOutput) | Out-Null
$unsignedSuffix = if ($Sign) { "" } else { "-UNSIGNED" }
$runtimeVariantLabel = (Get-Culture).TextInfo.ToTitleCase(
    ([string]$runtimeManifest.variant).ToLowerInvariant()
)
$outputInstaller = Join-Path $resolvedOutput "Local-Transcript-$Version-$runtimeVariantLabel-windows-x64-setup$unsignedSuffix.exe"
$outputManifest = Join-Path $resolvedOutput "Local-Transcript-$Version-$runtimeVariantLabel-windows-x64$unsignedSuffix.release-manifest.json"
foreach ($candidate in @($outputInstaller, $outputManifest)) {
    if (Test-Path -LiteralPath $candidate) {
        throw "Refusing to overwrite existing release artifact: $candidate"
    }
}

$temporaryRoot = Join-Path ([IO.Path]::GetTempPath()) "LocalTranscriptAppRelease-$([Guid]::NewGuid().ToString('N'))"
[IO.Directory]::CreateDirectory($temporaryRoot) | Out-Null
$generatedConfigPath = Join-Path $temporaryRoot "tauri.release.generated.json"

$previousSignTool = $env:LOCAL_TRANSCRIPT_SIGNTOOL_PATH
$previousThumbprint = $env:LOCAL_TRANSCRIPT_SIGN_CERT_THUMBPRINT
$previousTimestamp = $env:LOCAL_TRANSCRIPT_TIMESTAMP_URL
try {
    $releaseConfig = Get-Content -LiteralPath $releaseConfigPath -Raw | ConvertFrom-Json
    $releaseConfig.bundle.windows.nsis |
        Add-Member -NotePropertyName "installerHooks" -NotePropertyValue $bundledRuntimeHooksPath -Force
    if ($Sign) {
        $powerShellExecutable = (Get-Process -Id $PID).Path
        $signingCommand = [ordered]@{
            cmd  = $powerShellExecutable
            args = @(
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-File",
                (Join-Path $PSScriptRoot "release-sign.ps1"),
                "-FilePath",
                "%1"
            )
        }
        $releaseConfig.bundle.windows |
            Add-Member -NotePropertyName "signCommand" -NotePropertyValue $signingCommand -Force
        $env:LOCAL_TRANSCRIPT_SIGNTOOL_PATH = $signing.signToolPath
        $env:LOCAL_TRANSCRIPT_SIGN_CERT_THUMBPRINT = $signing.thumbprint
        $env:LOCAL_TRANSCRIPT_TIMESTAMP_URL = $TimestampUrl
    }
    Write-ReleaseJson -Value $releaseConfig -Path $generatedConfigPath

    $npm = Get-Command npm.cmd -ErrorAction Stop
    if (-not $SkipTests) {
        Invoke-ReleaseNative `
            -FilePath $npm.Source `
            -ArgumentList @("test") `
            -FailureMessage "Frontend tests failed."
    }

    $buildStarted = [DateTime]::UtcNow.AddSeconds(-5)
    Invoke-ReleaseNative `
        -FilePath $npm.Source `
        -ArgumentList @("run", "tauri", "--", "build", "--config", $generatedConfigPath) `
        -FailureMessage "Tauri could not build the per-user NSIS installer."

    $nsisOutput = Join-Path $repositoryRoot "src-tauri\target\release\bundle\nsis"
    $installers = Get-ChildItem -LiteralPath $nsisOutput -File -Filter "*.exe" -ErrorAction Stop |
        Where-Object { $_.LastWriteTimeUtc -ge $buildStarted } |
        Sort-Object LastWriteTimeUtc -Descending
    if (@($installers).Count -ne 1) {
        $paths = ($installers.FullName -join ", ")
        throw "Expected exactly one newly built NSIS installer; found $(@($installers).Count). $paths"
    }
    $bundleRoot = Join-Path $temporaryRoot "setup-payload"
    $bundleRuntime = Join-Path $bundleRoot "runtime"
    [IO.Directory]::CreateDirectory($bundleRoot) | Out-Null
    Copy-Item `
        -LiteralPath $installers[0].FullName `
        -Destination (Join-Path $bundleRoot "Local-Transcript-App-Installer.exe")
    Copy-Item -LiteralPath $runtimeRoot -Destination $bundleRuntime -Recurse
    Copy-Item `
        -LiteralPath $runtimeInstallScriptPath `
        -Destination (Join-Path $bundleRoot "install-runtime.ps1")
    $sevenZipLicenseOutput = Join-Path $bundleRoot "licenses\7zip\License.txt"
    [IO.Directory]::CreateDirectory((Split-Path -Parent $sevenZipLicenseOutput)) | Out-Null
    Copy-Item -LiteralPath $resolvedSevenZipLicense -Destination $sevenZipLicenseOutput
    & $setupBundleScriptPath `
        -BundleRoot $bundleRoot `
        -SevenZipPath $resolvedSevenZip `
        -OutputFile $outputInstaller

    if ($Sign) {
        Invoke-ReleaseSigning `
            -FilePath $outputInstaller `
            -SignToolPath $signing.signToolPath `
            -CertificateThumbprint $signing.thumbprint `
            -TimestampUrl $TimestampUrl
        $signature = Get-AuthenticodeSignature -LiteralPath $outputInstaller
        if ($signature.Status -ne "Valid") {
            throw "The self-extracting setup does not have a valid Authenticode signature: $($signature.Status)."
        }
    }

    $manifest = [ordered]@{
        schema_version          = 1
        artifact_kind           = "app_installer"
        product                 = "SayTrace"
        app_identifier          = [string]$tauriConfig.identifier
        version                 = $Version
        architecture            = "x64"
        source_revision         = Get-ReleaseRevision -RepositoryRoot $repositoryRoot
        generated_at_utc        = [DateTimeOffset]::UtcNow.ToString("O")
        signed                  = [bool]$Sign
        install_scope           = "current_user"
        installer               = Get-ReleaseFileRecord -FilePath $outputInstaller -RelativeTo $resolvedOutput
        package_lock_sha256     = (Get-FileHash -LiteralPath (Join-Path $repositoryRoot "package-lock.json") -Algorithm SHA256).Hash.ToLowerInvariant()
        cargo_lock_sha256       = (Get-FileHash -LiteralPath (Join-Path $repositoryRoot "src-tauri\Cargo.lock") -Algorithm SHA256).Hash.ToLowerInvariant()
        tauri_config_sha256     = (Get-FileHash -LiteralPath $tauriConfigPath -Algorithm SHA256).Hash.ToLowerInvariant()
        runtime_bundled         = $true
        runtime_variant         = [string]$runtimeManifest.variant
        runtime_version         = [string]$runtimeManifest.runtime_version
        runtime_manifest_sha256 = (Get-FileHash -LiteralPath $runtimeManifestPath -Algorithm SHA256).Hash.ToLowerInvariant()
        runtime_container       = "sha256_verified_self_extracting_7z"
        inner_installer         = "tauri_nsis_current_user"
        silent_install_argument = "/S"
        seven_zip_version       = (Get-Item -LiteralPath $resolvedSevenZip).VersionInfo.FileVersion
        seven_zip_sha256        = (Get-FileHash -LiteralPath $resolvedSevenZip -Algorithm SHA256).Hash.ToLowerInvariant()
        models_bundled          = $false
        update_policy           = "user_initiated_combined_installer_process_guarded"
    }
    Write-ReleaseJson -Value $manifest -Path $outputManifest

    Write-Host "App installer: $outputInstaller"
    Write-Host "Release manifest: $outputManifest"
    if (-not $Sign) {
        Write-Warning "The app installer is an explicitly UNSIGNED development artifact."
    }
} finally {
    if ($null -eq $previousSignTool) {
        Remove-Item Env:\LOCAL_TRANSCRIPT_SIGNTOOL_PATH -ErrorAction SilentlyContinue
    } else {
        $env:LOCAL_TRANSCRIPT_SIGNTOOL_PATH = $previousSignTool
    }
    if ($null -eq $previousThumbprint) {
        Remove-Item Env:\LOCAL_TRANSCRIPT_SIGN_CERT_THUMBPRINT -ErrorAction SilentlyContinue
    } else {
        $env:LOCAL_TRANSCRIPT_SIGN_CERT_THUMBPRINT = $previousThumbprint
    }
    if ($null -eq $previousTimestamp) {
        Remove-Item Env:\LOCAL_TRANSCRIPT_TIMESTAMP_URL -ErrorAction SilentlyContinue
    } else {
        $env:LOCAL_TRANSCRIPT_TIMESTAMP_URL = $previousTimestamp
    }

    $resolvedTemporary = [IO.Path]::GetFullPath($temporaryRoot)
    $systemTemporary = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    if (
        $resolvedTemporary.StartsWith($systemTemporary, [StringComparison]::OrdinalIgnoreCase) -and
        (Split-Path -Leaf $resolvedTemporary).StartsWith("LocalTranscriptAppRelease-", [StringComparison]::Ordinal)
    ) {
        Remove-Item -LiteralPath $resolvedTemporary -Recurse -Force -ErrorAction SilentlyContinue
    }
}
