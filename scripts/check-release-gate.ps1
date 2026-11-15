param(
    [string]$Version = $env:RELEASE_VERSION,
    [string]$Repository = $env:GITHUB_REPOSITORY,
    [string]$Sha = $env:GITHUB_SHA,
    [string]$Ref = $env:GITHUB_REF
)
$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor 3072
. (Join-Path $PSScriptRoot 'release-validation.ps1')
if (-not $env:GH_TOKEN) { throw 'GH_TOKEN with contents/actions read access is required' }
if ($Repository -cne 'weironz/adit') { throw 'Unexpected release repository' }
if ($Sha -cnotmatch '^[a-f0-9]{40}$') { throw 'Invalid commit SHA' }
if ($Ref -cne 'refs/heads/main') { throw 'Release is allowed from main only' }
$root = Split-Path $PSScriptRoot
$current = Get-AditWorkspaceVersion $root
Assert-AditReleaseVersion $current $Version
$api = if ($env:GITHUB_API_URL) { $env:GITHUB_API_URL } else { 'https://api.github.com' }
$headers = @{Authorization = "Bearer $env:GH_TOKEN"; Accept = 'application/vnd.github+json'; 'X-GitHub-Api-Version' = '2022-11-28'}
# The self-hosted runner reaches GitHub only through its proxy, and Windows
# PowerShell's Invoke-RestMethod ignores HTTPS_PROXY (gh, git and cargo honour
# it), so without -Proxy the gate failed with "could not resolve api.github.com".
$proxy = @($env:HTTPS_PROXY, $env:https_proxy, $env:HTTP_PROXY, $env:http_proxy) |
    Where-Object { $_ } | Select-Object -First 1
$proxyArgs = if ($proxy) { @{Proxy = $proxy} } else { @{} }
function Read-ReleaseAPI([string]$Path, [switch]$AllowMissing) {
    try { return Invoke-RestMethod -Uri "$api/repos/$Repository/$Path" -Headers $headers @proxyArgs }
    catch {
        if ($AllowMissing -and $_.Exception.Response -and [int]$_.Exception.Response.StatusCode -eq 404) { return $null }
        throw
    }
}
$main = Read-ReleaseAPI 'git/ref/heads/main'
$runs = Read-ReleaseAPI "actions/workflows/ci.yml/runs?head_sha=$Sha&branch=main&per_page=1"
$run = @($runs.workflow_runs) | Select-Object -First 1
$jobs = if ($run) { @((Read-ReleaseAPI "actions/runs/$($run.id)/jobs?filter=latest&per_page=100").jobs) } else { @() }
Assert-AditReleaseCI $Ref $Sha $main.object.sha $run $jobs
if (Read-ReleaseAPI "git/ref/tags/v$Version" -AllowMissing) { throw "Tag v$Version already exists" }
if (Read-ReleaseAPI "releases/tags/v$Version" -AllowMissing) { throw "Release v$Version already exists" }
Write-Output "Release gate passed for $Sha / $Version / Windows self-hosted and Linux hosted CI jobs. This check performs no publishing."
