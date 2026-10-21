$ErrorActionPreference='Stop'
function Canon([string]$v){if($v -notmatch '^[A-Za-z]:[\\/]'){throw 'Absolute local path required'};([IO.Path]::GetFullPath($v).Replace('/','\') -replace '\\+','\').TrimEnd('\')}
$names=@('AditVncKeys-74c435392ff548f8b04dfb36481e310a','AditVncKeys-0e455bba634e4dd3b05235022ad89618','AditVncKeys-a8f917e502845c647a9f318cf68d9f11')
$ids=@(9932,11728,8072,11632,2144,9676,12056,4396,1568)
$roots=@($names|ForEach-Object { 'C:\ProgramData\'+$_ })
$remaining=@(Get-CimInstance Win32_Process|Where-Object {$p=$_.ExecutablePath;$p -and @($roots|Where-Object {(Canon $p).StartsWith((Canon $_)+'\payload\',[StringComparison]::OrdinalIgnoreCase)}).Count}|Select-Object ProcessId,Name)
$listener=@(Get-NetTCPConnection -LocalPort 15915 -State Listen -ErrorAction SilentlyContinue|Select-Object LocalAddress,LocalPort,OwningProcess)
if($remaining.Count -or $listener.Count){throw 'Owned processes or fixture listener remain'}
$a=@{stage='all_cases_independent_cleanup';pids=$ids;owned_processes_remaining=$remaining;listener_15915=$listener;utc=[datetime]::UtcNow.ToString('o')}
$reports=@()
foreach($n in $names){
 if($n -notmatch '^AditVncKeys-[a-f0-9]{32}$'){throw 'Name not allowlisted'}
 $d=Canon ('C:\ProgramData\'+$n)
 if(!(Test-Path -LiteralPath $d)){$reports+=@{name=$n;directory_absent=$true;already_absent=$true};continue}
 if((Get-Item -LiteralPath $d).Attributes -band [IO.FileAttributes]::ReparsePoint){throw 'Root must not be a reparse point'}
 $owner=Get-Content -LiteralPath ($d+'\owner.json') -Raw|ConvertFrom-Json
 if($owner.name -cne $n -or (Canon $owner.directory) -cne $d -or $d -cne ('C:\ProgramData\'+$n)){throw 'Exact owner/path check failed'}
 if((Get-FileHash -LiteralPath ($d+'\cleanup.ps1') -Algorithm SHA256).Hash.ToLowerInvariant() -ne 'f223a379e3105d47eaae2ab2b5493c3ade2b001024a43fe7a1df283028cd675e'){throw 'Cleanup source hash mismatch'}
 & ($d+'\cleanup.ps1') -Directory $d
 $r=Get-Content -LiteralPath ($d+'\cleanup.json') -Raw|ConvertFrom-Json
 if(!$r.share_absent -or $r.remaining_processes.Count -or $r.tasks_remaining.Count){throw 'Owned cleanup incomplete'}
 # Only the three explicit GUID roots above, each verified against its owner.
 Remove-Item -LiteralPath $d -Recurse -Force
 $reports+=@{name=$n;cleanup=$r;directory_absent=(!(Test-Path -LiteralPath $d))}
}
Add-Type 'using System.Runtime.InteropServices; public static class AditVncFinalKeyProbe { [DllImport("user32.dll")] public static extern short GetAsyncKeyState(int key); }'
$pressed=@(foreach($vk in @(0xA0,0xA1,0xA2,0xA3,0xA4,0xA5,0x5B,0x5C)){if(([AditVncFinalKeyProbe]::GetAsyncKeyState($vk) -band 0x8000) -ne 0){$vk}})
$final=@{stage='all_owned_cleanup_verified';audit=$a;roots=$reports;pressed_modifier_keys=$pressed;language_tags=@((Get-WinUserLanguageList).LanguageTag);utc=[datetime]::UtcNow.ToString('o')}
Set-Clipboard ('ADIT_VNC_TEST:'+($final|ConvertTo-Json -Depth 10 -Compress))
