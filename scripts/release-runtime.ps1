#requires -Version 7.2

[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet("Cpu", "Nvidia")]
    [string]$Variant,

    [Parameter(Mandatory)]
    [string]$WorkerBundle,

    [Parameter(Mandatory)]
    [string]$FfmpegDirectory,

    [Parameter(Mandatory)]
    [ValidatePattern("^https://")]
    [string]$FfmpegSourceUrl,

    [Parameter()]
    [string]$Version = "",

    [Parameter()]
    [string]$OutputDirectory = "",

    [Parameter()]
    [string]$MakeNsisPath = "",

    [Parameter()]
    [string]$PayloadOutputDirectory = "",

    [Parameter()]
    [switch]$PayloadOnly,

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
    [switch]$AllowDirty
)

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "release-common.ps1")

function Copy-RuntimePayloadItem {
    param(
        [Parameter(Mandatory)]
        [string]$Source,

        [Parameter(Mandatory)]
        [string]$Destination
    )

    if (Test-Path -LiteralPath $Destination -PathType Leaf) {
        $sourceHash = (Get-FileHash -LiteralPath $Source -Algorithm SHA256).Hash
        $destinationHash = (Get-FileHash -LiteralPath $Destination -Algorithm SHA256).Hash
        if ($sourceHash -ne $destinationHash) {
            throw "Payload collision at $Destination."
        }
        return
    }
    [IO.Directory]::CreateDirectory((Split-Path -Parent $Destination)) | Out-Null
    Copy-Item -LiteralPath $Source -Destination $Destination
}

function ConvertTo-NsisLiteral {
    param([Parameter(Mandatory)][string]$Value)
    return $Value.Replace("$", "$$").Replace('"', '$\"')
}

function Write-RuntimeFilesInclude {
    param(
        [Parameter(Mandatory)]
        [string]$PayloadRoot,

        [Parameter(Mandatory)]
        [string]$OutputPath
    )

    $files = Get-ChildItem -LiteralPath $PayloadRoot -File -Recurse -Force |
        Sort-Object FullName
    $lines = [Collections.Generic.List[string]]::new()
    $lines.Add("!macro InstallRuntimePayload")
    $lastDirectory = $null
    foreach ($file in $files) {
        $relative = [IO.Path]::GetRelativePath($PayloadRoot, $file.FullName)
        $relativeDirectory = Split-Path -Parent $relative
        if ($relativeDirectory -ne $lastDirectory) {
            $outPath = if ($relativeDirectory) {
                "`$INSTDIR\$relativeDirectory"
            } else {
                "`$INSTDIR"
            }
            $lines.Add("  SetOutPath `"$(ConvertTo-NsisLiteral $outPath)`"")
            $lastDirectory = $relativeDirectory
        }
        $source = ConvertTo-NsisLiteral $file.FullName
        $name = ConvertTo-NsisLiteral $file.Name
        $lines.Add("  File `"/oname=$name`" `"$source`"")
    }
    $lines.Add("!macroend")
    $lines.Add("")
    $lines.Add("!macro UninstallRuntimePayload")
    foreach ($file in ($files | Sort-Object FullName -Descending)) {
        $relative = [IO.Path]::GetRelativePath($PayloadRoot, $file.FullName)
        $target = ConvertTo-NsisLiteral "`$INSTDIR\$relative"
        $lines.Add("  Delete `"$target`"")
    }
    $directories = $files |
        ForEach-Object {
            $relative = [IO.Path]::GetRelativePath($PayloadRoot, $_.DirectoryName)
            if ($relative -ne ".") {
                $relative
            }
        } |
        Sort-Object @{ Expression = { ($_ -split "[\\/]").Count }; Descending = $true }, @{ Expression = { $_ }; Descending = $true } -Unique
    foreach ($directory in $directories) {
        $target = ConvertTo-NsisLiteral "`$INSTDIR\$directory"
        $lines.Add("  RMDir `"$target`"")
    }
    $lines.Add("!macroend")
    [IO.File]::WriteAllLines(
        [IO.Path]::GetFullPath($OutputPath),
        $lines,
        [Text.UTF8Encoding]::new($false)
    )
}

$repositoryRoot = Get-ReleaseRepositoryRoot
$tauriConfigPath = Join-Path $repositoryRoot "src-tauri\tauri.conf.json"
$tauriConfig = Get-Content -LiteralPath $tauriConfigPath -Raw | ConvertFrom-Json
if (-not $Version) {
    $Version = [string]$tauriConfig.version
}
if ($Version -notmatch "^(?<major>\d+)\.(?<minor>\d+)\.(?<patch>\d+)(?:[-+].*)?$") {
    throw "Runtime version must be semantic version text such as 1.2.3."
}
$versionMajor = $Matches.major
$versionMinor = $Matches.minor
$versionPatch = $Matches.patch
$workingTreeDirty = Test-ReleaseWorkingTreeDirty -RepositoryRoot $repositoryRoot
if ($Sign -and $workingTreeDirty) {
    throw "Signed production artifacts require a clean committed working tree; -AllowDirty is valid only for unsigned development builds."
}
if (-not $AllowDirty -and $workingTreeDirty) {
    throw "The working tree is not clean. Commit the release inputs or pass -AllowDirty for an explicitly non-production build."
}

$ffmpegSourceUri = [Uri]$FfmpegSourceUrl
$ffmpegSourceHost = $ffmpegSourceUri.DnsSafeHost.ToLowerInvariant()
$reservedSourceHosts = @(
    "example.com",
    "example.net",
    "example.org",
    "example.invalid"
)
$placeholderSourceHost = @(
    $reservedSourceHosts | Where-Object {
        $ffmpegSourceHost -eq $_ -or
        $ffmpegSourceHost.EndsWith(".$_", [StringComparison]::Ordinal)
    }
).Count -gt 0
if (
    -not $ffmpegSourceHost -or
    $placeholderSourceHost -or
    $ffmpegSourceUri.UserInfo -or
    $ffmpegSourceUri.Fragment -or
    $FfmpegSourceUrl -match "(?i)placeholder|replace[-_ ]?with|todo"
) {
    throw "FFmpeg source URL must identify the real supplier build/source page; placeholder, credential-bearing, and fragment URLs are rejected."
}

$workerRoot = [IO.Path]::GetFullPath($WorkerBundle)
if (-not (Test-Path -LiteralPath $workerRoot -PathType Container)) {
    throw "Worker bundle directory does not exist: $workerRoot"
}
$workerExecutable = Join-Path $workerRoot "local-transcript-worker.exe"
if (-not (Test-Path -LiteralPath $workerExecutable -PathType Leaf)) {
    throw "The worker bundle must contain local-transcript-worker.exe at its root."
}

$workerFiles = Get-ChildItem -LiteralPath $workerRoot -File -Recurse -Force
$gpuMarkers = $workerFiles | Where-Object {
    $_.Name -match "^(?:c10_cuda|torch_cuda|cudart|cublas|cudnn|nvrtc).*\.dll$"
}
if ($Variant -eq "Nvidia" -and -not $gpuMarkers) {
    throw "The NVIDIA runtime bundle contains no recognizable CUDA runtime libraries."
}
if ($Variant -eq "Cpu" -and $gpuMarkers) {
    throw "The CPU runtime bundle contains CUDA libraries and would be mislabeled."
}

$ffmpegRoot = [IO.Path]::GetFullPath($FfmpegDirectory)
if (-not (Test-Path -LiteralPath $ffmpegRoot -PathType Container)) {
    throw "FFmpeg directory does not exist: $ffmpegRoot"
}
$ffmpegCandidates = @(
    (Join-Path $ffmpegRoot "ffmpeg.exe"),
    (Join-Path $ffmpegRoot "bin\ffmpeg.exe")
)
$ffprobeCandidates = @(
    (Join-Path $ffmpegRoot "ffprobe.exe"),
    (Join-Path $ffmpegRoot "bin\ffprobe.exe")
)
$ffmpegExecutable = $ffmpegCandidates | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
$ffprobeExecutable = $ffprobeCandidates | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
if (-not $ffmpegExecutable -or -not $ffprobeExecutable) {
    throw "FFmpeg input must contain both ffmpeg.exe and ffprobe.exe, either at its root or in bin."
}
$ffmpegMetadata = Get-LgplFfmpegMetadata -FfmpegPath $ffmpegExecutable
$ffmpegBinDirectory = Split-Path -Parent $ffmpegExecutable

$makeNsis = $null
if (-not $PayloadOnly) {
    $makeNsis = Resolve-ReleaseMakeNsis -RequestedPath $MakeNsisPath
    if (-not $makeNsis) {
        throw "makensis.exe was not found. Install NSIS 3 or set LOCAL_TRANSCRIPT_MAKENSIS_PATH."
    }
}
if ($PayloadOnly -and -not $PayloadOutputDirectory) {
    throw "-PayloadOnly requires -PayloadOutputDirectory."
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
    $OutputDirectory = Join-Path $repositoryRoot "artifacts\runtime\$Version\$($Variant.ToLowerInvariant())"
}
$resolvedOutput = [IO.Path]::GetFullPath($OutputDirectory)
[IO.Directory]::CreateDirectory($resolvedOutput) | Out-Null

$unsignedSuffix = if ($Sign) { "" } else { "-UNSIGNED" }
$baseName = "Local-Transcript-Runtime-$Variant-$Version-windows-x64$unsignedSuffix"
$installerPath = Join-Path $resolvedOutput "$baseName.exe"
$payloadManifestOutput = Join-Path $resolvedOutput "$baseName.payload-manifest.json"
$releaseManifestOutput = Join-Path $resolvedOutput "$baseName.release-manifest.json"
$releaseArtifacts = if ($PayloadOnly) {
    @($payloadManifestOutput)
} else {
    @($installerPath, $payloadManifestOutput, $releaseManifestOutput)
}
foreach ($candidate in $releaseArtifacts) {
    if (Test-Path -LiteralPath $candidate) {
        throw "Refusing to overwrite existing release artifact: $candidate"
    }
}
$resolvedPayloadOutput = $null
if ($PayloadOutputDirectory) {
    $resolvedPayloadOutput = [IO.Path]::GetFullPath($PayloadOutputDirectory)
    if (Test-Path -LiteralPath $resolvedPayloadOutput) {
        throw "Refusing to overwrite existing runtime payload directory: $resolvedPayloadOutput"
    }
}

$temporaryRoot = Join-Path ([IO.Path]::GetTempPath()) "LocalTranscriptRuntime-$([Guid]::NewGuid().ToString('N'))"
$payloadRoot = Join-Path $temporaryRoot "payload"
$filesInclude = Join-Path $temporaryRoot "runtime-files.nsh"
[IO.Directory]::CreateDirectory($payloadRoot) | Out-Null

try {
    foreach ($item in Get-ChildItem -LiteralPath $workerRoot -Force) {
        Copy-Item -LiteralPath $item.FullName -Destination $payloadRoot -Recurse
    }

    Copy-RuntimePayloadItem `
        -Source $ffmpegExecutable `
        -Destination (Join-Path $payloadRoot "ffmpeg.exe")
    Copy-RuntimePayloadItem `
        -Source $ffprobeExecutable `
        -Destination (Join-Path $payloadRoot "ffprobe.exe")
    foreach ($library in Get-ChildItem -LiteralPath $ffmpegBinDirectory -File -Filter "*.dll" -ErrorAction SilentlyContinue) {
        Copy-RuntimePayloadItem `
            -Source $library.FullName `
            -Destination (Join-Path $payloadRoot $library.Name)
    }
    $runtimeSmokeScript = Join-Path $repositoryRoot "scripts\verify-worker-runtime.ps1"
    $stagedWorker = Join-Path $payloadRoot "local-transcript-worker.exe"
    $stagedFfmpeg = Join-Path $payloadRoot "ffmpeg.exe"
    $runtimeHealth = if ($Variant -eq "Nvidia") {
        & $runtimeSmokeScript `
            -WorkerExecutable $stagedWorker `
            -FfmpegExecutable $stagedFfmpeg `
            -RequireNvidia
    } else {
        & $runtimeSmokeScript `
            -WorkerExecutable $stagedWorker `
            -FfmpegExecutable $stagedFfmpeg
    }

    $ffmpegLicenseDirectory = Join-Path $payloadRoot "licenses\ffmpeg"
    foreach ($notice in Get-ChildItem -LiteralPath $ffmpegRoot -File -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match "^(?:COPYING|LICENSE|NOTICE|README)" }) {
        [IO.Directory]::CreateDirectory($ffmpegLicenseDirectory) | Out-Null
        Copy-RuntimePayloadItem `
            -Source $notice.FullName `
            -Destination (Join-Path $ffmpegLicenseDirectory $notice.Name)
    }
    [IO.Directory]::CreateDirectory($ffmpegLicenseDirectory) | Out-Null
    $buildNotice = @"
SayTrace FFmpeg runtime notice

Packaged build: $($ffmpegMetadata.versionLine)
License family: $($ffmpegMetadata.license)
Corresponding build/source information: $FfmpegSourceUrl

This runtime payload builder rejects FFmpeg configurations containing
--enable-gpl or --enable-nonfree. Preserve the supplier's complete license
and source materials when publishing the installer.
"@
    [IO.File]::WriteAllText(
        (Join-Path $ffmpegLicenseDirectory "BUILD-AND-SOURCE.txt"),
        "$buildNotice`n",
        [Text.UTF8Encoding]::new($false)
    )

    if ($Sign) {
        $signedEntrypoints = @(
            (Join-Path $payloadRoot "local-transcript-worker.exe"),
            (Join-Path $payloadRoot "ffmpeg.exe"),
            (Join-Path $payloadRoot "ffprobe.exe")
        )
        foreach ($file in $signedEntrypoints) {
            Invoke-ReleaseSigning `
                -FilePath $file `
                -SignToolPath $signing.signToolPath `
                -CertificateThumbprint $signing.thumbprint `
                -TimestampUrl $TimestampUrl
        }
    }

    $revisionsPath = Join-Path $repositoryRoot "packaging\revisions.json"
    $revisions = Get-Content -LiteralPath $revisionsPath -Raw | ConvertFrom-Json
    $modelManifestPath = Join-Path $repositoryRoot "worker\model-manifest.json"
    $payloadRecords = Get-ChildItem -LiteralPath $payloadRoot -File -Recurse -Force |
        Sort-Object FullName |
        ForEach-Object {
            Get-ReleaseFileRecord -FilePath $_.FullName -RelativeTo $payloadRoot
        }
    $payloadManifest = [ordered]@{
        schema_version             = 1
        product                    = "SayTrace Runtime"
        app_identifier             = [string]$revisions.app_identifier
        runtime_version            = $Version
        variant                    = $Variant.ToLowerInvariant()
        architecture               = "x64"
        install_scope              = "current_user"
        install_relative_path      = if ($PayloadOnly) {
            "application_resources/runtime"
        } else {
            "com.localtranscript.desktop/runtime"
        }
        worker_protocol_version    = [string]$revisions.worker_protocol_version
        pipeline_version           = [string]$revisions.pipeline_version
        source_revision            = Get-ReleaseRevision -RepositoryRoot $repositoryRoot
        generated_at_utc           = [DateTimeOffset]::UtcNow.ToString("O")
        component_authenticode     = [bool]$Sign
        authenticode_files         = if ($Sign) {
            @("local-transcript-worker.exe", "ffmpeg.exe", "ffprobe.exe")
        } else {
            @()
        }
        cpu_fallback_declared      = $true
        runtime_validation         = [ordered]@{
            worker_handshake = "passed_by_packager"
            torch_cuda       = [bool]$runtimeHealth.torch_cuda
            ctranslate2_cuda = [bool]$runtimeHealth.ctranslate2_cuda
            cpu_fallback     = "required_on_clean_test_machine"
            model_inference  = "not_performed_by_packager"
        }
        ffmpeg                     = [ordered]@{
            version_line = $ffmpegMetadata.versionLine
            license      = $ffmpegMetadata.license
            source_url   = $FfmpegSourceUrl
        }
        model_manifest             = [ordered]@{
            bundled       = $false
            sha256        = (Get-FileHash -LiteralPath $modelManifestPath -Algorithm SHA256).Hash.ToLowerInvariant()
            revisions     = $revisions.models
        }
        payload                    = @($payloadRecords)
    }
    $embeddedManifest = Join-Path $payloadRoot "runtime-manifest.json"
    Write-ReleaseJson -Value $payloadManifest -Path $embeddedManifest
    Copy-Item -LiteralPath $embeddedManifest -Destination $payloadManifestOutput

    if ($resolvedPayloadOutput) {
        [IO.Directory]::CreateDirectory($resolvedPayloadOutput) | Out-Null
        foreach ($item in Get-ChildItem -LiteralPath $payloadRoot -Force) {
            Copy-Item -LiteralPath $item.FullName -Destination $resolvedPayloadOutput -Recurse
        }
    }

    if (-not $PayloadOnly) {
        Write-RuntimeFilesInclude -PayloadRoot $payloadRoot -OutputPath $filesInclude
        $versionQuad = "$versionMajor.$versionMinor.$versionPatch.0"
        $installerIcon = Join-Path $repositoryRoot "src-tauri\icons\icon.ico"
        $nsisScript = Join-Path $repositoryRoot "packaging\runtime\runtime-installer.nsi"
        $nsisArguments = @(
            "/V2",
            "/DFILES_INCLUDE=$filesInclude",
            "/DOUTPUT_FILE=$installerPath",
            "/DINSTALLER_ICON=$installerIcon",
            "/DRUNTIME_VARIANT=$Variant",
            "/DRUNTIME_VERSION=$Version",
            "/DVERSION_QUAD=$versionQuad",
            $nsisScript
        )
        Invoke-ReleaseNative `
            -FilePath $makeNsis `
            -ArgumentList $nsisArguments `
            -FailureMessage "NSIS could not build the runtime installer."

        if ($Sign) {
            Invoke-ReleaseSigning `
                -FilePath $installerPath `
                -SignToolPath $signing.signToolPath `
                -CertificateThumbprint $signing.thumbprint `
                -TimestampUrl $TimestampUrl
        }

        $releaseManifest = [ordered]@{
            schema_version          = 1
            artifact_kind           = "runtime_installer"
            product                 = "SayTrace Runtime"
            version                 = $Version
            variant                 = $Variant.ToLowerInvariant()
            architecture            = "x64"
            source_revision         = $payloadManifest.source_revision
            signed                  = [bool]$Sign
            install_scope           = "current_user"
            installer               = Get-ReleaseFileRecord -FilePath $installerPath -RelativeTo $resolvedOutput
            payload_manifest        = Get-ReleaseFileRecord -FilePath $payloadManifestOutput -RelativeTo $resolvedOutput
            models_bundled          = $false
            update_policy           = "user_initiated_app_closed"
        }
        Write-ReleaseJson -Value $releaseManifest -Path $releaseManifestOutput
        Write-Host "Runtime installer: $installerPath"
        Write-Host "Release manifest: $releaseManifestOutput"
    }

    Write-Host "Payload manifest: $payloadManifestOutput"
    if ($resolvedPayloadOutput) {
        Write-Host "Runtime payload directory: $resolvedPayloadOutput"
    }
    if (-not $Sign) {
        Write-Warning "The runtime payload is an explicitly UNSIGNED development artifact."
    }
} finally {
    $resolvedTemporary = [IO.Path]::GetFullPath($temporaryRoot)
    $systemTemporary = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    if (
        $resolvedTemporary.StartsWith($systemTemporary, [StringComparison]::OrdinalIgnoreCase) -and
        (Split-Path -Leaf $resolvedTemporary).StartsWith("LocalTranscriptRuntime-", [StringComparison]::Ordinal)
    ) {
        Remove-Item -LiteralPath $resolvedTemporary -Recurse -Force -ErrorAction SilentlyContinue
    }
}
