param(
    [ValidateSet('all', 'unit', 'integration')][string]$Suite = 'all',
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
$filters = @()
if ($Suite -ne 'integration') {
    $unitFilters = ($suites.unit | ForEach-Object { "test(~$_)" }) -join ' | '
    $tunnelFilter = ($suites.tunnel_unit | ForEach-Object { "test(=$_)" }) -join ' | '
    $cargoArgs += @('--package', 'agit', '--package', 'agit-tunnel', '--lib', '--bin', 'agit')
    $filters += "(package(=agit) & ((kind(lib) & ($unitFilters)) | (kind(bin) & test(~startup_tests)))) | (package(=agit-tunnel) & kind(lib) & ($tunnelFilter))"
}
if ($Suite -ne 'unit') {
    foreach ($test in $suites.integration) {
        $cargoArgs += @('--test', $test)
    }
    $filters += 'kind(test)'
}
$cargoArgs += @('-E', ($filters -join ' | '))
if ($Suite -ne 'integration') {
    $tunnelFilter = ($suites.tunnel_unit | ForEach-Object { "test(=$_)" }) -join ' | '
    $tunnelArgs = @('--locked', '--target', $target, '--package', 'agit-tunnel', '--lib', '-E', $tunnelFilter)
    $inventory = (& cargo nextest list @tunnelArgs --message-format json) -join "`n"
    if ($LASTEXITCODE -ne 0) { throw 'Tunnel test enumeration failed' }
    $selected = @()
    $listed = $inventory | ConvertFrom-Json
    foreach ($binary in $listed.'rust-suites'.PSObject.Properties) {
        foreach ($test in $binary.Value.testcases.PSObject.Properties) {
            if ($test.Value.'filter-match'.status -eq 'matches') { $selected += $test.Name }
        }
    }
    foreach ($test in $suites.tunnel_unit) {
        if ($selected -notcontains $test) { throw "Required tunnel test was not selected: $test" }
        Write-Output "Selected tunnel regression: $test"
    }
}
& cargo @cargoArgs
if ($LASTEXITCODE -ne 0) { throw "Windows $Suite tests failed" }
if ($Suite -ne 'unit') {
    $env:AGIT_NPM_SMOKE_BINARY = (Resolve-Path "target/$target/debug/agit.exe").Path
    & node npm/smoke.mjs
    if ($LASTEXITCODE -ne 0) { throw 'Windows npm installation smoke test failed' }
}
