#requires -Version 7.2

[CmdletBinding()]
param(
    [Parameter()]
    [string]$ManifestPath = "",

    [Parameter()]
    [string]$PayloadRoot = "",

    [Parameter()]
    [switch]$RequireSignature,

    [Parameter()]
    [string]$SignToolPath = ""
)

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "release-common.ps1")

function Resolve-VerifiedManifestPath {
    param(
        [Parameter(Mandatory)]
        [string]$Root,

        [Parameter(Mandatory)]
        [string]$RelativePath
    )

    if (
        [IO.Path]::IsPathRooted($RelativePath) -or
        $RelativePath -eq ".." -or
        $RelativePath.Replace("\", "/").StartsWith("../", [StringComparison]::Ordinal)
    ) {
        throw "Manifest contains an unsafe payload path: $RelativePath"
    }
    $resolvedRoot = [IO.Path]::GetFullPath($Root).TrimEnd("\", "/")
    $resolved = [IO.Path]::GetFullPath((Join-Path $resolvedRoot $RelativePath))
    $prefix = "$resolvedRoot$([IO.Path]::DirectorySeparatorChar)"
    if (-not $resolved.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Manifest path escapes its payload root: $RelativePath"
    }
    return $resolved
}

function Assert-ReleaseRecord {
    param(
        [Parameter(Mandatory)]
        [object]$Record,

        [Parameter(Mandatory)]
        [string]$Root
    )

    $path = Resolve-VerifiedManifestPath -Root $Root -RelativePath ([string]$Record.path)
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Manifest file is missing: $path"
    }
    $item = Get-Item -LiteralPath $path
    if ($item.Length -ne [long]$Record.size) {
        throw "Size mismatch for $($Record.path): expected $($Record.size), found $($item.Length)."
    }
    $hash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($hash -ne ([string]$Record.sha256).ToLowerInvariant()) {
        throw "SHA-256 mismatch for $($Record.path)."
    }
    return $path
}

function Assert-SourceReleaseContracts {
    param([Parameter(Mandatory)][string]$RepositoryRoot)

    $tauriConfigPath = Join-Path $RepositoryRoot "src-tauri\tauri.conf.json"
    $packagePath = Join-Path $RepositoryRoot "package.json"
    $cargoPath = Join-Path $RepositoryRoot "src-tauri\Cargo.toml"
    $libPath = Join-Path $RepositoryRoot "src-tauri\src\lib.rs"
    $workerHostPath = Join-Path $RepositoryRoot "src-tauri\src\worker.rs"
    $appInstallerHooksPath = Join-Path $RepositoryRoot "packaging\app-installer-hooks.nsh"
    $runtimeInstallerPath = Join-Path $RepositoryRoot "packaging\runtime\runtime-installer.nsi"
    $appReleaseScriptPath = Join-Path $RepositoryRoot "scripts\release-app.ps1"
    $runtimeReleaseScriptPath = Join-Path $RepositoryRoot "scripts\release-runtime.ps1"
    $workerBuildScriptPath = Join-Path $RepositoryRoot "worker\scripts\build.ps1"
    $workerSpecPath = Join-Path $RepositoryRoot "worker\local_transcript_worker.spec"
    $workerEntrypointPath = Join-Path $RepositoryRoot "worker\pyinstaller_entrypoint.py"
    $bundledRuntimeHooksPath = Join-Path $RepositoryRoot "packaging\app-installer-bundled-runtime-hooks.nsh"
    $bundledRuntimeInstallPath = Join-Path $RepositoryRoot "packaging\install-runtime.ps1"
    $setupBundleScriptPath = Join-Path $RepositoryRoot "scripts\build-setup-bundle.ps1"
    $modelManifestPath = Join-Path $RepositoryRoot "worker\model-manifest.json"
    $revisionsPath = Join-Path $RepositoryRoot "packaging\revisions.json"

    $tauriConfig = Get-Content -LiteralPath $tauriConfigPath -Raw | ConvertFrom-Json
    $package = Get-Content -LiteralPath $packagePath -Raw | ConvertFrom-Json
    $cargoVersionLine = Select-String -LiteralPath $cargoPath -Pattern '^version\s*=\s*"([^"]+)"' |
        Select-Object -First 1
    $cargoVersion = $cargoVersionLine.Matches[0].Groups[1].Value
    if (
        [string]$tauriConfig.version -ne [string]$package.version -or
        [string]$tauriConfig.version -ne $cargoVersion
    ) {
        throw "App versions differ across tauri.conf.json, package.json, and Cargo.toml."
    }
    if ("nsis" -notin @($tauriConfig.bundle.targets)) {
        throw "Tauri does not enable the NSIS bundle target."
    }
    if ([string]$tauriConfig.bundle.windows.nsis.installMode -ne "currentUser") {
        throw "Tauri NSIS installMode must remain currentUser."
    }
    $configuredHooks = [string]$tauriConfig.bundle.windows.nsis.installerHooks
    if (-not $configuredHooks) {
        throw "Tauri NSIS installerHooks must configure the app process guard."
    }
    $resolvedHooks = if ([IO.Path]::IsPathRooted($configuredHooks)) {
        [IO.Path]::GetFullPath($configuredHooks)
    } else {
        [IO.Path]::GetFullPath(
            (Join-Path (Split-Path -Parent $tauriConfigPath) $configuredHooks)
        )
    }
    if (
        -not $resolvedHooks.Equals(
            [IO.Path]::GetFullPath($appInstallerHooksPath),
            [StringComparison]::OrdinalIgnoreCase
        )
    ) {
        throw "Tauri NSIS installerHooks does not reference packaging/app-installer-hooks.nsh."
    }
    if (-not (Test-Path -LiteralPath $resolvedHooks -PathType Leaf)) {
        throw "Tauri NSIS installer hook does not exist: $resolvedHooks"
    }
    $appInstallerHooks = Get-Content -LiteralPath $resolvedHooks -Raw
    foreach ($macroName in @("NSIS_HOOK_PREINSTALL", "NSIS_HOOK_PREUNINSTALL")) {
        $macro = [regex]::Match(
            $appInstallerHooks,
            "(?ms)!macro\s+$macroName\b(?<body>.*?)!macroend"
        )
        if (-not $macro.Success) {
            throw "App installer hook is missing $macroName."
        }
        $body = $macro.Groups["body"].Value
        if (
            $body -notmatch 'Get-Process\s+-Name\s+local-transcript,local-transcript-worker' -or
            $body -notmatch '\bAbort\b'
        ) {
            throw "$macroName must abort when the app or worker process is running."
        }
    }

    $rustEntry = Get-Content -LiteralPath $libPath -Raw
    if ($rustEntry -notmatch '\.app_local_data_dir\(\)\?') {
        throw "Rust must use Tauri app_local_data_dir() for the non-roaming library root."
    }
    if ($rustEntry -match '\.app_data_dir\(\)\?') {
        throw "Rust still references Tauri app_data_dir(), which is roaming on Windows."
    }
    if (
        $rustEntry -notmatch '\.resource_dir\(\)\?' -or
        $rustEntry -notmatch 'join\("runtime"\)'
    ) {
        throw "Rust must resolve the installer-owned processing runtime from Tauri resources."
    }

    $runtimeInstaller = Get-Content -LiteralPath $runtimeInstallerPath -Raw
    $expectedRuntimeInstall = 'InstallDir "$LOCALAPPDATA\com.localtranscript.desktop\runtime"'
    if (-not $runtimeInstaller.Contains($expectedRuntimeInstall)) {
        throw "The runtime NSIS installer does not target the app's LocalAppData runtime directory."
    }
    if ($runtimeInstaller -match '\$APPDATA\\com\.localtranscript\.desktop') {
        throw "The runtime NSIS installer still targets roaming AppData."
    }
    $runtimeGuard = [regex]::Match(
        $runtimeInstaller,
        '(?ms)!macro\s+EnsureLocalTranscriptIdle\b(?<body>.*?)!macroend'
    )
    if (
        -not $runtimeGuard.Success -or
        $runtimeGuard.Groups["body"].Value -notmatch 'StrCmp\s+\$0\s+"0"' -or
        $runtimeGuard.Groups["body"].Value -notmatch 'StrCmp\s+\$0\s+"23"' -or
        ([regex]::Matches($runtimeGuard.Groups["body"].Value, '\bAbort\b')).Count -lt 2
    ) {
        throw "The runtime installer process guard must fail closed for install and uninstall."
    }
    foreach ($functionName in @("Function .onInit", "Function un.onInit")) {
        $function = [regex]::Match(
            $runtimeInstaller,
            "(?ms)$([regex]::Escape($functionName))(?<body>.*?)FunctionEnd"
        )
        if (
            -not $function.Success -or
            $function.Groups["body"].Value -notmatch 'EnsureLocalTranscriptIdle'
        ) {
            throw "$functionName must enforce the runtime process guard."
        }
    }

    foreach ($releaseScriptPath in @($appReleaseScriptPath, $runtimeReleaseScriptPath)) {
        $releaseScript = Get-Content -LiteralPath $releaseScriptPath -Raw
        if (
            -not $releaseScript.Contains('if ($Sign -and $workingTreeDirty)') -or
            -not $releaseScript.Contains("Signed production artifacts require a clean committed working tree")
        ) {
            throw "$(Split-Path -Leaf $releaseScriptPath) must reject signed builds from dirty working trees."
        }
    }
    $runtimeReleaseScript = Get-Content -LiteralPath $runtimeReleaseScriptPath -Raw
    if (
        -not $runtimeReleaseScript.Contains('$placeholderSourceHost') -or
        -not $runtimeReleaseScript.Contains("FFmpeg source URL must identify the real supplier build/source page")
    ) {
        throw "release-runtime.ps1 must reject placeholder FFmpeg provenance URLs."
    }
    if (
        -not $runtimeReleaseScript.Contains("cpu_fallback_declared") -or
        -not $runtimeReleaseScript.Contains('worker_handshake = "passed_by_packager"') -or
        -not $runtimeReleaseScript.Contains("verify-worker-runtime.ps1") -or
        -not $runtimeReleaseScript.Contains('model_inference  = "not_performed_by_packager"')
    ) {
        throw "release-runtime.ps1 must distinguish declared runtime capabilities from post-install validation."
    }
    $appReleaseScript = Get-Content -LiteralPath $appReleaseScriptPath -Raw
    if (
        $appReleaseScript -notmatch '\[Parameter\(Mandatory\)\]\s*\r?\n\s*\[string\]\$RuntimePayloadDirectory' -or
        -not $appReleaseScript.Contains("app-installer-bundled-runtime-hooks.nsh") -or
        -not $appReleaseScript.Contains("build-setup-bundle.ps1") -or
        -not $appReleaseScript.Contains('runtime_bundled         = $true') -or
        -not $appReleaseScript.Contains('sha256_verified_self_extracting_7z') -or
        -not $appReleaseScript.Contains('user_initiated_combined_installer_process_guarded')
    ) {
        throw "release-app.ps1 must require and automatically install the processing runtime from one verified setup executable."
    }
    $bundledRuntimeHooks = Get-Content -LiteralPath $bundledRuntimeHooksPath -Raw
    $bundledRuntimeInstall = Get-Content -LiteralPath $bundledRuntimeInstallPath -Raw
    $setupBundleScript = Get-Content -LiteralPath $setupBundleScriptPath -Raw
    if (
        -not $bundledRuntimeHooks.Contains("NSIS_HOOK_POSTINSTALL") -or
        -not $bundledRuntimeHooks.Contains("install-runtime.ps1") -or
        -not $bundledRuntimeInstall.Contains("[Security.Cryptography.SHA256]::Create()") -or
        -not $bundledRuntimeInstall.Contains("runtime-manifest.json") -or
        -not $setupBundleScript.Contains("LTRSFXBUNDLE0001")
    ) {
        throw "The one-file setup must verify, stage, and atomically add its bundled runtime."
    }
    $workerBuildScript = Get-Content -LiteralPath $workerBuildScriptPath -Raw
    $workerSpec = Get-Content -LiteralPath $workerSpecPath -Raw
    $workerEntrypoint = Get-Content -LiteralPath $workerEntrypointPath -Raw
    if (
        -not $workerSpec.Contains('entrypoint = project_root / "pyinstaller_entrypoint.py"') -or
        -not $workerEntrypoint.Contains(
            "from local_transcript_worker.__main__ import main"
        ) -or
        -not $workerBuildScript.Contains('& $workerExecutable --version')
    ) {
        throw "The packaged worker must launch through an absolute package import and pass a post-build startup smoke test."
    }

    $workerHost = Get-Content -LiteralPath $workerHostPath -Raw
    $protocolMatch = [regex]::Match($workerHost, 'PROTOCOL_VERSION:\s*&str\s*=\s*"([^"]+)"')
    $pipelineMatch = [regex]::Match($workerHost, 'PIPELINE_VERSION:\s*&str\s*=\s*"([^"]+)"')
    if (-not $protocolMatch.Success -or -not $pipelineMatch.Success) {
        throw "Could not read the Rust worker protocol constants."
    }

    $modelManifest = Get-Content -LiteralPath $modelManifestPath -Raw | ConvertFrom-Json
    $revisions = Get-Content -LiteralPath $revisionsPath -Raw | ConvertFrom-Json
    if ([string]$revisions.worker_protocol_version -ne $protocolMatch.Groups[1].Value) {
        throw "packaging/revisions.json worker protocol does not match Rust."
    }
    if (
        [string]$revisions.pipeline_version -ne $pipelineMatch.Groups[1].Value -or
        [string]$revisions.pipeline_version -ne [string]$modelManifest.pipeline_version
    ) {
        throw "Pipeline revisions differ across packaging, Rust, and the model manifest."
    }

    foreach ($model in $modelManifest.models) {
        if ([string]$model.revision -notmatch "^[0-9a-f]{40}$") {
            throw "Model $($model.key) does not use a 40-character pinned revision."
        }
        $packagedRevision = $revisions.models.PSObject.Properties[$model.key].Value
        if ([string]$packagedRevision -ne [string]$model.revision) {
            throw "Model revision mismatch for $($model.key)."
        }
        foreach ($file in $model.files) {
            if (
                [string]$file.hash.algorithm -ne "sha256" -or
                [string]$file.hash.value -notmatch "^[0-9a-f]{64}$" -or
                [long]$file.size -le 0
            ) {
                throw "Model file $($model.key)/$($file.path) is not pinned by size and SHA-256."
            }
        }
    }

    Write-Host "Source release contracts: OK"
    Write-Host "  App version: $($tauriConfig.version)"
    Write-Host "  Data root: %LOCALAPPDATA%\$($tauriConfig.identifier)"
    Write-Host "  App installer guard: $configuredHooks"
    Write-Host "  Worker protocol: $($revisions.worker_protocol_version)"
    Write-Host "  Pipeline: $($revisions.pipeline_version)"
    Write-Host "  Model manifest SHA-256: $((Get-FileHash -LiteralPath $modelManifestPath -Algorithm SHA256).Hash.ToLowerInvariant())"
}

$repositoryRoot = Get-ReleaseRepositoryRoot
Assert-SourceReleaseContracts -RepositoryRoot $repositoryRoot

if (-not $ManifestPath) {
    return
}

$resolvedManifest = [IO.Path]::GetFullPath($ManifestPath)
if (-not (Test-Path -LiteralPath $resolvedManifest -PathType Leaf)) {
    throw "Manifest does not exist: $resolvedManifest"
}
$manifest = Get-Content -LiteralPath $resolvedManifest -Raw | ConvertFrom-Json
$manifestDirectory = Split-Path -Parent $resolvedManifest
$signatureCandidates = [Collections.Generic.List[string]]::new()
$artifactKindProperty = $manifest.PSObject.Properties["artifact_kind"]
$payloadProperty = $manifest.PSObject.Properties["payload"]

if ($artifactKindProperty -and $artifactKindProperty.Value) {
    $installerCandidate = $null
    foreach ($propertyName in @("installer", "payload_manifest")) {
        $property = $manifest.PSObject.Properties[$propertyName]
        if ($property -and $property.Value) {
            $verified = Assert-ReleaseRecord -Record $property.Value -Root $manifestDirectory
            if ($propertyName -eq "installer") {
                $installerCandidate = $verified
            }
        }
    }
    if ($RequireSignature -and -not [bool]$manifest.signed) {
        throw "The release manifest identifies this artifact as unsigned."
    }
    if ([bool]$manifest.signed -and $installerCandidate) {
        $signatureCandidates.Add($installerCandidate)
    }
} elseif ($payloadProperty -and $payloadProperty.Value) {
    if (-not $PayloadRoot) {
        throw "A payload manifest requires -PayloadRoot (for example the installed runtime directory)."
    }
    $resolvedPayloadRoot = [IO.Path]::GetFullPath($PayloadRoot)
    foreach ($record in $manifest.payload) {
        Assert-ReleaseRecord -Record $record -Root $resolvedPayloadRoot | Out-Null
    }
    if ($RequireSignature -and -not [bool]$manifest.component_authenticode) {
        throw "The payload manifest identifies its components as unsigned."
    }
    if ([bool]$manifest.component_authenticode) {
        if (-not $manifest.authenticode_files) {
            throw "Signed payload manifest does not declare its Authenticode entrypoints."
        }
        foreach ($relativePath in $manifest.authenticode_files) {
            $signatureCandidates.Add(
                (Resolve-VerifiedManifestPath -Root $resolvedPayloadRoot -RelativePath ([string]$relativePath))
            )
        }
    }
} else {
    throw "Unsupported release manifest shape."
}

if ($RequireSignature -or $signatureCandidates.Count -gt 0) {
    $signTool = Resolve-ReleaseSignTool -RequestedPath $SignToolPath
    if (-not $signTool) {
        throw "Signature verification requires SignTool. Install the Windows SDK or set LOCAL_TRANSCRIPT_SIGNTOOL_PATH."
    }
    foreach ($candidate in $signatureCandidates) {
        Invoke-ReleaseNative `
            -FilePath $signTool `
            -ArgumentList @("verify", "/pa", "/all", "/v", $candidate) `
            -FailureMessage "Authenticode verification failed for $candidate."
    }
}

Write-Host "Release manifest and file hashes: OK"
