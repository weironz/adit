$ErrorActionPreference='Stop'
$d='C:\ProgramData\AditVncKeys-0e455bba634e4dd3b05235022ad89618'
$ids=@(8084,4384,10124)
$a=@{stage='candidate2_independent_cleanup';utc=[datetime]::UtcNow.ToString('o');pids=$ids;processes_remaining=@(Get-CimInstance Win32_Process|Where-Object {$_.ProcessId -in $ids}|Select-Object ProcessId,Name,ExecutablePath);listener_15915=@(Get-NetTCPConnection -LocalPort 15915 -State Listen -ErrorAction SilentlyContinue|Select-Object LocalAddress,LocalPort,OwningProcess);previous_share_absent=(-not [bool](Get-SmbShare -Name 'AditVncKeys-74c435392ff548f8b04dfb36481e310a' -ErrorAction SilentlyContinue));previous_tasks=@(Get-ScheduledTask|Where-Object {$_.TaskName.StartsWith('AditVncKeys-74c435392ff548f8b04dfb36481e310a-')}|Select-Object TaskName)}
$a|ConvertTo-Json -Depth 5|Set-Content ($d+'\results\independent-cleanup.json') -Encoding UTF8
if($a.processes_remaining.Count -or $a.listener_15915.Count){throw 'Owned native resources still present'}
& ($d+'\cleanup.ps1') -Directory $d
$f=Get-Content ($d+'\cleanup.json') -Raw|ConvertFrom-Json
Set-Clipboard ('ADIT_VNC_TEST:'+(@{stage='candidate2_cleanup_verified';audit=$a;cleanup=$f}|ConvertTo-Json -Depth 8 -Compress))
