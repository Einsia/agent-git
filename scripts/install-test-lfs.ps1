param(
    [Parameter(Mandatory = $true)][string]$Destination,
    [Parameter(Mandatory = $true)][string]$CacheDirectory,
    [switch]$RequireCachedArchive
)

$ErrorActionPreference = 'Stop'
$tool = (Get-Content (Join-Path $PSScriptRoot 'windows-test-tools.json') -Raw | ConvertFrom-Json).lfs
$version = $tool.version
$checksum = $tool.sha256
$asset = $tool.archive
$temporary = Join-Path ([System.IO.Path]::GetTempPath()) ("agit-test-lfs-" + [Guid]::NewGuid().ToString('N'))
$archive = Join-Path $temporary $asset
$cached = Join-Path $CacheDirectory "$checksum-$asset"

function Test-ReleaseArchive([string]$Path) {
    return (Test-Path -LiteralPath $Path -PathType Leaf) -and
        ((Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() -eq $checksum)
}

New-Item -ItemType Directory -Path $temporary | Out-Null
try {
    if (Test-Path -LiteralPath $cached -PathType Leaf) {
        Copy-Item -LiteralPath $cached -Destination $archive
    }
    if (-not (Test-ReleaseArchive $archive)) {
        if ($RequireCachedArchive) { throw 'The prepared Git LFS archive is missing or corrupt' }
        for ($attempt = 0; $attempt -lt 3; $attempt++) {
            try {
                Invoke-WebRequest -UseBasicParsing -Uri $tool.url -OutFile $archive -TimeoutSec 180
                break
            } catch {
                if ($attempt -eq 2) { throw 'Unable to download the pinned Git LFS release' }
                Start-Sleep -Seconds 2
            }
        }
    }
    if (-not (Test-ReleaseArchive $archive)) {
        throw 'Git LFS release checksum does not match its pinned value'
    }
    New-Item -ItemType Directory -Force -Path $CacheDirectory | Out-Null
    Copy-Item -LiteralPath $archive -Destination $cached -Force
    $unpacked = Join-Path $temporary 'extracted'
    Expand-Archive -LiteralPath $archive -DestinationPath $unpacked
    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    $binary = Join-Path $Destination 'git-lfs.exe'
    Copy-Item -LiteralPath (Join-Path $unpacked "git-lfs-$version/git-lfs.exe") -Destination $binary -Force
    $actualVersion = & $binary version
    if ($LASTEXITCODE -ne 0 -or $actualVersion -notmatch ('^git-lfs/' + [regex]::Escape($version) + '\s')) {
        throw 'Job-local Git LFS version verification failed'
    }
    Write-Output $actualVersion
} finally {
    Remove-Item -LiteralPath $temporary -Recurse -Force
}
