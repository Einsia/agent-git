$ErrorActionPreference = 'Stop'
& (Join-Path $PSScriptRoot 'bootstrap-windows-rust.ps1')
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
& cargo test --locked --release --target $target --test windows_rc --test import_noninteractive_selection --test show_ref_header --test import_lineage_preview --test merge_recon --test merge_settlement --test hub_credential_binding --test whoami_identity_snapshot --test codex_resume_provider --test windows_json_capture --test quiet_presentation --test doctor_deep --test doctor_link_integrity --test doctor_local_health --test doctor_repo_scope --test context_echo --test context_echo_run_mine -- --nocapture
if ($LASTEXITCODE -ne 0) { throw 'Windows native CLI integration tests failed' }
& cargo test --locked --release --target $target --lib commands::json::windows::tests -- --nocapture
if ($LASTEXITCODE -ne 0) { throw 'Windows native JSON capture lifecycle tests failed' }
& cargo test --locked --release --target $target --lib hub::git:: -- --nocapture
if ($LASTEXITCODE -ne 0) { throw 'Windows Git transport tests failed' }
foreach ($suite in @('native_snapshot', 'domain::native_archive::tests', 'adapter::codex_index::tests', 'domain::import_lineage::tests', 'domain::repo::tests::local_', 'domain::repo::tests::inspection_', 'domain::repo::tests::legacy_inspection_', 'commands::fix::tests')) {
    & cargo test --locked --release --target $target --lib $suite -- --nocapture
    if ($LASTEXITCODE -ne 0) { throw "Windows import lineage suite failed: $suite" }
}
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
