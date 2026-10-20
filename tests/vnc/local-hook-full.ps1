# Explicit native acceptance; no user profiles, clipboard or desktop captures.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$ProbeExe,
    [Parameter(Mandatory)][string]$PythonExe,
    [Parameter(Mandatory)][string]$StateDir
)
$ErrorActionPreference = 'Stop'
if (-not [IO.Path]::IsPathFullyQualified($StateDir) -or (Test-Path -LiteralPath $StateDir)) {
    throw 'StateDir must be a fresh absolute directory.'
}
$executable = (Resolve-Path -LiteralPath $ProbeExe).Path
$python = (Resolve-Path -LiteralPath $PythonExe).Path
$fixture = Join-Path $PSScriptRoot 'native_rfb_fixture.py'
New-Item -ItemType Directory -Path $StateDir | Out-Null
$peerDir = Join-Path $StateDir 'peer'
$report = Join-Path $StateDir 'report.json'
$record = [ordered]@{
    scope = 'full isolated GUI; production dispatcher; tagged F24; synthetic loopback only'
    executable_sha256 = (Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant()
    fixture_sha256 = (Get-FileHash -LiteralPath $fixture -Algorithm SHA256).Hash.ToLowerInvariant()
    started_utc = [DateTime]::UtcNow.ToString('o')
    completed = $false
}
$server = $null
$probe = $null
try {
    $server = Start-Process -FilePath $python -ArgumentList ('"'+$fixture+'" --state-dir "'+$peerDir+'"') -WindowStyle Hidden -PassThru -RedirectStandardError (Join-Path $StateDir 'peer-errors.txt')
    $record.peer_pid = $server.Id
    $deadline = [DateTime]::UtcNow.AddSeconds(8)
    while (-not (Test-Path -LiteralPath (Join-Path $peerDir 'ready.json'))) {
        if ($server.HasExited -or [DateTime]::UtcNow -ge $deadline) { throw 'Owned synthetic peer did not become ready.' }
        Start-Sleep -Milliseconds 80
    }
    $start = [Diagnostics.ProcessStartInfo]::new($executable)
    $start.UseShellExecute = $false
    $start.Environment['ADIT_NATIVE_HOOK_AUTOTEST'] = '1'
    $start.Environment['ADIT_NATIVE_HOOK_REPORT'] = $report
    $probe = [Diagnostics.Process]::Start($start)
    $record.probe_pid = $probe.Id
    if (-not $probe.WaitForExit(35000)) { throw 'Owned native window exceeded its deadline.' }
    $record.exit_code = $probe.ExitCode
    if (-not (Test-Path -LiteralPath $report)) { throw 'Native window exited without a report.' }
    $result = Get-Content -LiteralPath $report -Raw | ConvertFrom-Json
    $record.outcome = $result.outcome
    $record.completed = $true
    [pscustomobject]@{outcome=$result.outcome;cycles=$result.cycles.Count;final_restored=$result.final_restored;state_dir=$StateDir} | ConvertTo-Json
    if ($probe.ExitCode -ne 0 -or $result.outcome -ne 'passed') { throw 'Native acceptance did not pass; retained the full report.' }
} finally {
    if ($probe -and -not $probe.HasExited) { $probe.Kill(); $probe.WaitForExit() }
    if (Test-Path -LiteralPath $peerDir) { [IO.File]::WriteAllText((Join-Path $peerDir 'stop.request'),'stop') }
    if ($server -and -not $server.WaitForExit(3000)) { $server.Kill(); $server.WaitForExit() }
    $record.finished_utc = [DateTime]::UtcNow.ToString('o')
    $record.owned_processes_exited = (($null -eq $probe -or $probe.HasExited) -and ($null -eq $server -or $server.HasExited))
    [IO.File]::WriteAllText((Join-Path $StateDir 'invocation.json'),
        ($record | ConvertTo-Json -Depth 5), [Text.UTF8Encoding]::new($false))
}
