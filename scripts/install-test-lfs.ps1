param(
    [Parameter(Mandatory = $true)][string]$Destination,
    [Parameter(Mandatory = $true)][string]$CacheDirectory
)

$ErrorActionPreference = 'Stop'
$version = '3.8.0'
$checksum = 'b62e7b8ceddee635f691233d77de8eaa4b213e9209e0173811d8cfa77f7882c1'
$asset = "git-lfs-windows-amd64-v$version.zip"
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
        for ($attempt = 0; $attempt -lt 3; $attempt++) {
            try {
                Invoke-WebRequest -UseBasicParsing -Uri "https://github.com/git-lfs/git-lfs/releases/download/v$version/$asset" -OutFile $archive -TimeoutSec 180
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
    if ($LASTEXITCODE -ne 0 -or $actualVersion -notmatch '^git-lfs/3\.8\.0\s') {
        throw 'Job-local Git LFS version verification failed'
    }
    Write-Output $actualVersion
} finally {
    Remove-Item -LiteralPath $temporary -Recurse -Force
}
