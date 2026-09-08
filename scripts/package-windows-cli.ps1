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
foreach ($tool in @('cargo', 'node', 'git', 'tar')) {
    Get-Command $tool -ErrorAction Stop | Out-Null
}
& node scripts/verify-windows-powershell.mjs
if ($LASTEXITCODE -ne 0) { throw 'Windows PowerShell host verification failed' }
if ($env:AGIT_RELEASE_CHANNEL -notin @('dev', 'staging')) { throw 'Internal packages require dev or staging' }
if (-not $env:AGIT_DEFAULT_HUB_URL) { throw 'AGIT_DEFAULT_HUB_URL is required' }
if (-not $env:AGIT_BUILD_SHA) { throw 'AGIT_BUILD_SHA is required' }
$version = [regex]::Match((Get-Content Cargo.toml -Raw), '(?m)^version = "([^"]+)"').Groups[1].Value
if (-not $version) { throw 'Cargo package version is missing' }
$shortSha = $env:AGIT_BUILD_SHA.Substring(0, 12)
$env:AGIT_BUILD_VERSION = "$version-$env:AGIT_RELEASE_CHANNEL+$shortSha"
$target = 'x86_64-pc-windows-msvc'
$env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS = '-Ctarget-feature=+crt-static'
& rustup target add $target
if ($LASTEXITCODE -ne 0) { throw 'Rust target installation failed' }
& cargo build --locked --release --target $target --bin agit
if ($LASTEXITCODE -ne 0) { throw 'Windows CLI build failed' }
$binary = "target/$target/release/agit.exe"
& cargo check --locked --release --no-default-features --target $target --lib
if ($LASTEXITCODE -ne 0) { throw 'Windows library without RC failed to compile' }
& cargo test --locked --release --target $target --test windows_rc --test import_noninteractive_selection --test merge_recon --test merge_settlement --test hub_credential_binding --test whoami_identity_snapshot -- --nocapture
if ($LASTEXITCODE -ne 0) { throw 'Windows native CLI integration tests failed' }
& node scripts/verify-windows-cli.mjs $binary
if ($LASTEXITCODE -ne 0) { throw 'Windows executable verification failed' }
$actualVersion = & $binary --version
if ($LASTEXITCODE -ne 0 -or $actualVersion -ne "agit $env:AGIT_BUILD_VERSION") { throw "Unexpected version: $actualVersion" }
$env:AGIT_NPM_SMOKE_BINARY = (Resolve-Path $binary).Path
$env:AGIT_NPM_SMOKE_VERSION = $env:AGIT_BUILD_VERSION
& node npm/smoke.mjs
if ($LASTEXITCODE -ne 0) { throw 'Windows npm installation smoke test failed' }
$package = 'dist/package'
New-Item -ItemType Directory -Force $package | Out-Null
Copy-Item $binary "$package/agit.exe"
@"
channel=$env:AGIT_RELEASE_CHANNEL
version=$env:AGIT_BUILD_VERSION
commit=$env:AGIT_BUILD_SHA
hub=$env:AGIT_DEFAULT_HUB_URL
target=$target
"@ | Set-Content "$package/BUILD-INFO.txt" -Encoding utf8
@'
AgentGit CLI installation for Windows x64

Extract the archive, add its directory to your user PATH, and run:
  .\agit.exe --version
  .\agit.exe setup

Verify the executable with Get-FileHash .\agit.exe -Algorithm SHA256 against SHA256SUMS.
'@ | Set-Content "$package/INSTALL.txt" -Encoding utf8
$hash = (Get-FileHash "$package/agit.exe" -Algorithm SHA256).Hash.ToLowerInvariant()
"$hash  agit.exe" | Set-Content "$package/SHA256SUMS" -Encoding ascii
$archive = "dist/agit-$env:AGIT_RELEASE_CHANNEL-$shortSha-$target.tar.gz"
& tar -C $package -czf $archive agit.exe BUILD-INFO.txt INSTALL.txt SHA256SUMS
if ($LASTEXITCODE -ne 0) { throw 'Windows artifact packaging failed' }
Write-Output "built $archive"
