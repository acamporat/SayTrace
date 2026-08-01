#requires -Version 7.2

[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$FilePath,

    [Parameter()]
    [string]$SignToolPath = $env:LOCAL_TRANSCRIPT_SIGNTOOL_PATH,

    [Parameter()]
    [string]$CertificateThumbprint = $env:LOCAL_TRANSCRIPT_SIGN_CERT_THUMBPRINT,

    [Parameter()]
    [string]$TimestampUrl = $(if ($env:LOCAL_TRANSCRIPT_TIMESTAMP_URL) {
        $env:LOCAL_TRANSCRIPT_TIMESTAMP_URL
    } else {
        "http://timestamp.digicert.com"
    })
)

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "release-common.ps1")

if (-not $CertificateThumbprint) {
    throw "Signing was requested, but LOCAL_TRANSCRIPT_SIGN_CERT_THUMBPRINT is empty."
}

Invoke-ReleaseSigning `
    -FilePath $FilePath `
    -SignToolPath $SignToolPath `
    -CertificateThumbprint $CertificateThumbprint `
    -TimestampUrl $TimestampUrl
