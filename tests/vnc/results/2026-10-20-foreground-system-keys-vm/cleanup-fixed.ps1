param([Parameter(Mandatory)][string]$Directory)
$ErrorActionPreference='Stop'
function Canon([string]$Value){
 if($Value -notmatch '^[A-Za-z]:[\\/]'){throw 'Expected an absolute local path'}
 ([IO.Path]::GetFullPath($Value).Replace('/','\') -replace '\\+','\').TrimEnd('\')
}
$d=Canon $Directory
$b=Get-Content -LiteralPath ($d+'\owner.json') -Raw|ConvertFrom-Json
if($b.name -notmatch '^AditVncKeys-[a-f0-9]{32}$' -or $d -cne (Canon $b.directory) -or $d -cne ('C:\ProgramData\'+$b.name)){throw 'Owner mismatch'}
$prefix=$d+'\payload\'
$owned=@(Get-CimInstance Win32_Process|Where-Object {$_.ExecutablePath -and (Canon $_.ExecutablePath).StartsWith($prefix,[StringComparison]::OrdinalIgnoreCase)})
foreach($process in $owned){Stop-Process -Id $process.ProcessId -Force -ErrorAction SilentlyContinue}
$share=Get-SmbShare -Name $b.name -ErrorAction SilentlyContinue
if($share){if((Canon $share.Path) -cne $d){throw 'Share ownership mismatch'};Remove-SmbShare -Name $b.name -Force}
foreach($task in @(Get-ScheduledTask|Where-Object {$_.TaskName.StartsWith($b.name+'-')})){Unregister-ScheduledTask -TaskName $task.TaskName -Confirm:$false}
$remaining=@(Get-CimInstance Win32_Process|Where-Object {$_.ExecutablePath -and (Canon $_.ExecutablePath).StartsWith($prefix,[StringComparison]::OrdinalIgnoreCase)}|Select-Object ProcessId,Name)
$final=@{stage='owned_cleanup';directory=$d;remaining_processes=$remaining;share_absent=(-not [bool](Get-SmbShare -Name $b.name -ErrorAction SilentlyContinue));tasks_remaining=@(Get-ScheduledTask|Where-Object {$_.TaskName.StartsWith($b.name+'-')}|Select-Object TaskName);winrm=[string](Get-Service WinRM).Status;firewall=@(Get-NetFirewallProfile|Select-Object Name,Enabled);utc=[datetime]::UtcNow.ToString('o')}
$final|ConvertTo-Json -Depth 6|Set-Content -LiteralPath ($d+'\cleanup.json') -Encoding UTF8
Set-Clipboard ('ADIT_VNC_TEST:'+($final|ConvertTo-Json -Depth 6 -Compress))
