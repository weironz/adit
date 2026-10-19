param(
    [Parameter(Mandatory)][string]$Directory,
    [Parameter(Mandatory)][string[]]$RemoteAddress,
    [Parameter(Mandatory)][string]$UserSid
)
$ErrorActionPreference = 'Stop'
$admin = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $admin.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'An elevated, authorized test VM is required' }
if (-not [IO.Path]::IsPathRooted($Directory) -or (Test-Path -LiteralPath $Directory)) { throw 'A new absolute evidence directory is required' }
if ($UserSid -notmatch '^S-1-5-21-\d+-\d+-\d+-\d+$') { throw 'An explicit local test-user SID is required' }
New-Item -ItemType Directory -Path $Directory | Out-Null
$name = 'AditVncAcceptance-' + [guid]::NewGuid().ToString('N')
$service = Get-Service WinRM
$record = [ordered]@{ name=$name; directory=$Directory; original_status=[string]$service.Status; original_start_type=[string]$service.StartType; listener_created=$false; endpoint_created=$false; firewall_created=$false; outcome='failed' }
$state = Join-Path $Directory 'bootstrap.json'
function Save-State { $record | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $state -Encoding UTF8 }
Save-State
try {
    Start-Service WinRM
    $http = @(Get-ChildItem WSMan:\localhost\Listener | Where-Object { $_.Keys -contains 'Transport=HTTP' })
    if ($http.Count -eq 0) {
        New-Item WSMan:\localhost\Listener -Transport HTTP -Address '*' | Out-Null
        $record.listener_created = $true
        Save-State
    }
    # A removable endpoint grants only the named test identity. It avoids changing
    # LocalAccountTokenFilterPolicy, TrustedHosts, Basic or AllowUnencrypted.
    $sddl = "O:NSG:BAD:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;$UserSid)"
    $configuration = Join-Path $Directory 'endpoint.pssc'
    New-PSSessionConfigurationFile -Path $configuration -RunAsVirtualAccount -ExecutionPolicy Bypass -LanguageMode FullLanguage
    Register-PSSessionConfiguration -Name $name -Path $configuration -SecurityDescriptorSddl $sddl -Force -NoServiceRestart | Out-Null
    $record.endpoint_created = $true
    Save-State
    New-NetFirewallRule -Name $name -DisplayName $name -Direction Inbound -Action Allow -Protocol TCP -LocalPort 5985 -RemoteAddress $RemoteAddress -Profile Any | Out-Null
    $record.firewall_created = $true
    Save-State
    Restart-Service WinRM
    $record.outcome = 'ready'
    Save-State
    Set-Clipboard ('ADIT_VNC_TEST:' + ($record | ConvertTo-Json -Depth 5 -Compress))
} catch {
    $record.error = $_.Exception.Message
    Save-State
    Set-Clipboard ('ADIT_VNC_TEST:' + ($record | ConvertTo-Json -Depth 5 -Compress))
    throw
}
