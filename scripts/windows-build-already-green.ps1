# Prints "green" when a Windows build of this main commit has already passed, and "build" otherwise.
#
# Each successful dev:windows-x64 and dev:windows-x64-scheduled run keeps the commit it built in
# windows-build-commit.txt. This reads that file from the newest successful main pipeline of each
# job with the job token. Only a record naming this very commit counts: a missing file, a newer or
# older commit, or a failed read prints "build", so the scheduled build runs whenever it cannot show
# the commit is already covered. It always exits 0, so the job decides what to do with the answer.
#
# A failed read prints the HTTP status and, when GitLab's answer is one of the known ones below,
# that answer, then tries the same URL once more with the token as the job_token query parameter: a
# runner or proxy that drops the JOB-TOKEN header would otherwise look exactly like a missing
# record. GitLab answers "404 Project Not Found" when no token arrived, "401 Unauthorized" for a
# token it rejects, and "404 Not found" when the pipeline has no such job, so the answer tells those
# apart.
# The second request's URL carries the token, so no response text this script does not recognize is
# ever printed, in any form: an unknown error body, an exception message, or a record that is not a
# commit id could echo the request URL, the token included or encoded. The URL printed for the
# second request shows [MASKED] where the token goes.
$ErrorActionPreference = 'Stop'
$token = "$env:CI_JOB_TOKEN"
if (-not $token) { Write-Host 'CI_JOB_TOKEN is empty in this job' }
$knownAnswers = '404 Project Not Found', '404 Not found', '404 page not found', '401 Unauthorized', '403 Forbidden'

# Names why a request failed using only the status and GitLab's known answers.
function Get-FailureReason($failure) {
    $response = $failure.Exception.Response
    if (-not $response) { return "no HTTP response ($($failure.Exception.GetType().Name))" }
    $status = [int]$response.StatusCode
    $body = "$($failure.ErrorDetails.Message)".Trim()
    $answer = $null
    try { $answer = (ConvertFrom-Json $body).message } catch { $answer = $body }
    if ($answer -is [string] -and $knownAnswers -ccontains $answer) { return "$status `"$answer`"" }
    return "$status, response not shown"
}

# Returns the commit id the record names, or $null after printing why there is none.
function Read-BuildRecord([string] $Uri, [hashtable] $Headers, [string] $Shown) {
    try {
        $content = (Invoke-WebRequest -UseBasicParsing -Uri $Uri -Headers $Headers -TimeoutSec 60).Content
    } catch {
        Write-Host "No readable Windows build record from ${Shown}: $(Get-FailureReason $_)"
        return $null
    }
    # GitLab may serve the file as binary, which PowerShell 7 returns as bytes.
    if ($content -is [byte[]]) { $content = [System.Text.Encoding]::ASCII.GetString($content) }
    $record = "$content".Trim()
    if ($record -cnotmatch '^[0-9a-f]{40}$') {
        Write-Host "The Windows build record from $Shown is not a commit id; not shown"
        return $null
    }
    return $record
}

foreach ($job in 'dev:windows-x64', 'dev:windows-x64-scheduled') {
    $uri = '{0}/projects/{1}/jobs/artifacts/{2}/raw/windows-build-commit.txt?job={3}' -f `
        $env:CI_API_V4_URL, $env:CI_PROJECT_ID, [uri]::EscapeDataString($env:CI_DEFAULT_BRANCH), [uri]::EscapeDataString($job)
    $built = Read-BuildRecord $uri @{ 'JOB-TOKEN' = $token } "$uri with the JOB-TOKEN header"
    if ($null -eq $built) {
        $built = Read-BuildRecord "$uri&job_token=$([uri]::EscapeDataString($token))" @{} "$uri&job_token=[MASKED]"
    }
    if ($null -eq $built) { continue }
    if ($built -eq $env:CI_COMMIT_SHA) {
        Write-Host "already green: $job built $built"
        Write-Output 'green'
        exit 0
    }
    Write-Host "$job last built $built, not $env:CI_COMMIT_SHA"
}
Write-Output 'build'
exit 0
