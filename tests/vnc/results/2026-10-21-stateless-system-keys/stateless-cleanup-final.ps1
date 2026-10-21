$ErrorActionPreference='Stop'
$d='C:\ProgramData\AditVncKeys-2aa748064b34498fb59db43a6805bdde'
$n='AditVncKeys-2aa748064b34498fb59db43a6805bdde'
if([IO.Path]::GetFullPath($d) -cne ('C:\ProgramData\'+$n) -or $n -notmatch '^AditVncKeys-[a-f0-9]{32}$'){throw 'Absolute owned target mismatch'}
$b=Get-Content -LiteralPath ($d+'\owner.json') -Raw|ConvertFrom-Json
if($b.name -cne $n -or [IO.Path]::GetFullPath($b.directory) -cne $d){throw 'Owner mismatch'}
if((Get-Item -LiteralPath $d).Attributes -band [IO.FileAttributes]::ReparsePoint){throw 'Root reparse point'}
if(@(Get-ChildItem -LiteralPath $d -Force -Recurse|Where-Object {$_.Attributes -band [IO.FileAttributes]::ReparsePoint}).Count){throw 'Owned tree contains reparse point'}
if((Get-FileHash -LiteralPath ($d+'\cleanup.ps1') -Algorithm SHA256).Hash.ToLowerInvariant() -cne 'f223a379e3105d47eaae2ab2b5493c3ade2b001024a43fe7a1df283028cd675e'){throw 'Cleanup source mismatch'}
$previous=Get-Content -LiteralPath ($d+'\results\independent-before-cleanup.json') -Raw|ConvertFrom-Json
if(!$previous.language_restored -or @($previous.owned_processes).Count -or @($previous.listeners).Count){throw 'Independent recovery proof missing'}
& ($d+'\cleanup.ps1') -Directory $d
$payload=$d+'\payload\'
$processes=@(Get-CimInstance Win32_Process|Where-Object {$_.ExecutablePath -and $_.ExecutablePath.StartsWith($payload,[StringComparison]::OrdinalIgnoreCase)}|Select-Object ProcessId,Name)
$tasks=@(Get-ScheduledTask|Where-Object {$_.TaskName.StartsWith($n+'-')}|Select-Object TaskName)
$share=Get-SmbShare -Name $n -ErrorAction SilentlyContinue
$listeners=@(Get-NetTCPConnection -State Listen -ErrorAction SilentlyContinue|Where-Object {$_.LocalPort -eq 15915}|Select-Object LocalPort,OwningProcess)
if($processes.Count -or $tasks.Count -or $share -or $listeners.Count){throw 'Owned resources remain'}
Remove-Item -LiteralPath $d -Recurse -Force
$l=Get-WinUserLanguageList
$report=@{stage='owned_cleanup_final';directory=$d;directory_absent=!(Test-Path -LiteralPath $d);share_absent=(!$share);tasks=$tasks;processes=$processes;listeners=$listeners;languages=@($l|Select-Object LanguageTag,InputMethodTips,Spellchecking,Handwriting);winrm=[string](Get-Service WinRM).Status;firewall=@(Get-NetFirewallProfile|Select-Object Name,Enabled);utc=[datetime]::UtcNow.ToString('o')}
Set-Clipboard ('ADIT_VNC_TEST:'+($report|ConvertTo-Json -Depth 7 -Compress))
