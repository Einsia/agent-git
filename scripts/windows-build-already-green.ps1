# Prints "green" when GitLab's own job records show that this commit already has a successful
# Windows build on the default branch, and "build" otherwise.
#
# It reads this project's pipelines API with the job token: the default branch's pipelines of
# CI_COMMIT_SHA, then each one's jobs. Only a dev:windows-x64 or dev:windows-x64-scheduled job of
# exactly that commit on the default branch with status success answers "green". It relies on
# GitLab's records of the default branch's own build jobs, not on any marker that can be written
# separately, so it is as trustworthy as that branch's CI definition and its allowed run settings.
# A scheduled build that stopped as "already green" is such a record too, and it only exists after
# an earlier successful build of the same commit. A failed read, an answer it cannot parse, or no
# matching job answers "build", so the scheduled build runs whenever it cannot show the commit is
# covered. It always exits 0, so the job decides what to do with the answer.
#
# Nothing printed holds the token or a response the script does not recognize: a failed request
# prints the HTTP status and GitLab's answer only when that answer is one of the known ones below,
# and any other failure prints only the exception type.
$ErrorActionPreference = 'Stop'
$knownAnswers = '404 Not found', '404 Project Not Found', '401 Unauthorized', '403 Forbidden'
$windowsJobs = 'dev:windows-x64', 'dev:windows-x64-scheduled'
$token = "$env:CI_JOB_TOKEN"
$commit = "$env:CI_COMMIT_SHA"
$branch = "$env:CI_DEFAULT_BRANCH"
$api = '{0}/projects/{1}' -f $env:CI_API_V4_URL, $env:CI_PROJECT_ID

# Names why a read failed using only the status and GitLab's known answers.
function Get-FailureReason($failure) {
    $response = $failure.Exception.Response
    if (-not $response) { return "no usable answer ($($failure.Exception.GetType().Name))" }
    $status = [int]$response.StatusCode
    $body = "$($failure.ErrorDetails.Message)".Trim()
    $answer = $null
    try { $answer = (ConvertFrom-Json $body).message } catch { $answer = $body }
    if ($answer -is [string] -and $knownAnswers -ccontains $answer) { return "$status `"$answer`"" }
    return "$status, response not shown"
}

function Get-ApiList([string] $Path) {
    $content = (Invoke-WebRequest -UseBasicParsing -Uri "$api$Path" -Headers @{ 'JOB-TOKEN' = $token } -TimeoutSec 60).Content
    if ($content -is [byte[]]) { $content = [System.Text.Encoding]::UTF8.GetString($content) }
    return @(ConvertFrom-Json $content)
}

if (-not $token) { Write-Host 'CI_JOB_TOKEN is empty in this job' }
if ($commit -cnotmatch '^[0-9a-f]{40}$' -or -not $branch) {
    Write-Host 'CI_COMMIT_SHA is not a commit id or the default branch is unknown; not looking for an earlier Windows build'
    Write-Output 'build'
    exit 0
}
try {
    $pipelines = Get-ApiList ('/pipelines?ref={0}&sha={1}&per_page=100' -f [uri]::EscapeDataString($branch), $commit)
    foreach ($pipeline in $pipelines) {
        if ("$($pipeline.id)" -cnotmatch '^[0-9]+$') { continue }
        foreach ($job in (Get-ApiList "/pipelines/$($pipeline.id)/jobs?per_page=100")) {
            if ($windowsJobs -ccontains $job.name -and $job.status -ceq 'success' -and $job.ref -ceq $branch `
                    -and $job.commit.id -ceq $commit -and "$($job.id)" -cmatch '^[0-9]+$') {
                Write-Host "already green: $($job.name) job $($job.id) in pipeline $($pipeline.id) built $commit"
                Write-Output 'green'
                exit 0
            }
        }
    }
} catch {
    Write-Host "Could not read the Windows builds of ${commit}: $(Get-FailureReason $_)"
    Write-Output 'build'
    exit 0
}
Write-Host "No successful Windows build of $commit on $branch in GitLab's records"
Write-Output 'build'
exit 0
