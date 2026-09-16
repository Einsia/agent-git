param([switch]$RequireCachedArchive)

$ErrorActionPreference = 'Stop'
$tool = Get-Content (Join-Path $PSScriptRoot 'windows-build-python.json') -Raw | ConvertFrom-Json
$archive = Join-Path $PWD (Join-Path $tool.cacheDirectory "$($tool.sha256)-$($tool.archive)")
if (-not (Test-Path -LiteralPath $archive -PathType Leaf)) {
    if ($RequireCachedArchive) { throw 'The prepared Python archive is missing' }
    & node (Join-Path $PSScriptRoot 'prepare-windows-test-tools.mjs') . python
    if ($LASTEXITCODE -ne 0) { throw 'Python archive preparation failed' }
}
if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant() -ne $tool.sha256) {
    throw 'Python archive checksum does not match the pinned official release'
}
$destination = Join-Path $PWD ".cache/build-python/$($tool.sha256)"
Expand-Archive -LiteralPath $archive -DestinationPath $destination -Force
$binary = Join-Path $destination 'python.exe'
$version = & $binary --version
if ($LASTEXITCODE -ne 0 -or $version -ne "Python $($tool.version)") {
    throw 'Job-local Python version verification failed'
}
$env:PATH = "$destination;$env:PATH"
Write-Output $version
