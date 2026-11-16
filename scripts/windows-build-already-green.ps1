# Prints "green" when a Windows build of this main commit has already passed, and "build" otherwise.
#
# Each successful dev:windows-x64 and dev:windows-x64-scheduled run keeps the commit it built in
# windows-build-commit.txt. This reads that file from the newest successful main pipeline of each
# job with the job token. Only a record naming this very commit counts: a missing file, a newer or
# older commit, or a failed read prints "build", so the scheduled build runs whenever it cannot show
# the commit is already covered. It always exits 0, so the job decides what to do with the answer.
$ErrorActionPreference = 'Stop'
foreach ($job in 'dev:windows-x64', 'dev:windows-x64-scheduled') {
    $uri = '{0}/projects/{1}/jobs/artifacts/{2}/raw/windows-build-commit.txt?job={3}' -f `
        $env:CI_API_V4_URL, $env:CI_PROJECT_ID, [uri]::EscapeDataString($env:CI_DEFAULT_BRANCH), [uri]::EscapeDataString($job)
    try {
        $content = (Invoke-WebRequest -UseBasicParsing -Uri $uri -Headers @{ 'JOB-TOKEN' = $env:CI_JOB_TOKEN } -TimeoutSec 60).Content
        # GitLab may serve the file as binary, which PowerShell 7 returns as bytes.
        if ($content -is [byte[]]) { $content = [System.Text.Encoding]::ASCII.GetString($content) }
        $built = "$content".Trim()
    } catch {
        Write-Host "No readable Windows build record from ${job}: $($_.Exception.Message)"
        continue
    }
    if ($built -eq $env:CI_COMMIT_SHA) {
        Write-Host "already green: $job built $built"
        Write-Output 'green'
        exit 0
    }
    Write-Host "$job last built $built, not $env:CI_COMMIT_SHA"
}
Write-Output 'build'
exit 0
