$ErrorActionPreference='Stop'
$root=Join-Path $env:TEMP 'adit-vnc-followup-20261020/cross-process-delayed'
if(Test-Path -LiteralPath $root){throw 'Fresh result directory required'}
New-Item -ItemType Directory -Path $root|Out-Null
$guiPath='C:\data\codes\adit\target\debug\examples\keyboard_hook_gui_probe.exe'
$observerPath='C:\data\codes\adit\target\debug\examples\keyboard_hook_probe.exe'
$result=Join-Path $root 'foreground.json'
$gui=$null
$observer=$null
$record=[ordered]@{started_utc=[DateTime]::UtcNow.ToString('o');scope='separate observe-only process while minimal iced GUI is foreground';gui_sha256=(Get-FileHash $guiPath).Hash;observer_sha256=(Get-FileHash $observerPath).Hash}
try{
    $gui=Start-Process -FilePath $guiPath -ArgumentList ('"'+$result+'" --rearm') -WindowStyle Normal -PassThru
    $record.gui_pid=$gui.Id
    $observer=Start-Process -FilePath $observerPath -ArgumentList '--observe-only --inject-after=4' -WindowStyle Hidden -RedirectStandardOutput (Join-Path $root 'observer.txt') -RedirectStandardError (Join-Path $root 'observer-errors.txt') -PassThru
    $record.observer_pid=$observer.Id
    if(-not $observer.WaitForExit(22000)){throw 'Observer deadline exceeded'}
    if(-not $gui.WaitForExit(1000)){throw 'GUI did not finish within observer interval'}
    $record.gui_exit=$gui.ExitCode
    $record.observer_exit=$observer.ExitCode
    $r=Get-Content -LiteralPath $result -Raw|ConvertFrom-Json
    $record.gui_outcome=$r.outcome
    $record.gui_continuous_foreground=$r.continuous_foreground
    $record.gui_own_probe_events=$r.final_hook.probe_events
    [pscustomobject]$record|ConvertTo-Json -Depth 6
    Get-Content (Join-Path $root 'observer.txt') | Select-String 'selftest_foreground_pid|own_probe_events|injected_f24'
}finally{
    foreach($owned in @($gui,$observer)){if($owned -and -not $owned.HasExited){$owned.Kill();$owned.WaitForExit()}}
    $record.finished_utc=[DateTime]::UtcNow.ToString('o')
    $record.owned_processes_exited=($gui.HasExited -and $observer.HasExited)
    [IO.File]::WriteAllText((Join-Path $root 'invocation.json'),($record|ConvertTo-Json -Depth 6),[Text.UTF8Encoding]::new($false))
}
