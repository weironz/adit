# Opt-in Windows diagnostic: only tagged F24 down/up, no intercepted input.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$ProbeExe,
    [Parameter(Mandatory)][string]$StateDir,
    [ValidateSet('foreground', 'background', 'prearm', 'rearm')]
    [string]$Mode = 'foreground'
)
$ErrorActionPreference = 'Stop'
if (-not [IO.Path]::IsPathFullyQualified($StateDir)) {
    throw 'StateDir must be an absolute, fresh directory.'
}
if (Test-Path -LiteralPath $StateDir) { throw 'Refusing to overwrite an earlier diagnostic.' }
$executable = (Resolve-Path -LiteralPath $ProbeExe).Path
New-Item -ItemType Directory -Path $StateDir | Out-Null
$report = Join-Path $StateDir 'report.json'
$arguments = '"' + $report + '"'
if ($Mode -ne 'foreground') { $arguments += ' --' + $Mode }
$record = [ordered]@{
    scope = 'minimal iced GUI; actual library hook; tagged F24; observe only'
    mode = $Mode
    executable_sha256 = (Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant()
    started_utc = [DateTime]::UtcNow.ToString('o')
    completed = $false
}
$owned = $null
try {
    $owned = Start-Process -FilePath $executable -ArgumentList $arguments -WindowStyle Normal -PassThru
    $record.pid = $owned.Id
    if (-not $owned.WaitForExit(25000)) { throw 'Owned probe exceeded its 25-second deadline.' }
    $record.exit_code = $owned.ExitCode
    if (-not (Test-Path -LiteralPath $report)) { throw 'Probe exited without a report.' }
    $result = Get-Content -LiteralPath $report -Raw | ConvertFrom-Json
    $record.outcome = $result.outcome
    $record.completed = $true
    if ($owned.ExitCode -ne 0) { throw 'Probe process failed.' }
    [pscustomobject]@{
        mode = $Mode
        outcome = $result.outcome
        foreground = $result.continuous_foreground
        own_probe_events = $result.final_hook.probe_events
        injected_f24_accepted = $result.injected_f24_accepted
        state_dir = $StateDir
    } | ConvertTo-Json
} finally {
    if ($owned -and -not $owned.HasExited) { $owned.Kill(); $owned.WaitForExit() }
    $record.finished_utc = [DateTime]::UtcNow.ToString('o')
    $record.owned_process_exited = ($null -eq $owned -or $owned.HasExited)
    [IO.File]::WriteAllText((Join-Path $StateDir 'invocation.json'),
        ($record | ConvertTo-Json -Depth 5), [Text.UTF8Encoding]::new($false))
}
