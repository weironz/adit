$ErrorActionPreference='Stop'
$d='C:\ProgramData\AditVncKeys-f6a76343ddc648689e1c9ff5efe2b1e5'
$owner=Get-Content -LiteralPath ($d+'\owner.json') -Raw|ConvertFrom-Json
if($owner.name -cne 'AditVncKeys-f6a76343ddc648689e1c9ff5efe2b1e5' -or [IO.Path]::GetFullPath($owner.directory) -cne $d){throw 'Owner mismatch'}
$baseline=@([IO.File]::ReadAllText(($d+'\results\language-baseline.json'),[Text.Encoding]::UTF8)|ConvertFrom-Json)
$l=Get-WinUserLanguageList
$actual=@($l|Select-Object LanguageTag,InputMethodTips,Spellchecking,Handwriting)
$expected=$baseline|ConvertTo-Json -Depth 6 -Compress
$actualJson=$actual|ConvertTo-Json -Depth 6 -Compress
$payload=$d+'\payload\'
$report=@{stage='independent_before_cleanup';language_restored=($expected -ceq $actualJson);baseline=$baseline;languages=$actual;owned_processes=@(Get-CimInstance Win32_Process|Where-Object {$_.ExecutablePath -and $_.ExecutablePath.StartsWith($payload,[StringComparison]::OrdinalIgnoreCase)}|Select-Object ProcessId,Name);listeners=@(Get-NetTCPConnection -State Listen -ErrorAction SilentlyContinue|Where-Object {$_.LocalPort -eq 15915}|Select-Object LocalAddress,LocalPort,OwningProcess);winrm=[string](Get-Service WinRM).Status;firewall=@(Get-NetFirewallProfile|Select-Object Name,Enabled);tasks=@(Get-ScheduledTask|Where-Object {$_.TaskName.StartsWith($owner.name+'-')}|Select-Object TaskName);utc=[datetime]::UtcNow.ToString('o')}
$report|ConvertTo-Json -Depth 8|Set-Content -LiteralPath ($d+'\results\independent-before-cleanup.json') -Encoding UTF8
if(!$report.language_restored -or $report.owned_processes.Count -or $report.listeners.Count){throw 'Recovery or owned resources not clear'}
