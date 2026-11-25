param(
    [string]$User = 'aditkey89',
    [string]$Root = 'C:\Users\Public\Documents\AditKey89',
    [string]$Task = 'AditKeyObserver89'
)
$ErrorActionPreference = 'Stop'
if ((Test-Path -LiteralPath $Root) -or
    (Get-LocalUser -Name $User -ErrorAction SilentlyContinue) -or
    (Get-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue)) {
    throw 'Refusing to replace a preexisting test resource'
}
New-Item -ItemType Directory -Path $Root | Out-Null
$compiler = Join-Path $env:WINDIR 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'
& $compiler /nologo /target:winexe /r:System.Windows.Forms.dll "/out:$Root\KeyObserver.exe" (Join-Path $PSScriptRoot 'KeyObserver.cs')
if ($LASTEXITCODE -ne 0) { throw 'Observer compilation failed' }

$bytes = New-Object byte[] 24
$random = [Security.Cryptography.RandomNumberGenerator]::Create()
try { $random.GetBytes($bytes) } finally { $random.Dispose() }
$password = ConvertTo-SecureString ('Ad9' + [BitConverter]::ToString($bytes).Replace('-', '')) -AsPlainText -Force
[Array]::Clear($bytes, 0, $bytes.Length)
$identity = New-LocalUser -Name $User -Password $password -Description 'Temporary Adit issue 89 acceptance'
$group = Get-LocalGroup -SID S-1-5-32-555
$account = "$env:COMPUTERNAME\$User"
Add-LocalGroupMember -Group $group -Member $account
& icacls.exe $Root /inheritance:r /grant:r '*S-1-5-32-544:(OI)(CI)(F)' '*S-1-5-18:(OI)(CI)(F)' "${account}:(OI)(CI)(RX)" | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Observer ACL setup failed' }
New-Item -ItemType File -Path "$Root\remote.txt" | Out-Null
& icacls.exe "$Root\remote.txt" /grant "${account}:(M)" | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Observer log ACL setup failed' }
$password | ConvertFrom-SecureString | Set-Content -LiteralPath (Join-Path $HOME "AditKey89-$User.dpapi") -Encoding Ascii

$action = New-ScheduledTaskAction -Execute "$Root\KeyObserver.exe" -Argument ('"' + "$Root\remote.txt" + '"')
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $account
$principal = New-ScheduledTaskPrincipal -UserId $account -LogonType Interactive -RunLevel Limited
Register-ScheduledTask -TaskName $task -Action $action -Trigger $trigger -Principal $principal | Out-Null
"observer ready for $account, SID=$($identity.SID)"
