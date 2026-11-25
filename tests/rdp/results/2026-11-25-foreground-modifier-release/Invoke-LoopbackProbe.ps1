param(
    [Parameter(Mandatory = $true)][string]$Probe,
    [Parameter(Mandatory = $true)][string]$Helper,
    [Parameter(Mandatory = $true)][ValidateSet('a', 'b')][string]$Session,
    [Parameter(Mandatory = $true)][string]$Commands,
    [ValidateRange(1, 3600)][int]$TimeoutSeconds = 180
)
$ErrorActionPreference = 'Stop'
$name = "adit89$Session"
$target = if ($Session -eq 'a') { '127.0.0.2:3389' } else { '127.0.0.3:3389' }
$instructions = Get-Content -LiteralPath $Commands
$password = Get-Content -LiteralPath (Join-Path $HOME "AditKey89-$name.dpapi") | ConvertTo-SecureString
$credential = [PSCredential]::new($name, $password)
try {
    & {
        $ErrorActionPreference = 'Continue'
        & {
            $credential.GetNetworkCredential().Password
            $instructions
        } | & $Probe $Helper $target $name $TimeoutSeconds --domain $env:COMPUTERNAME 2>&1 |
            ForEach-Object { $_.ToString() }
        if ($LASTEXITCODE -ne 0) { throw "RDP probe exited with code $LASTEXITCODE" }
    }
} finally {
    $password.Dispose()
}
