$ErrorActionPreference='Stop'
$d='C:\ProgramData\AditVncKeys-2aa748064b34498fb59db43a6805bdde'
$n='AditVncKeys-2aa748064b34498fb59db43a6805bdde'
$o=Get-Content -LiteralPath ($d+'\owner.json') -Raw|ConvertFrom-Json
if($o.name -ne $n -or $o.directory -ne $d){throw 'Owner mismatch'}
foreach($suffix in @('-language','-lease')){$t=Get-ScheduledTask -TaskName ($n+$suffix);if(($t.Actions.Arguments|Out-String) -notlike ('*'+$d+'*')){throw 'Lease action mismatch'};Set-ScheduledTask -TaskName ($n+$suffix) -Trigger (New-ScheduledTaskTrigger -Once -At (Get-Date).AddMinutes(45))|Out-Null}
