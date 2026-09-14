param(
    [Parameter(Mandatory = $true)][ValidateSet('unit', 'integration')][string]$Suite,
    [switch]$RequireCachedTools
)

$ErrorActionPreference = 'Stop'
& (Join-Path $PSScriptRoot 'bootstrap-windows-rust.ps1')
foreach ($tool in @('cargo', 'node', 'git', 'curl.exe')) {
    Get-Command $tool -ErrorAction Stop | Out-Null
}
& node scripts/verify-windows-powershell.mjs
if ($LASTEXITCODE -ne 0) { throw 'Windows PowerShell host verification failed' }
$target = 'x86_64-pc-windows-msvc'
$env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS = '-Ctarget-feature=+crt-static'
& rustup target add $target
if ($LASTEXITCODE -ne 0) { throw 'Rust target installation failed' }
$testBin = Join-Path $PWD '.cache/test-bin'
& (Join-Path $PSScriptRoot 'install-windows-nextest.ps1') -Destination $testBin -CacheDirectory (Join-Path $PWD '.cache/test-nextest') -RequireCachedArchive:$RequireCachedTools
& (Join-Path $PSScriptRoot 'install-test-lfs.ps1') -Destination $testBin -CacheDirectory (Join-Path $PWD '.cache/test-lfs') -RequireCachedArchive:$RequireCachedTools
$env:PATH = "$testBin;$env:PATH"
$env:AGIT_TEST_REQUIRE_LFS = '1'
$suites = Get-Content (Join-Path $PSScriptRoot 'windows-test-suites.json') -Raw | ConvertFrom-Json
$cargoArgs = @('nextest', 'run', '--locked', '--target', $target, '--profile', 'windows')
if ($Suite -eq 'unit') {
    & cargo check --locked --no-default-features --target $target --lib
    if ($LASTEXITCODE -ne 0) { throw 'Windows library without RC failed to compile' }
    $filters = ($suites.unit | ForEach-Object { "test(~$_)" }) -join ' | '
    $cargoArgs += @('--lib', '--bin', 'agit', '-E', "(kind(lib) & ($filters)) | (kind(bin) & test(~startup_tests))")
} else {
    foreach ($test in $suites.integration) {
        $cargoArgs += @('--test', $test)
    }
}
& cargo @cargoArgs
if ($LASTEXITCODE -ne 0) { throw "Windows $Suite tests failed" }
if ($Suite -eq 'integration') {
    # Standalone harnesses do not implement nextest's test-listing protocol.
    foreach ($test in $suites.custom) {
        & cargo test --locked --target $target --test $test
        if ($LASTEXITCODE -ne 0) { throw "Windows custom harness failed: $test" }
    }
}
