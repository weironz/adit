param([Parameter(Mandatory)][string]$Directory)
$ErrorActionPreference = 'Stop'
$Directory=[IO.Path]::GetFullPath($Directory)
$record = Get-Content -LiteralPath (Join-Path $Directory 'bootstrap.json') -Raw | ConvertFrom-Json
if ([IO.Path]::GetFullPath($record.directory) -cne $Directory -or $record.name -notmatch '^AditVncAcceptance-[0-9a-f]{32}$') { throw 'Unexpected ownership metadata' }
$name = $record.name
foreach($ready in @('results/native-ready.json','power-native/native-ready.json')) {
 $path=Join-Path $Directory $ready
 if(Test-Path -LiteralPath $path){
  $actors=Get-Content -LiteralPath $path -Raw|ConvertFrom-Json
  foreach($id in @($actors.app,$actors.fixture,$actors.observer)|Where-Object {$_}){
   $process=Get-CimInstance Win32_Process -Filter "ProcessId=$id" -ErrorAction SilentlyContinue
   if($process -and $process.ExecutablePath -and [IO.Path]::GetFullPath($process.ExecutablePath).StartsWith((Join-Path $Directory 'payload\'),[StringComparison]::OrdinalIgnoreCase)){Stop-Process -Id $id -Force}
  }
 }
}
$tasks=@(Get-ScheduledTask|Where-Object {$_.TaskName -like 'AditVncAcceptance-*' -and @($_.Actions|Where-Object {$_.Arguments -and $_.Arguments.Replace('\\','\').Contains($Directory)}).Count -gt 0})
foreach($task in $tasks){if($task.TaskName -ne ($name+'-cleanup')){Stop-ScheduledTask -TaskName $task.TaskName -ErrorAction SilentlyContinue};Unregister-ScheduledTask -TaskName $task.TaskName -Confirm:$false}
foreach($rule in @('AditVncAcceptance-Rfb-20261019','AditVncAcceptance-DropIn-20261019','AditVncAcceptance-DropOut-20261019','AditVncAcceptance-RdpGuard-20261019')){Get-NetFirewallRule -Name $rule -ErrorAction SilentlyContinue|Remove-NetFirewallRule}
if ($record.firewall_created) { Get-NetFirewallRule -Name $name -ErrorAction SilentlyContinue|Remove-NetFirewallRule }
if ($record.endpoint_created -and (Get-PSSessionConfiguration -Name $name -ErrorAction SilentlyContinue)) { Unregister-PSSessionConfiguration -Name $name -Force -NoServiceRestart }
if ($record.listener_created) {
    $http = @(Get-ChildItem WSMan:\localhost\Listener | Where-Object { $_.Keys -contains 'Transport=HTTP' })
    if ($http.Count -ne 1) { throw 'HTTP listener ownership changed; inspect before cleanup' }
    Remove-Item -LiteralPath $http[0].PSPath -Recurse
}
$cleanup = @{ outcome='failed'; endpoint_absent= -not [bool](Get-PSSessionConfiguration -Name $name -ErrorAction SilentlyContinue); firewall_absent= -not [bool](Get-NetFirewallRule -Name $name -ErrorAction SilentlyContinue) }
if ($record.original_status -eq 'Stopped') { Stop-Service WinRM -Force } else { Restart-Service WinRM }
$cleanup.service_status=[string](Get-Service WinRM).Status
$cleanup.outcome=if($cleanup.endpoint_absent -and $cleanup.firewall_absent -and $cleanup.service_status -eq $record.original_status){'passed'}else{'failed'}
$cleanup | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $Directory 'cleanup.json') -Encoding UTF8
