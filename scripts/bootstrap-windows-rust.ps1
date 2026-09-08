$ErrorActionPreference = 'Stop'
$env:CARGO_HOME = Join-Path $PWD '.cache/cargo'
$env:RUSTUP_HOME = Join-Path $PWD '.cache/rustup'
$env:PATH = "$env:CARGO_HOME\bin;$env:PATH"
$rustup = Join-Path $env:CARGO_HOME 'bin/rustup.exe'
if (-not (Test-Path $rustup)) {
    $bootstrap = Join-Path $PWD '.cache/rustup-bootstrap'
    New-Item -ItemType Directory -Force $bootstrap | Out-Null
    $installer = Join-Path $bootstrap 'rustup-init.exe'
    Invoke-WebRequest 'https://static.rust-lang.org/rustup/archive/1.28.2/x86_64-pc-windows-msvc/rustup-init.exe' -OutFile $installer
    $checksum = '88d8258dcf6ae4f7a80c7d1088e1f36fa7025a1cfd1343731b4ee6f385121fc0'
    if ((Get-FileHash $installer -Algorithm SHA256).Hash.ToLowerInvariant() -ne $checksum) {
        throw 'Rust installer checksum does not match the pinned official release'
    }
    & $installer -y --no-modify-path --default-toolchain none --default-host x86_64-pc-windows-msvc --profile minimal
    if ($LASTEXITCODE -ne 0) { throw 'Job-local Rust bootstrap failed' }
}
