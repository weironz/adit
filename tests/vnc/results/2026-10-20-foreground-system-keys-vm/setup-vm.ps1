try{
$ErrorActionPreference='Stop'
$name='AditVncKeys-74c435392ff548f8b04dfb36481e310a'
$d='C:\ProgramData\'+$name
if(Test-Path -LiteralPath $d){throw 'A fresh owned directory is required'}
New-Item -ItemType Directory -Path $d,($d+'\payload'),($d+'\results')|Out-Null
$owner=@{bootstrap_pid=$PID;bootstrap_started=(Get-Process -Id $PID).StartTime.ToUniversalTime().ToString("o");name=$name;directory=$d;computer=$env:COMPUTERNAME;winrm=[string](Get-Service WinRM).Status;firewall=@(Get-NetFirewallProfile|Select-Object Name,Enabled);utc=[datetime]::UtcNow.ToString('o')}
$owner|ConvertTo-Json -Depth 5|Set-Content ($d+'\owner.json') -Encoding UTF8
$cleanup=@'
param([string]$Directory)
$ErrorActionPreference='Stop'
$d=[IO.Path]::GetFullPath($Directory);$b=Get-Content ($d+'\owner.json') -Raw|ConvertFrom-Json
if($d -cne $b.directory -or $b.name -notmatch '^AditVncKeys-[a-f0-9]{32}$'){throw 'Owner mismatch'}
$owned=@(Get-CimInstance Win32_Process|Where-Object {$_.ExecutablePath -and $_.ExecutablePath.StartsWith($d+'\payload\',[StringComparison]::OrdinalIgnoreCase)})
foreach($process in $owned){Stop-Process -Id $process.ProcessId -Force -ErrorAction SilentlyContinue}
$share=Get-SmbShare -Name $b.name -ErrorAction SilentlyContinue
if($share -and $share.Path -ceq $d){Remove-SmbShare -Name $b.name -Force}
foreach($task in @(Get-ScheduledTask|Where-Object {$_.TaskName.StartsWith($b.name+'-')})){Unregister-ScheduledTask -TaskName $task.TaskName -Confirm:$false}
@{stage='owned_cleanup';remaining_processes=@(Get-CimInstance Win32_Process|Where-Object {$_.ExecutablePath -and $_.ExecutablePath.StartsWith($d+'\payload\',[StringComparison]::OrdinalIgnoreCase)}|Select-Object ProcessId,Name);share_absent=(-not [bool](Get-SmbShare -Name $b.name -ErrorAction SilentlyContinue));winrm=[string](Get-Service WinRM).Status;firewall=@(Get-NetFirewallProfile|Select-Object Name,Enabled);utc=[datetime]::UtcNow.ToString('o')}|ConvertTo-Json -Depth 5|Set-Content ($d+'\cleanup.json') -Encoding UTF8
'@
$cleanup|Set-Content ($d+'\cleanup.ps1') -Encoding UTF8
$action=New-ScheduledTaskAction -Execute powershell.exe -Argument ('-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File '+$d+'\cleanup.ps1 -Directory '+$d)
$principal=New-ScheduledTaskPrincipal -UserId SYSTEM -LogonType ServiceAccount -RunLevel Highest
Register-ScheduledTask -TaskName ($name+'-lease') -Action $action -Principal $principal -Trigger (New-ScheduledTaskTrigger -Once -At (Get-Date).AddMinutes(60)) -Settings (New-ScheduledTaskSettingsSet -StartWhenAvailable)|Out-Null
Copy-Item -LiteralPath 'C:\ProgramData\AditVncAcceptance20261019\payload\python' -Destination ($d+'\payload\python') -Recurse
New-SmbShare -Name $name -Path $d -FullAccess ([Security.Principal.WindowsIdentity]::GetCurrent().Name) -EncryptData $true -Temporary|Out-Null
Set-Clipboard ('ADIT_VNC_TEST:'+(@{stage='keys_share_ready';directory=$d;share=$name;computer=$env:COMPUTERNAME;utc=[datetime]::UtcNow.ToString('o')}|ConvertTo-Json -Compress))

}catch{Set-Clipboard ("ADIT_VNC_TEST:"+(@{stage="setup_failed";error_type=$_.Exception.GetType().Name;message=$_.Exception.Message}|ConvertTo-Json -Compress))}