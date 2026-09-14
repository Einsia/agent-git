param(
    [Parameter(Mandatory = $true)][string]$Destination,
    [Parameter(Mandatory = $true)][string]$CacheDirectory,
    [switch]$RequireCachedArchive
)

$ErrorActionPreference = 'Stop'
$tool = (Get-Content (Join-Path $PSScriptRoot 'windows-test-tools.json') -Raw | ConvertFrom-Json).nextest
$version = $tool.version
$checksum = $tool.sha256
$archive = Join-Path $CacheDirectory "$checksum-$($tool.archive)"
New-Item -ItemType Directory -Force -Path $CacheDirectory | Out-Null
if (-not (Test-Path -LiteralPath $archive -PathType Leaf) -or
    (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant() -ne $checksum) {
    if ($RequireCachedArchive) { throw 'The prepared cargo-nextest archive is missing or corrupt' }
    & curl.exe --fail --silent --show-error --location --proto '=https' --proto-redir '=https' `
        --retry 3 --retry-all-errors --connect-timeout 20 --max-time 180 `
        $tool.url --output $archive
    if ($LASTEXITCODE -ne 0) { throw 'Unable to download the pinned cargo-nextest release' }
}
if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant() -ne $checksum) {
    throw 'cargo-nextest release checksum does not match its pinned value'
}
Expand-Archive -LiteralPath $archive -DestinationPath $Destination -Force
$actualVersion = (& (Join-Path $Destination 'cargo-nextest.exe') nextest --version) -join "`n"
if ($LASTEXITCODE -ne 0 -or $actualVersion -notmatch ('^cargo-nextest ' + [regex]::Escape($version) + '\s')) {
    throw "Job-local cargo-nextest version verification failed (exit $LASTEXITCODE): $actualVersion"
}
Write-Output $actualVersion
