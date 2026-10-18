# This fixture runs only on a disposable GitHub-hosted Windows runner.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Probe,
    [Parameter(Mandatory)][string]$Results
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (-not $IsWindows -or $env:GITHUB_ACTIONS -ne 'true' -or
    $env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows' -or
    -not $env:RUNNER_NAME -or -not $env:RUNNER_TEMP -or -not $env:GITHUB_SHA) {
    throw 'Only a disposable GitHub-hosted Windows runner may execute this vendor fixture.'
}
$resultPath = [IO.Path]::GetFullPath($Results)
$runnerTemp = [IO.Path]::GetFullPath($env:RUNNER_TEMP).TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
if (-not $resultPath.StartsWith($runnerTemp, [StringComparison]::OrdinalIgnoreCase) -or
    (Test-Path -LiteralPath $resultPath)) {
    throw 'Choose a fresh results directory beneath RUNNER_TEMP; existing evidence is never overwritten.'
}
[void][IO.Directory]::CreateDirectory($resultPath)
$work = Join-Path $env:RUNNER_TEMP ('adit-ultravnc-' + [guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($work)
$utf8 = [Text.UTF8Encoding]::new($false)
$port = 15911
$server = $null
$reservation = $null
$failed = $false
$source = '0cd8b41aeb34e52e170b33e24411b3f805e303be'
$record = [ordered]@{
    outcome = 'failed'; source_sha = $env:GITHUB_SHA; run_id = $env:GITHUB_RUN_ID
    scope = 'UltraVNC 1.8.3.0 portable application on disposable Windows CI; 3 full-frame VncAuth samples and 1 wrong-password rejection only'
    runner = $env:RUNNER_NAME; os = [Environment]::OSVersion.VersionString
    target = "127.0.0.1:$port"; samples = @(); configuration_reference_commit = $source
    configuration_reference_note = 'Pinned official reference source explains INI/password/CLI semantics; it is not a claim that the downloaded vendor binary was built from this commit.'
    sources = @(
        'https://uvnc.com/downloads/ultravnc/172-ultravnc-1-8-3-0.html',
        'https://uvnc.com/docs/ultravnc-server/69-ultravnc-ini.html',
        'https://uvnc.com/docs/ultravnc-server/51-ultravnc-server-commandline-parameters.html',
        "https://github.com/ultravnc/UltraVNC/blob/$source/common/inifile.cpp",
        "https://github.com/ultravnc/UltraVNC/blob/$source/winvnc/winvnc/vncauth.c",
        "https://github.com/ultravnc/UltraVNC/blob/$source/winvnc/winvnc/winvnc.cpp",
        "https://github.com/ultravnc/UltraVNC/blob/$source/winvnc/winvnc/UdpEchoServer.cpp"
    )
    limitations = @(
        'Not a Windows workstation/service, UAC, lock/unlock, keyboard, clipboard, DPI, sleep or physical-interface acceptance.',
        'Three loopback samples are smoke evidence, not the Windows weak-network matrix or latency percentiles.',
        'Full framebuffer means opaque decoded pixels in production SessionManager, not GPU presentation or proof of desktop content.',
        'VncAuth is not transport encryption. Only a public synthetic fixture password is used.',
        'Upstream enables a wildcard UDP echo independently of LoopbackOnly. An exclusive loopback-only UDP reservation attempts to prevent that unused listener; unexpected vendor non-loopback endpoints fail the fixture.',
        'The hosted runner may not expose a capturable interactive desktop. A startup/listener alone never passes this test.'
    )
}
function Write-Evidence {
    [IO.File]::WriteAllText((Join-Path $resultPath 'result.json'), ($record | ConvertTo-Json -Depth 12) + "`n", $utf8)
}
function Write-Phase([string]$Message) {
    $line = "{0:o} {1}" -f [DateTimeOffset]::UtcNow, $Message
    [IO.File]::AppendAllText((Join-Path $resultPath 'runner.log'), $line + "`n", $utf8)
    Write-Host $line
}
function Assert-OwnedListeners {
    $tcp = @(Get-NetTCPConnection -OwningProcess $server.Id -State Listen -ErrorAction SilentlyContinue)
    $udp = @(Get-NetUDPEndpoint -OwningProcess $server.Id -ErrorAction SilentlyContinue)
    $snapshot = [ordered]@{
        tcp = @($tcp | Select-Object LocalAddress, LocalPort, OwningProcess)
        udp = @($udp | Select-Object LocalAddress, LocalPort, OwningProcess)
    }
    [IO.File]::WriteAllText((Join-Path $resultPath 'listeners.json'), ($snapshot | ConvertTo-Json -Depth 5) + "`n", $utf8)
    foreach ($endpoint in @($tcp) + @($udp)) {
        if ($endpoint.LocalAddress -notin @('127.0.0.1', '::1') -or $endpoint.LocalPort -ne $port) {
            throw "Unexpected vendor endpoint: $($endpoint.LocalAddress):$($endpoint.LocalPort)"
        }
    }
    return @($tcp | Where-Object { $_.LocalAddress -eq '127.0.0.1' -and $_.LocalPort -eq $port }).Count -gt 0
}
function Invoke-Probe([int]$Index, [string]$Password, [bool]$WrongPassword) {
    $info = [Diagnostics.ProcessStartInfo]::new($script:probePath)
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.StandardOutputEncoding = $utf8
    $info.StandardErrorEncoding = $utf8
    foreach ($argument in @('127.0.0.1', [string]$port, '15', '--steady-seconds=1')) { $info.ArgumentList.Add($argument) }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $info
    $started = $false
    try {
        $started = $process.Start()
        if (-not $started) { throw 'Client probe did not start.' }
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        $process.StandardInput.WriteLine($Password)
        $process.StandardInput.Close()
        $timedOut = -not $process.WaitForExit(23000)
        if ($timedOut) { $process.Kill($true); $process.WaitForExit() }
        $out = $stdout.GetAwaiter().GetResult()
        $err = $stderr.GetAwaiter().GetResult()
        [IO.File]::WriteAllText((Join-Path $resultPath "probe-$Index.stdout.txt"), $out, $utf8)
        [IO.File]::WriteAllText((Join-Path $resultPath "probe-$Index.stderr.txt"), $err, $utf8)
        $sample = [ordered]@{ index = $Index; wrong_password = $WrongPassword; exit = $process.ExitCode; timed_out = $timedOut }
        $complete = $out -match '(?m)^complete=true\r?$'
        if ($WrongPassword) {
            $sample.passed = -not $timedOut -and $process.ExitCode -ne 0 -and -not $complete -and
                $out -notmatch '(?m)^connected_ms=' -and $err -match '(?im)^error:.*(密码认证失败|wrong.?password|authentication failed)'
        } else {
            $first = [regex]::Match($out, '(?m)^first_paint_ms=(\d+) size=(\d+)x(\d+)\r?$')
            $connected = [regex]::Match($out, '(?m)^connected_ms=(\d+)\r?$')
            $paint = [regex]::Match($out, '(?m)^complete_paint_ms=(\d+) generation=(\d+)\r?$')
            $sample.passed = -not $timedOut -and $process.ExitCode -eq 0 -and $complete -and
                $connected.Success -and $first.Success -and $paint.Success -and
                $out -match 'security: "VncAuth"' -and [int]$first.Groups[2].Value -gt 0 -and [int]$first.Groups[3].Value -gt 0
            if ($sample.passed) {
                $sample.connected_ms = [long]$connected.Groups[1].Value
                $sample.first_paint_ms = [long]$first.Groups[1].Value
                $sample.complete_paint_ms = [long]$paint.Groups[1].Value
                $sample.framebuffer = @([int]$first.Groups[2].Value, [int]$first.Groups[3].Value)
                $sample.security = 'VncAuth'; $sample.full_pixel_coverage = $true
            }
        }
        $record.samples += $sample
        Write-Evidence
        if (-not $sample.passed) { throw "Probe $Index failed its acceptance assertions; original stdout/stderr retained." }
    } finally {
        if ($started -and -not $process.HasExited) {
            $process.Kill($true)
            [void]$process.WaitForExit(5000)
        }
        $process.Dispose()
    }
}
try {
    Write-Phase 'Validate runner, probe and unused fixture port'
    $script:probePath = (Resolve-Path -LiteralPath $Probe).Path
    $record.probe_sha256 = (Get-FileHash -LiteralPath $probePath -Algorithm SHA256).Hash.ToLowerInvariant()
    $record.os = Get-CimInstance Win32_OperatingSystem | Select-Object Caption, Version, BuildNumber, OSArchitecture
    if (@(Get-Process winvnc -ErrorAction SilentlyContinue).Count -ne 0 -or
        @(Get-Service -Name '*uvnc*' -ErrorAction SilentlyContinue).Count -ne 0 -or
        @(Get-NetTCPConnection -LocalPort $port -ErrorAction SilentlyContinue).Count -ne 0 -or
        @(Get-NetUDPEndpoint -LocalPort $port -ErrorAction SilentlyContinue).Count -ne 0) {
        throw 'Refusing to share or replace an existing server/service/fixture port.'
    }
    $url = 'https://uvnc.eu/download/1800/UltraVNC_1830.zip'
    $expected = '11163a0b9b86321bf6403eb0cf84b81f7230ce32e6291377e1fe5beb987f7e4f'
    $archive = Join-Path $work 'UltraVNC_1830.zip'
    Write-Phase 'Download pinned official portable archive'
    Invoke-WebRequest -Uri $url -OutFile $archive -TimeoutSec 90
    $actual = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    $record.archive = @{ url = $url; expected_sha256 = $expected; actual_sha256 = $actual }
    if ($actual -ne $expected) { throw 'Official archive checksum mismatch; vendor code will not run.' }
    Expand-Archive -LiteralPath $archive -DestinationPath (Join-Path $work 'portable')
    $serverPath = Join-Path $work 'portable/x64/winvnc.exe'
    $serverFolder = Split-Path -Parent $serverPath
    $record.server = @{ version = [Diagnostics.FileVersionInfo]::GetVersionInfo($serverPath).FileVersion
        sha256 = (Get-FileHash -LiteralPath $serverPath -Algorithm SHA256).Hash.ToLowerInvariant(); mode = 'portable user application, never installed as service' }
    if ($record.server.version -notmatch '^1[.,]8[.,]3[.,]0(?:\D|$)') { throw 'Unexpected server file version in pinned archive.' }
    $ini = Join-Path $serverFolder 'ultravnc.ini'
    @"
[admin]
UseRegistry=0
SocketConnect=1
AutoPortSelect=0
PortNumber=$port
HTTPConnect=0
EnableTLS=0
LoopbackOnly=1
AllowLoopback=1
UseIpv6=0
AuthRequired=1
AuthHosts=-:+127.0.0.1
QuerySetting=2
QueryIfNoLogon=0
Secure=0
MSLogonRequired=0
RequireMSLogonIII=0
NewMSLogon=0
UseDSMPlugin=0
FileTransferEnabled=0
InputsEnabled=0
LocalInputsDisabled=0
BlankMonitorEnabled=0
LockSetting=0
RemoveWallpaper=0
RemoveEffects=0
RemoveFontSmoothing=0
AllowProperties=0
AllowEditClients=0
DisableTrayIcon=1
DebugMode=2
DebugLevel=9
path=$resultPath
[poll]
PollFullScreen=1
OnlyPollOnEvent=0
EnableDriver=0
EnableHook=0
EnableVirtual=0
[UltraVNC]
"@ | Set-Content -LiteralPath $ini -Encoding ascii
    # Protocol format only; no upstream implementation is copied. d3des uses
    # LSB-first key bits, so reverse each fixed-key byte for .NET's DES API.
    $fixed = [byte[]]@(23,82,107,6,35,78,88,7)
    $key = [byte[]]::new(8)
    for ($i = 0; $i -lt 8; $i++) {
        for ($bit = 0; $bit -lt 8; $bit++) { $key[$i] = $key[$i] -bor ((($fixed[$i] -shr $bit) -band 1) -shl (7-$bit)) }
    }
    $plain = [byte[]]::new(8)
    [Text.Encoding]::ASCII.GetBytes('adit-ci').CopyTo($plain, 0)
    $des = [Security.Cryptography.DES]::Create()
    try {
        $des.Key = $key; $des.Mode = [Security.Cryptography.CipherMode]::ECB; $des.Padding = [Security.Cryptography.PaddingMode]::None
        $transform = $des.CreateEncryptor()
        try { $encrypted = $transform.TransformFinalBlock($plain, 0, 8) } finally { $transform.Dispose() }
    } finally { $des.Dispose(); [Array]::Clear($plain); [Array]::Clear($key) }
    Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class AditVncFixtureIni {
    [DllImport("kernel32.dll", EntryPoint="WritePrivateProfileStructW", CharSet=CharSet.Unicode, SetLastError=true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool Write(string section, string key, byte[] value, uint size, string file);
    [DllImport("kernel32.dll", EntryPoint="GetPrivateProfileStructW", CharSet=CharSet.Unicode, SetLastError=true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool Read(string section, string key, [Out] byte[] value, uint size, string file);
}
'@
    if (-not [AditVncFixtureIni]::Write('UltraVNC', 'passwd', $encrypted, 8, $ini)) { throw 'Could not write native password structure.' }
    $readback = [byte[]]::new(8)
    if (-not [AditVncFixtureIni]::Read('UltraVNC', 'passwd', $readback, 8, $ini) -or
        [Convert]::ToHexString($readback) -ne [Convert]::ToHexString($encrypted)) { throw 'Native password structure readback failed.' }
    $record.password_format = 'Public synthetic adit-ci, zero-padded8; fixed-key VNC DES with reversed key bits; Win32 Write/GetPrivateProfileStruct8-byte readback verified; never argv/env.'
    Copy-Item -LiteralPath $ini -Destination (Join-Path $resultPath 'fixture.ini')
    $reservation = [Net.Sockets.Socket]::new([Net.Sockets.AddressFamily]::InterNetwork, [Net.Sockets.SocketType]::Dgram, [Net.Sockets.ProtocolType]::Udp)
    $reservation.ExclusiveAddressUse = $true
    $reservation.Bind([Net.IPEndPoint]::new([Net.IPAddress]::Loopback, $port))
    $record.udp_reservation = 'exclusive127.0.0.1:15911, no receive/send; blocks upstream wildcard echo if Windows exclusivity applies'
    Write-Phase 'Start portable user-mode server with explicit private INI; no installer/service/firewall action'
    $server = Start-Process -FilePath $serverPath -ArgumentList @('-config', ('"' + $ini + '"'), '-run') -WorkingDirectory $serverFolder -WindowStyle Hidden -PassThru
    $record.server.pid = $server.Id
    $ready = $false
    $deadline = [DateTime]::UtcNow.AddSeconds(25)
    while ([DateTime]::UtcNow -lt $deadline) {
        $server.Refresh()
        if ($server.HasExited) { throw "Vendor process exited before listener readiness: $($server.ExitCode)" }
        if (Assert-OwnedListeners) { $ready = $true; break }
        Start-Sleep -Milliseconds 250
    }
    if (-not $ready) { throw 'No owned loopback RFB listener within25s; startup is not acceptance.' }
    for ($sample = 1; $sample -le 3; $sample++) {
        Write-Phase "Read-only full framebuffer sample $sample of3"
        [void](Assert-OwnedListeners)
        Invoke-Probe $sample 'adit-ci' $false
    }
    Write-Phase 'Wrong password must explicitly fail authentication, not merely time out'
    Invoke-Probe 4 'wrong-ci' $true
    [void](Assert-OwnedListeners)
    $record.outcome = 'passed'
} catch {
    $failed = $true
    $record.error = $_.Exception.Message
    Write-Phase ('FAILED: ' + $_.Exception.Message)
} finally {
    if ($null -ne $server) {
        try {
            $server.Refresh()
            if (-not $server.HasExited) {
                if ($server.Path -ne $serverPath) { throw 'Owned process identity changed; cleanup refused.' }
                Stop-Process -InputObject $server -Force
                [void]$server.WaitForExit(5000)
            }
            $record.server_cleanup = 'owned vendor process exited'
        } catch { $failed = $true; $record.outcome = 'failed'; $record.cleanup_error = $_.Exception.Message }
        $server.Dispose()
    }
    if ($null -ne $reservation) { $reservation.Dispose() }
    # All temporary vendor files belong to this ephemeral runner; no user path is
    # deleted and no service or firewall configuration was installed or changed.
    Write-Evidence
}
if ($failed) { throw 'Windows VNC smoke failed; inspect retained result and raw logs.' }
