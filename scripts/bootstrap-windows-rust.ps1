$ErrorActionPreference = 'Stop'
$env:CARGO_HOME = Join-Path $PWD '.cache/cargo'
$env:RUSTUP_HOME = Join-Path $PWD '.cache/rustup'
$env:PATH = "$env:CARGO_HOME\bin;$env:PATH"
$rustup = Join-Path $env:CARGO_HOME 'bin/rustup.exe'
if (-not (Test-Path $rustup)) {
    $bootstrap = Join-Path $PWD '.cache/rustup-bootstrap'
    New-Item -ItemType Directory -Force $bootstrap | Out-Null
    $installer = Join-Path $bootstrap 'rustup-init.exe'
    $checksum = '88d8258dcf6ae4f7a80c7d1088e1f36fa7025a1cfd1343731b4ee6f385121fc0'
    # RUSTUP_UPDATE_ROOT is the mirror rustup itself updates from, and it serves the same archive
    # path. Only a copy matching the pinned checksum runs, so a mirror decides whether the installer
    # is reachable, never which installer runs; upstream is tried when the mirror fails.
    $roots = @($env:RUSTUP_UPDATE_ROOT, 'https://static.rust-lang.org/rustup') | Where-Object { $_ } | Select-Object -Unique
    $verified = $false
    foreach ($root in $roots) {
        & curl.exe --fail --silent --show-error --location --proto '=https' --proto-redir '=https' `
            --retry 3 --retry-all-errors --connect-timeout 20 --max-time 180 `
            "$root/archive/1.28.2/x86_64-pc-windows-msvc/rustup-init.exe" --output $installer
        if ($LASTEXITCODE -eq 0 -and
            (Get-FileHash $installer -Algorithm SHA256).Hash.ToLowerInvariant() -eq $checksum) {
            $verified = $true
            break
        }
        Write-Warning "rustup-init from $root is unreachable or does not match the pinned official release"
    }
    if (-not $verified) { throw 'Unable to download the pinned official Rust installer' }
    & $installer -y --no-modify-path --default-toolchain none --default-host x86_64-pc-windows-msvc --profile minimal
    if ($LASTEXITCODE -ne 0) { throw 'Job-local Rust bootstrap failed' }
}
