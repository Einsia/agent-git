$ErrorActionPreference = 'Stop'
& (Join-Path $PSScriptRoot 'bootstrap-windows-rust.ps1')
& (Join-Path $PSScriptRoot 'bootstrap-windows-python.ps1')
foreach ($tool in @('cargo', 'node', 'git', 'tar')) {
    Get-Command $tool -ErrorAction Stop | Out-Null
}
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
foreach ($tool in @('git', 'bundledLfs')) {
    & node scripts/prepare-windows-test-tools.mjs . $tool
    if ($LASTEXITCODE -ne 0) { throw "Git runtime download failed: $tool" }
}
& python scripts/prepare-git-runtime.py $target .cache/git-runtime/payload.tar.gz --cache .cache/test-lfs
if ($LASTEXITCODE -ne 0) { throw 'Git runtime preparation failed' }
$env:AGIT_GIT_RUNTIME_ARCHIVE = (Resolve-Path '.cache/git-runtime/payload.tar.gz').Path
& cargo build --locked --release --features bundled-git --target $target --bin agit
if ($LASTEXITCODE -ne 0) { throw 'Bundled CLI build failed' }
& cargo test --locked --release --features bundled-git --target $target --test bundled_git
if ($LASTEXITCODE -ne 0) { throw 'Windows CLI build failed' }
$binary = "target/$target/release/agit.exe"
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
