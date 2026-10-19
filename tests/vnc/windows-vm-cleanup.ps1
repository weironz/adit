param([Parameter(Mandatory)][string]$Directory)
$ErrorActionPreference = 'Stop'
$record = Get-Content -LiteralPath (Join-Path $Directory 'bootstrap.json') -Raw | ConvertFrom-Json
if ($record.directory -cne $Directory -or $record.name -notmatch '^AditVncAcceptance-[0-9a-f]{32}$') { throw 'Unexpected ownership metadata' }
$name = $record.name
if ($record.firewall_created) { Remove-NetFirewallRule -Name $name }
if ($record.endpoint_created) { Unregister-PSSessionConfiguration -Name $name -Force -NoServiceRestart }
if ($record.listener_created) {
    $http = @(Get-ChildItem WSMan:\localhost\Listener | Where-Object { $_.Keys -contains 'Transport=HTTP' })
    if ($http.Count -ne 1) { throw 'HTTP listener ownership changed; inspect before cleanup' }
    Remove-Item -LiteralPath $http[0].PSPath -Recurse
}
if ($record.original_status -eq 'Stopped') { Stop-Service WinRM -Force } else { Restart-Service WinRM }
$cleanup = @{ outcome='passed'; endpoint_absent= -not [bool](Get-PSSessionConfiguration -Name $name -ErrorAction SilentlyContinue); firewall_absent= -not [bool](Get-NetFirewallRule -Name $name -ErrorAction SilentlyContinue); service_status=[string](Get-Service WinRM).Status }
$cleanup | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $Directory 'cleanup.json') -Encoding UTF8
Set-Clipboard ('ADIT_VNC_TEST:' + ($cleanup | ConvertTo-Json -Compress))
