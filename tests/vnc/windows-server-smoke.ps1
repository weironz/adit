# This fixture runs only on a disposable GitHub-hosted Windows runner.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Probe,
    [Parameter(Mandatory)][string]$Results,
    [ValidateRange(1,10)][int]$Samples = 3,
    [ValidateSet('static','scroll','drag')][string]$Workload = 'static',
    [ValidateSet('lossless','wan')][string]$Quality = 'lossless',
    [switch]$SyntheticLoad,
    [switch]$SharedSoak,
    [switch]$ServiceMode,
    [ValidateRange(1,28800)][int]$DurationSeconds = 7200,
    [ValidateRange(1,600)][int]$IdleSeconds = 120,
    [ValidateSet(15911,15912)][int]$ProbePort = 15911,
    [string]$GuestNetworkEvidence
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (-not $IsWindows -or $env:GITHUB_ACTIONS -ne 'true' -or
    $env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows' -or
    -not $env:RUNNER_NAME -or -not $env:RUNNER_TEMP -or -not $env:GITHUB_SHA) {
    throw 'Only a disposable GitHub-hosted Windows runner may execute this vendor fixture.'
}
if ($ServiceMode -and ($SharedSoak -or $ProbePort -ne 15911 -or -not [string]::IsNullOrWhiteSpace($GuestNetworkEvidence) -or -not $SyntheticLoad)) { throw 'ServiceMode requires the owned synthetic target and excludes Guest/SharedSoak.' }
if (-not $SyntheticLoad -and $Workload -ne 'static') { throw 'Input workloads require the owned synthetic target.' }
if (($ProbePort -eq 15912) -ne (-not [string]::IsNullOrWhiteSpace($GuestNetworkEvidence))) {
    throw 'Guest port15912 requires its owned fresh network evidence; baseline15911 must not use guest evidence.'
}
if ($SharedSoak -and $ProbePort -ne 15911) { throw 'SharedSoak does not accept guest-network mode.' }
if ($SharedSoak -and (-not $SyntheticLoad -or $Workload -ne 'static' -or $Quality -ne 'lossless')) {
    throw 'SharedSoak requires the owned synthetic target, static workload and lossless quality.'
}
$resultPath = [IO.Path]::GetFullPath($Results)
$runnerTemp = [IO.Path]::GetFullPath($env:RUNNER_TEMP).TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
if (-not $resultPath.StartsWith($runnerTemp, [StringComparison]::OrdinalIgnoreCase) -or
    (Test-Path -LiteralPath $resultPath)) {
    throw 'Choose a fresh results directory beneath RUNNER_TEMP; existing evidence is never overwritten.'
}
[void][IO.Directory]::CreateDirectory($resultPath)
$work = Join-Path $env:RUNNER_TEMP ('adit-ultravnc-' + [guid]::NewGuid().ToString('N'))
if ($ServiceMode -and $work -match '\s') { throw 'Service calibration requires a fresh executable/config path without whitespace.' }
[void][IO.Directory]::CreateDirectory($work)
$utf8 = [Text.UTF8Encoding]::new($false)
$port = 15911
$server = $null
$loadProcess = $null
$pythonPath = $null
$loadScript = $null
$loadStateDir = Join-Path $work 'synthetic-state'
$firewallRuleName = 'AditVncCiUdpBlock-' + [guid]::NewGuid().ToString('N')
$firewallRuleOwned = $false
$failed = $false
$serviceName = 'AditVncCi_' + [guid]::NewGuid().ToString('N')
$serviceInstallAttempted = $false
$serviceRootIdentity = $null
$serviceWorkers = @{}
$serviceIni = $null
$serviceLaunchUtc = $null
$serviceConsoleSession = $null
$source = '0cd8b41aeb34e52e170b33e24411b3f805e303be'
$record = [ordered]@{
    outcome = 'failed'; source_sha = $env:GITHUB_SHA; run_id = $env:GITHUB_RUN_ID
    scope = "UltraVNC1.8.3.0 portable Windows hosted CI; $Samples $Quality/$Workload loopback samples plus1 wrong-password rejection; not impaired-network or service acceptance"
    sample_count_requested = $Samples; workload = $Workload; quality = $Quality; synthetic_load = [bool]$SyntheticLoad
    network = 'native loopback baseline; no RTT/bandwidth/loss shaping'
    runner = $env:RUNNER_NAME; os = [Environment]::OSVersion.VersionString
    target = "127.0.0.1:$ProbePort"; vendor_listener = "127.0.0.1:$port"; samples = @(); configuration_reference_commit = $source
    configuration_reference_note = 'Pinned official reference source explains INI/password/CLI semantics; it is not a claim that the downloaded vendor binary was built from this commit.'
    previous_failed_run = 'https://github.com/weironz/adit/actions/runs/39132780089'
    previous_failure_note = 'Exact pinned archive started a wildcard UDP listener despite exclusive loopback reservation. No client sample ran; failure remains failed evidence, not compatibility acceptance.'
    sources = @(
        'https://uvnc.com/downloads/ultravnc/172-ultravnc-1-8-3-0.html',
        'https://uvnc.com/docs/ultravnc-server/69-ultravnc-ini.html',
        'https://uvnc.com/docs/ultravnc-server/51-ultravnc-server-commandline-parameters.html',
        "https://github.com/ultravnc/UltraVNC/blob/$source/common/inifile.cpp",
        "https://github.com/ultravnc/UltraVNC/blob/$source/winvnc/winvnc/vncauth.c",
        "https://github.com/ultravnc/UltraVNC/blob/$source/winvnc/winvnc/winvnc.cpp",
        "https://github.com/ultravnc/UltraVNC/blob/$source/winvnc/winvnc/UdpEchoServer.cpp",
        'https://learn.microsoft.com/en-us/powershell/module/netsecurity/new-netfirewallrule',
        'https://learn.microsoft.com/en-us/powershell/module/netsecurity/get-netfirewallrule'
    )
    limitations = @(
        'Not a Windows workstation/service, UAC, lock/unlock, keyboard, clipboard, DPI, sleep or physical-interface acceptance.',
        'Loopback samples are a baseline, not the Windows weak-network matrix, latency percentiles or physical input acceptance.',
        'Full framebuffer means opaque decoded pixels in production SessionManager, not GPU presentation. SyntheticLoad additionally requires decoded public canary colors and real Tk input consumption.',
        'VncAuth is not transport encryption. Only a public synthetic fixture password is used.',
        'TCP RFB is loopback-only, but the official binary also binds a wildcard UDP echo on15911 independently of LoopbackOnly. This disposable CI adds one temporary exact-program/UDP-port inbound Block rule on all enabled firewall profiles; not all vendor sockets are loopback.',
        'The temporary UDP firewall rule is a CI containment measure, not a tested portable production configuration. No existing rule or profile is modified, no service is installed, and no user VM firewall is touched.',
        'The hosted runner may not expose a capturable interactive desktop. A startup/listener alone never passes this test.'
    )
}
if ($ServiceMode) {
    $record.scope = "UltraVNC1.8.3.0 self-owned Windows service; $Samples $Quality/$Workload loopback basic samples; service-to-worker RFB attribution required; not UAC/lock or weak-network acceptance"
    $record.service_name = $serviceName
    $record.service_acceptance = $false
    $record.expected_worker_identity = 'Direct SCM-root child via LaunchProcessWin/CreateProcessAsUser using GetSessionUserTokenWin/get_winlogon_handle duplicated winlogon primary token; SYSTEM SID and active-console SessionId required. TerminalServer IPC fallback is outside this calibration and must fail attribution.'
    $record.limitations[0] = 'Only uniquely owned SCM service plus its attributed worker/real synthetic desktop; no UAC, lock/unlock, login, keyboard, clipboard, DPI, sleep or physical-interface acceptance.'
    $record.limitations[5] = 'One fresh GUID service and exact executable UDP block are created only in disposable hosted CI; existing services, rules, profiles and system protection are not changed.'
    $record.sources += "https://github.com/ultravnc/UltraVNC/blob/$source/winvnc/winvnc/UltraVNCService.cpp"
}
if ($SharedSoak) {
    $record.scope = "Two concurrent production clients on UltraVNC1.8.3.0 Windows loopback; requested${DurationSeconds}s shared stability with${IdleSeconds}s no-input windows; not impaired-network or service acceptance"
    $record.shared_soak_requested = $true
    $record.duration_seconds_requested = $DurationSeconds
    $record.idle_seconds_requested = $IdleSeconds
    $record.long_term_acceptance = $false
    $record.sample_count_requested = 0
}
function Write-Evidence {
    [IO.File]::WriteAllText((Join-Path $resultPath 'result.json'), ($record | ConvertTo-Json -Depth 12) + "`n", $utf8)
}
function Assert-GuestNetwork {
    if ($ProbePort -eq 15911) { return }
    $evidencePath = [IO.Path]::GetFullPath($GuestNetworkEvidence)
    if (-not [IO.Path]::IsPathFullyQualified($GuestNetworkEvidence) -or
        -not $evidencePath.StartsWith($runnerTemp, [StringComparison]::OrdinalIgnoreCase) -or
        -not (Test-Path -LiteralPath $evidencePath -PathType Leaf)) { throw 'Guest evidence must be an absolute existing file under fresh RUNNER_TEMP.' }
    $item = Get-Item -LiteralPath $evidencePath
    $age = ([DateTime]::UtcNow - $item.CreationTimeUtc).TotalSeconds
    if ($age -lt -5 -or $age -gt 900) { throw 'Guest evidence is not fresh for this runner invocation.' }
    $evidence = [IO.File]::ReadAllText($evidencePath, $utf8) | ConvertFrom-Json -AsHashtable
    $qemu = Get-Process -Id ([int]$evidence.qemu_pid) -ErrorAction Stop
    $qemuInfo = Get-CimInstance Win32_Process -Filter "ProcessId=$($qemu.Id)" -ErrorAction Stop
    $qemuPath = [IO.Path]::GetFullPath([string]$qemu.Path)
    if ($evidence.protocol -ne 'adit-guest-netem-v1' -or $evidence.source_sha -ne $env:GITHUB_SHA -or
        $evidence.run_id -ne $env:GITHUB_RUN_ID -or $evidence.probe_port -ne 15912 -or $evidence.vendor_port -ne 15911 -or
        $evidence.mode -notin @('impaired','baseline') -or -not $evidence.calibration_passed -or
        $evidence.guest_manifest.source_sha -ne $env:GITHUB_SHA -or
        -not $qemuPath.StartsWith($runnerTemp, [StringComparison]::OrdinalIgnoreCase) -or
        -not [string]::Equals($qemuPath, [string]$evidence.qemu_path, [StringComparison]::OrdinalIgnoreCase) -or
        (Get-FileHash -LiteralPath $qemuPath -Algorithm SHA256).Hash.ToLowerInvariant() -ne $evidence.qemu_sha256 -or
        $qemuInfo.CommandLine -notlike '*tcg,thread=multi*' -or
        $qemuInfo.CommandLine -notlike '*restrict=on*' -or
        $qemuInfo.CommandLine -notlike '*hostfwd=tcp:127.0.0.1:15912-:15912*' -or
        $qemuInfo.CommandLine -notlike '*guestfwd=tcp:10.0.2.100:15911-tcp:127.0.0.1:15911*') {
        throw 'Guest evidence does not prove the same-source live owned QEMU mapping.'
    }
    $forward = @(Get-NetTCPConnection -OwningProcess $qemu.Id -State Listen -LocalPort 15912 -ErrorAction Stop)
    if ($forward.Count -ne 1 -or $forward[0].LocalAddress -ne '127.0.0.1') { throw 'Guest probe port is not the exact live loopback QEMU listener.' }
    $netem = @($evidence.before.stats.qdiscs | Where-Object { $_.kind -eq 'netem' })
    if ($evidence.mode -eq 'impaired') {
        if ($netem.Count -ne 1 -or [Math]::Abs([double]$netem[0].options.delay.delay - 0.09) -gt 0.000001 -or
            $netem[0].options.rate.rate -ne 625000 -or
            [Math]::Abs([double]$netem[0].options.'loss-random'.loss - 0.001) -gt 0.000001 -or
            $netem[0].options.limit -ne 20000) { throw 'Guest evidence lacks actual90ms/5mbit/0.1% tc configuration.' }
    } elseif ($netem.Count -ne 0) { throw 'Restored baseline still reports netem.' }
    $record.scope = "UltraVNC1.8.3.0 Windows portable via calibrated Linux guest egress $($evidence.mode); $Samples $Quality/$Workload full-RGB samples plus wrong-password rejection; TCP terminating Slirp/relay, not physical NIC or OS sleep"
    $record.network = $evidence
    $record.limitations[1] = 'Guest eth0 egress impairment across TCP-terminating Slirp/relay; Windows loopback TCP segments are not impaired. Not transparent Windows TCP-loss, physical LAN/NIC, native GUI or OS sleep acceptance.'
    [IO.File]::WriteAllText((Join-Path $resultPath 'guest-network.json'), ($evidence | ConvertTo-Json -Depth 18) + "`n", $utf8)
}
function Get-GuestSnapshot([string]$Name) {
    if ($ProbePort -eq 15911) { return $null }
    $evidence = $record.network
    if ($evidence.control_port -ne 15914) { throw 'Unexpected owned snapshot control port.' }
    $client = [Net.Sockets.TcpClient]::new()
    try {
        if (-not $client.ConnectAsync('127.0.0.1', 15914).Wait(3000)) { throw 'Owned snapshot connection deadline.' }
        $stream = $client.GetStream(); $stream.ReadTimeout = 25000; $stream.WriteTimeout = 3000
        $request = @{ protocol='adit-guest-netem-v1'; controller_id=$evidence.controller_id; command='snapshot' } | ConvertTo-Json -Compress
        $bytes = $utf8.GetBytes($request + "`n"); $stream.Write($bytes, 0, $bytes.Length)
        $reader = [IO.StreamReader]::new($stream, $utf8)
        $line = $reader.ReadLine()
        if ([string]::IsNullOrEmpty($line) -or $line.Length -gt 100000) { throw 'Missing or oversized guest snapshot response.' }
        $snapshot = $line | ConvertFrom-Json -AsHashtable
        if (-not $snapshot.ok -or $snapshot.source_sha -ne $env:GITHUB_SHA -or $snapshot.qemu_pid -ne $evidence.qemu_pid) {
            throw 'Snapshot does not belong to this source and live QEMU.'
        }
        [IO.File]::WriteAllText((Join-Path $resultPath $Name), $line + "`n", $utf8)
        return $snapshot.snapshot
    } finally { $client.Dispose() }
}
function Write-Phase([string]$Message) {
    $line = "{0:o} {1}" -f [DateTimeOffset]::UtcNow, $Message
    [IO.File]::AppendAllText((Join-Path $resultPath 'runner.log'), $line + "`n", $utf8)
    Write-Host $line
}
function Assert-UdpFirewallBlock {
    $profiles = @(Get-NetFirewallProfile -PolicyStore ActiveStore -ErrorAction Stop)
    $rule = @(Get-NetFirewallRule -PolicyStore ActiveStore -Name $firewallRuleName -ErrorAction Stop)
    if ($rule.Count -ne 1) { throw 'Owned UDP block is not unique in the effective firewall policy.' }
    $ports = @($rule[0] | Get-NetFirewallPortFilter -ErrorAction Stop)
    $applications = @($rule[0] | Get-NetFirewallApplicationFilter -ErrorAction Stop)
    $services = @(Get-Service -Name MpsSvc, BFE -ErrorAction Stop)
    $snapshot = [ordered]@{
        rule = $rule | Select-Object Name, Enabled, Direction, Action, Profile, PrimaryStatus, EnforcementStatus, PolicyStoreSourceType
        ports = @($ports | Select-Object Protocol, LocalPort, RemotePort)
        applications = @($applications | Select-Object Program)
        profiles = @($profiles | Select-Object Name, Enabled, AllowLocalFirewallRules)
        services = @($services | Select-Object Name, Status)
    }
    [IO.File]::WriteAllText((Join-Path $resultPath 'udp-firewall.json'), ($snapshot | ConvertTo-Json -Depth 8) + "`n", $utf8)
    foreach ($profileName in @('Domain', 'Private', 'Public')) {
        $profile = @($profiles | Where-Object { $_.Name -eq $profileName })
        if ($profile.Count -ne 1 -or [string]$profile[0].Enabled -ne 'True' -or
            [string]$profile[0].AllowLocalFirewallRules -eq 'False') {
            throw "Cannot establish enabled effective firewall and permitted local rules for $profileName profile."
        }
    }
    if ($services.Count -ne 2 -or @($services | Where-Object { $_.Status -ne 'Running' }).Count -ne 0 -or
        [string]$rule[0].Enabled -ne 'True' -or [string]$rule[0].Direction -ne 'Inbound' -or
        [string]$rule[0].Action -ne 'Block' -or [string]$rule[0].Profile -ne 'Any' -or
        [string]$rule[0].PrimaryStatus -ne 'OK' -or
        $ports.Count -ne 1 -or [string]$ports[0].Protocol -notin @('UDP', '17') -or
        @($ports[0].LocalPort).Count -ne 1 -or [string]$ports[0].LocalPort -ne [string]$port -or
        $applications.Count -ne 1 -or
        -not [string]::Equals([string]$applications[0].Program, $serverPath, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Owned effective firewall rule does not prove the exact enabled program/UDP15911 inbound block.'
    }
    $record.udp_firewall_rule = $firewallRuleName
}
function Get-ServiceProcessIdentity([int]$ProcessId) {
    $process = Get-CimInstance Win32_Process -Filter "ProcessId=$ProcessId" -ErrorAction Stop
    if ($null -eq $process) { throw "Owned service process $ProcessId disappeared during identity check." }
    $sid = Invoke-CimMethod -InputObject $process -MethodName GetOwnerSid -ErrorAction Stop
    if ($sid.ReturnValue -ne 0 -or $sid.Sid -ne 'S-1-5-18' -or
        -not [string]::Equals([string]$process.ExecutablePath, $serverPath, [StringComparison]::OrdinalIgnoreCase) -or
        (Get-FileHash -LiteralPath $process.ExecutablePath -Algorithm SHA256).Hash.ToLowerInvariant() -ne $record.server.sha256) {
        throw "Service process $ProcessId is not the owned pinned LocalSystem executable."
    }
    return [ordered]@{ pid=[int]$process.ProcessId; parent_pid=[int]$process.ParentProcessId
        executable=[string]$process.ExecutablePath; command_line=[string]$process.CommandLine
        created_utc=([DateTime]$process.CreationDate).ToUniversalTime().ToString('o')
        session_id=[int]$process.SessionId; owner_sid=[string]$sid.Sid; sha256=$record.server.sha256 }
}
function Get-OwnedService {
    $service = Get-CimInstance Win32_Service -Filter "Name='$serviceName'" -ErrorAction Stop
    if ($null -eq $service) { return $null }
    $expected = '"' + $serverPath + '" -service'
    if ($service.Name -ne $serviceName -or $service.Name -notmatch '^AditVncCi_[0-9a-f]{32}$' -or
        $service.PathName -cne $expected -or $service.StartName -ne 'LocalSystem') {
        throw 'SCM service identity/path/account differs from this exact owned registration; no service action permitted.'
    }
    return $service
}
function Capture-ServiceProcesses([switch]$DuringStop) {
    $service = Get-OwnedService
    if (-not $DuringStop -and $null -ne $service -and $service.ProcessId -gt 0) {
        $root = Get-ServiceProcessIdentity ([int]$service.ProcessId)
        if ($root.session_id -ne 0 -or [DateTime]$root.created_utc -lt $serviceLaunchUtc -or
            $root.command_line -notmatch ('^"?' + [regex]::Escape($serverPath) + '"? -service$')) {
            throw 'SCM root process does not prove newly started service mode in session0.'
        }
        if ($null -ne $script:serviceRootIdentity -and
            ($root.pid -ne $script:serviceRootIdentity.pid -or $root.created_utc -ne $script:serviceRootIdentity.created_utc)) {
            throw 'SCM root process changed unexpectedly during acceptance.'
        }
        $script:serviceRootIdentity = $root
        $record.service_captured_root = $root
    }
    if ($null -eq $serviceRootIdentity) { return }
    $root = $serviceRootIdentity
    $workers = @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$($root.pid)" -ErrorAction Stop |
        Where-Object { [string]::Equals([string]$_.ExecutablePath, $serverPath, [StringComparison]::OrdinalIgnoreCase) })
    $parentAlive = $false
    if (@($workers | Where-Object { -not $serviceWorkers.ContainsKey([string]$_.ProcessId) }).Count -gt 0) {
        $parentAlive = Test-OwnedProcessStillSame $root
    }
    foreach ($candidate in $workers) {
        try { $worker = Get-ServiceProcessIdentity ([int]$candidate.ProcessId) }
        catch {
            if ($DuringStop -and $null -eq (Get-CimInstance Win32_Process -Filter "ProcessId=$($candidate.ProcessId)" -ErrorAction Stop)) { continue }
            throw
        }
        if ([DateTime]$worker.created_utc -lt [DateTime]$root.created_utc) { throw 'Worker creation predates its attributed SCM parent.' }
        $key = [string]$worker.pid
        if ($script:serviceWorkers.ContainsKey($key) -and $script:serviceWorkers[$key].created_utc -ne $worker.created_utc) { throw 'Worker PID was reused.' }
        if (-not $script:serviceWorkers.ContainsKey($key) -and -not $parentAlive) { throw 'An unrecorded child outlived its root before ancestry could be proven; forced cleanup is not authorized.' }
        # Capture identity before testing desktop/config semantics, so a failed
        # readiness check can still retire this exact child after SCM stop.
        $script:serviceWorkers[$key] = $worker
        $record.service_captured_workers = @($serviceWorkers.Values)
    }
}
function Assert-ServiceWorker {
    if ([AditVncFixtureConsole]::WTSGetActiveConsoleSessionId() -ne $serviceConsoleSession -or
        $loadProcess.SessionId -ne $serviceConsoleSession) { throw 'Active console/Tk session changed during service acceptance.' }
    Capture-ServiceProcesses
    $service = Get-OwnedService
    if ($null -eq $service -or $service.State -ne 'Running' -or $service.ProcessId -eq 0 -or $null -eq $serviceRootIdentity) { return $false }
    $root = $serviceRootIdentity
    $workers = @($serviceWorkers.Values | Where-Object { Test-OwnedProcessStillSame $_ })
    if ($workers.Count -eq 0) { return $false }
    if ($workers.Count -ne 1) { throw 'Expected exactly one service-generated RFB worker, without helper or fallback instances.' }
    $worker = $workers[0]
    if ($worker.session_id -ne $serviceConsoleSession -or $worker.parent_pid -ne $root.pid -or
        [DateTime]$worker.created_utc -lt [DateTime]$root.created_utc -or
        $worker.command_line -notmatch ('^"?' + [regex]::Escape($serverPath) + '"? -config "' + [regex]::Escape($serviceIni) + '" -service_run$')) {
        throw 'Worker session, ancestry, creation or exact private INI command line is not the expected service child.'
    }
    if ($null -eq $script:server) { $script:server = Get-Process -Id $worker.pid -ErrorAction Stop }
    elseif ($script:server.Id -ne $worker.pid) { throw 'Service worker changed during the measured connection; no silent restart accepted.' }
    $record.service_identity = [ordered]@{
        scm=@{ name=$service.Name; path=$service.PathName; state=$service.State; start_name=$service.StartName; pid=$service.ProcessId }
        root=$root; worker=$worker; ini=$serviceIni; portable_marker=(Join-Path $serverFolder 'ultravnc.portable')
    }
    [IO.File]::WriteAllText((Join-Path $resultPath 'service-identity.json'), ($record.service_identity | ConvertTo-Json -Depth 8) + "`n", $utf8)
    return $true
}
function Test-OwnedProcessStillSame($Identity) {
    $process = Get-CimInstance Win32_Process -Filter "ProcessId=$($Identity.pid)" -ErrorAction Stop
    if ($null -eq $process) { return $false }
    try { $actual = Get-ServiceProcessIdentity ([int]$Identity.pid) }
    catch {
        if ($null -eq (Get-CimInstance Win32_Process -Filter "ProcessId=$($Identity.pid)" -ErrorAction Stop)) { return $false }
        throw
    }
    if ($actual.created_utc -ne $Identity.created_utc -or $actual.command_line -cne $Identity.command_line -or
        $actual.parent_pid -ne $Identity.parent_pid -or $actual.session_id -ne $Identity.session_id) { throw 'Recorded process identity changed; cleanup refused.' }
    return $true
}
function Invoke-OwnedScControl([ValidateSet('stop','delete')][string]$Control) {
    [void](Get-OwnedService)
    $info = [Diagnostics.ProcessStartInfo]::new((Join-Path $env:SystemRoot 'System32/sc.exe'))
    $info.UseShellExecute = $false; $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
    $info.ArgumentList.Add($Control); $info.ArgumentList.Add($serviceName)
    $process = [Diagnostics.Process]::new(); $process.StartInfo = $info
    try {
        if (-not $process.Start()) { throw "SCM $Control command failed to start." }
        $stdout = $process.StandardOutput.ReadToEndAsync(); $stderr = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit(5000)) {
            $process.Kill(); [void]$process.WaitForExit(5000)
            throw "Exact SCM $Control command exceeded5s."
        }
        [IO.File]::WriteAllText((Join-Path $resultPath "service-$Control.stdout.txt"), $stdout.GetAwaiter().GetResult(), $utf8)
        [IO.File]::WriteAllText((Join-Path $resultPath "service-$Control.stderr.txt"), $stderr.GetAwaiter().GetResult(), $utf8)
        if ($process.ExitCode -ne 0) { throw "Exact SCM $Control command returned $($process.ExitCode)." }
    } finally { $process.Dispose() }
}
function Stop-OwnedService {
    $service = Get-OwnedService
    $abnormal = $null -ne $serviceRootIdentity -and ($null -eq $service -or $service.State -eq 'Stopped')
    # Attribution failures must fail acceptance, but must not bypass a safe
    # stop of the exact GUID/path registration that this invocation created.
    try { Capture-ServiceProcesses }
    catch { $record.service_capture_cleanup_error = $_.Exception.Message; $abnormal = $true }
    if ($null -ne $service -and $service.State -ne 'Stopped') {
        try { Invoke-OwnedScControl 'stop' }
        catch { $record.service_stop_error = $_.Exception.Message; $abnormal = $true }
    }
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    $remaining = @()
    do {
        $service = Get-OwnedService
        try { Capture-ServiceProcesses -DuringStop }
        catch { $record.service_capture_cleanup_error = $_.Exception.Message; $abnormal = $true }
        $remaining = @()
        $identities = @($serviceWorkers.Values)
        if ($null -ne $serviceRootIdentity) { $identities += $serviceRootIdentity }
        foreach ($identity in $identities) {
            if (Test-OwnedProcessStillSame $identity) { $remaining += $identity }
        }
        if (($null -eq $service -or $service.State -eq 'Stopped') -and $remaining.Count -eq 0) { break }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    $abnormal = $abnormal -or $remaining.Count -gt 0 -or ($null -ne $service -and $service.State -ne 'Stopped')
    if ($abnormal) {
        $record.service_abnormal_cleanup = 'SCM stop did not retire every attributed process; exact recorded identities only are eligible for forced cleanup, acceptance failed.'
        $script:failed = $true; $record.outcome = 'failed'; $record.service_acceptance = $false
        foreach ($identity in $remaining) {
            if (Test-OwnedProcessStillSame $identity) {
                Stop-Process -Id ([int]$identity.pid) -Force -ErrorAction Stop
                $owned = Get-Process -Id ([int]$identity.pid) -ErrorAction SilentlyContinue
                if ($null -ne $owned -and -not $owned.WaitForExit(5000)) { throw 'An exactly recorded service-owned process survived forced cleanup.' }
            }
        }
    }
    foreach ($identity in $identities) {
        if (Test-OwnedProcessStillSame $identity) { throw 'Recorded service process remains after cleanup.' }
    }
    $tcpRemaining = @(Get-NetTCPConnection -LocalPort $port -ErrorAction SilentlyContinue)
    $record.service_closed_tcp = @($tcpRemaining | Select-Object LocalAddress, LocalPort, RemoteAddress, RemotePort, State, OwningProcess)
    if (@(Get-CimInstance Win32_Process -Filter "Name='winvnc.exe'" -ErrorAction Stop | Where-Object { [string]::Equals([string]$_.ExecutablePath, $serverPath, [StringComparison]::OrdinalIgnoreCase) }).Count -ne 0 -or
        @($tcpRemaining | Where-Object { $_.State -ne 'TimeWait' -or $_.OwningProcess -ne 0 }).Count -ne 0 -or
        @(Get-NetUDPEndpoint -LocalPort $port -ErrorAction SilentlyContinue).Count -ne 0) { throw 'Service worker or TCP/UDP port remains; service deletion and firewall removal refused.' }
    $service = Get-OwnedService
    if ($null -ne $service) {
        if ($service.State -ne 'Stopped') { throw 'Owned service is not stopped before deletion.' }
        Invoke-OwnedScControl 'delete'
    }
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    do {
        $exists = Get-CimInstance Win32_Service -Filter "Name='$serviceName'" -ErrorAction Stop
        $keyExists = Test-Path -LiteralPath "Registry::HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Services\$serviceName"
        if ($null -eq $exists -and -not $keyExists) { break }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    if ($null -ne $exists -or $keyExists) { throw 'Exact service still exists in SCM or registry after deletion.' }
    $record.service_cleanup = 'Exact GUID SCM registration and key absent; attributed root/workers and all live TCP/UDP endpoints absent before protective-rule removal; only recorded PID0 TIME_WAIT may remain as closed kernel state'
}
function Assert-OwnedListeners {
    if ($ServiceMode -and -not (Assert-ServiceWorker)) { return $false }
    $tcp = @(Get-NetTCPConnection -OwningProcess $server.Id -State Listen -ErrorAction SilentlyContinue)
    $udp = @(Get-NetUDPEndpoint -OwningProcess $server.Id -ErrorAction SilentlyContinue)
    $snapshot = [ordered]@{
        tcp = @($tcp | Select-Object LocalAddress, LocalPort, OwningProcess)
        udp = @($udp | Select-Object LocalAddress, LocalPort, OwningProcess)
    }
    [IO.File]::WriteAllText((Join-Path $resultPath 'listeners.json'), ($snapshot | ConvertTo-Json -Depth 5) + "`n", $utf8)
    foreach ($endpoint in $tcp) {
        if ($endpoint.LocalAddress -notin @('127.0.0.1', '::1') -or $endpoint.LocalPort -ne $port) {
            throw "Unexpected vendor TCP endpoint: $($endpoint.LocalAddress):$($endpoint.LocalPort)"
        }
    }
    foreach ($endpoint in $udp) {
        if ($endpoint.LocalPort -ne $port -or $endpoint.LocalAddress -notin @('0.0.0.0', '127.0.0.1')) {
            throw "Unexpected vendor UDP endpoint: $($endpoint.LocalAddress):$($endpoint.LocalPort)"
        }
    }
    Assert-UdpFirewallBlock
    return @($tcp | Where-Object { $_.LocalAddress -eq '127.0.0.1' -and $_.LocalPort -eq $port }).Count -gt 0
}
function Read-LoadState([string]$Name) {
    $path = Join-Path $loadStateDir $Name
    if (-not (Test-Path -LiteralPath $path)) { return $null }
    try { return [IO.File]::ReadAllText($path, $utf8) | ConvertFrom-Json -AsHashtable }
    catch [IO.IOException] { return $null }
}
function Assert-LoadAlive {
    $loadProcess.Refresh()
    if ($loadProcess.HasExited) { throw "Synthetic target exited: $($loadProcess.ExitCode); inspect target stdout/stderr." }
}
function Reset-SyntheticLoad([int]$Index) {
    Assert-LoadAlive
    $state = Read-LoadState 'state.json'
    if ($null -eq $state) { throw 'Synthetic target has no readable state before reset.' }
    $generation = [long]$state.reset_generation
    [IO.File]::WriteAllText((Join-Path $loadStateDir 'reset.request'), "$Index`n", $utf8)
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    while ([DateTime]::UtcNow -lt $deadline) {
        Assert-LoadAlive
        $ack = Read-LoadState 'reset-ack.json'
        if ($null -ne $ack -and [long]$ack.reset_generation -gt $generation) {
            if (-not $ack.ready -or $ack.closed -or -not $ack.focused -or
                @($ack.counts.Values | Where-Object { $_ -ne 0 }).Count -ne 0) { throw 'Synthetic reset did not restore focused, empty input counters.' }
            [IO.File]::WriteAllText((Join-Path $resultPath "load-$Index.reset.json"), ($ack | ConvertTo-Json -Depth 8) + "`n", $utf8)
            return [long]$ack.reset_generation
        }
        Start-Sleep -Milliseconds 50
    }
    throw 'Synthetic reset acknowledgement did not advance within5s.'
}
function Test-ConsumedInput($State, [long]$Generation) {
    if ($null -eq $State -or -not $State.ready -or $State.closed -or [long]$State.reset_generation -ne $Generation) { return $false }
    $counts = $State.counts
    switch ($Workload) {
        'static' { return @($counts.Values | Where-Object { $_ -ne 0 }).Count -eq 0 }
        'scroll' { return $counts.wheel_events -gt 0 -and $counts.viewport_changes -gt 0 }
        'drag' { return $counts.button_press -gt 0 -and $counts.drag_motion -gt 0 -and $counts.button_release -gt 0 -and $counts.viewport_changes -gt 0 }
    }
    return $false
}
function Invoke-SharedSoak {
    Assert-LoadAlive
    $initial = Read-LoadState 'state.json'
    if ($null -eq $initial -or $initial.shared_probe.protocol -ne 'adit-public-shared-v1' -or
        $initial.shared_probe.sequence -ne 0 -or $initial.shared_probe.clicks -ne 0) {
        throw 'Shared test must begin with the fresh owned public marker.'
    }
    $info = [Diagnostics.ProcessStartInfo]::new($script:probePath)
    $info.UseShellExecute = $false; $info.CreateNoWindow = $true
    $info.RedirectStandardInput = $true; $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
    $info.StandardOutputEncoding = $utf8; $info.StandardErrorEncoding = $utf8
    foreach ($argument in @("--port=$port", "--state-dir=$loadStateDir", "--target-pid=$($loadProcess.Id)",
        "--duration-seconds=$DurationSeconds", "--idle-seconds=$IdleSeconds")) { $info.ArgumentList.Add($argument) }
    $process = [Diagnostics.Process]::new(); $process.StartInfo = $info
    $started = $false
    $stdoutPath = Join-Path $resultPath 'shared.stdout.jsonl'
    $stderrPath = Join-Path $resultPath 'shared.stderr.txt'
    $stderr = $null
    $events = [Collections.Generic.List[object]]::new()
    $snapshotClock = [Diagnostics.Stopwatch]::StartNew()
    $deadline = [DateTime]::UtcNow.AddSeconds($DurationSeconds + $IdleSeconds + 120)
    $timedOut = $false
    try {
        $started = $process.Start()
        if (-not $started) { throw 'Shared production probe did not start.' }
        $stderr = $process.StandardError.ReadToEndAsync()
        $process.StandardInput.WriteLine('adit-ci')
        $process.StandardInput.Close()
        $lineTask = $process.StandardOutput.ReadLineAsync()
        $eof = $false
        while (-not $eof) {
            if ($lineTask.Wait(1000)) {
                $line = $lineTask.GetAwaiter().GetResult()
                if ($null -eq $line) { $eof = $true }
                else {
                    [IO.File]::AppendAllText($stdoutPath, $line + "`n", $utf8)
                    $sharedEvent = $line | ConvertFrom-Json -AsHashtable
                    if ($sharedEvent.event -ne 'progress') { $events.Add($sharedEvent) }
                    if ($sharedEvent.event -eq 'two_clients_ready') {
                        $connections = @(Get-NetTCPConnection -OwningProcess $server.Id -LocalPort $port -State Established -ErrorAction Stop)
                        if ($connections.Count -ne 2 -or @($connections | Where-Object { $_.RemoteAddress -ne '127.0.0.1' }).Count -ne 0) {
                            throw 'Vendor socket table does not prove two concurrent loopback clients.'
                        }
                        $record.shared_established_clients = @($connections | Select-Object LocalAddress, LocalPort, RemoteAddress, RemotePort, OwningProcess)
                        Write-Phase 'Two VncAuth clients have actual complete RGB desktops and two vendor TCP connections'
                    }
                    $lineTask = $process.StandardOutput.ReadLineAsync()
                }
            }
            if ([DateTime]::UtcNow -ge $deadline) { $timedOut = $true; throw 'Shared stability probe exceeded its absolute deadline.' }
            if ($snapshotClock.Elapsed.TotalSeconds -ge 30) {
                Assert-LoadAlive
                $server.Refresh()
                if ($server.HasExited) { throw 'Owned vendor exited during shared acceptance.' }
                [void](Assert-OwnedListeners)
                $record.shared_supervision = @{
                    utc = [DateTimeOffset]::UtcNow.ToString('o')
                    vendor_peak_working_set_bytes = $server.PeakWorkingSet64
                    vendor_cpu_ms_since_start = $server.TotalProcessorTime.TotalMilliseconds
                    target_peak_working_set_bytes = $loadProcess.PeakWorkingSet64
                }
                Write-Evidence
                Write-Phase "Shared stability still running; non-progress evidence events=$($events.Count)"
                $snapshotClock.Restart()
            }
        }
        if (-not $process.WaitForExit(5000)) { throw 'Shared probe closed stdout without exiting within5s.' }
        [IO.File]::WriteAllText($stderrPath, $stderr.GetAwaiter().GetResult(), $utf8)
        $readyEvents = @($events | Where-Object event -eq 'two_clients_ready')
        $completed = @($events | Where-Object event -eq 'soak_completed')
        $result = @($events | Where-Object event -eq 'result')
        $clicks = @($events | Where-Object event -eq 'shared_click')
        $idles = @($events | Where-Object event -eq 'idle_completed')
        if ($process.ExitCode -ne 0 -or $readyEvents.Count -ne 1 -or $completed.Count -ne 1 -or $result.Count -ne 1) {
            throw 'Shared probe did not finish exactly one complete acceptance result; original logs retained.'
        }
        $final = $result[0]; $soak = $completed[0]
        $longTerm = $DurationSeconds -ge 7200 -and $IdleSeconds -ge 120
        if (-not $final.passed -or -not $final.close_requested_and_records_removed -or
            $final.cycles -lt 1 -or $final.idle_windows -ne $final.cycles -or $idles.Count -ne $final.cycles -or
            $clicks.Count -ne 2 * $final.cycles -or $soak.actual_soak_ms -lt 1000L * $DurationSeconds -or
            $soak.requested_seconds -ne $DurationSeconds -or $soak.idle_seconds -ne $IdleSeconds -or
            $soak.long_term_acceptance -ne $longTerm -or $readyEvents[0].security -ne 'VncAuth' -or
            @($final.disconnects).Count -ne 2 -or @($final.disconnects | Where-Object { $_ -ne 0 }).Count -ne 0 -or
            @($final.max_pending).Count -ne 2 -or @($final.max_pending | Where-Object { $_ -gt 1 }).Count -ne 0 -or
            @($idles | Where-Object { -not $_.no_synthetic_input -or $_.idle_ms -lt 1000L * $IdleSeconds }).Count -ne 0) {
            throw 'Shared duration, idle, concurrency, policy or cleanup assertions failed.'
        }
        for ($index = 0; $index -lt $clicks.Count; $index++) {
            if ($clicks[$index].actor -ne ($index % 2) -or $clicks[$index].sequence -ne ($index + 1) -or
                $clicks[$index].consumed_clicks -ne ($index + 1) -or -not $clicks[$index].both_fresh_rgb -or
                -not $clicks[$index].full_coverage_and_static_canaries) { throw 'A shared input was not consumed and freshly painted by both clients in order.' }
        }
        Assert-LoadAlive
        $target = Read-LoadState 'state.json'
        if ($null -eq $target -or $target.pid -ne $loadProcess.Id -or $target.closed -or
            $target.shared_probe.sequence -ne $clicks.Count -or $target.shared_probe.clicks -ne $clicks.Count) {
            throw 'Final actual target state does not match every consumed input.'
        }
        [IO.File]::WriteAllText((Join-Path $resultPath 'shared.target.final.json'), ($target | ConvertTo-Json -Depth 8) + "`n", $utf8)
        $disconnectDeadline = [DateTime]::UtcNow.AddSeconds(5)
        do {
            $remaining = @(Get-NetTCPConnection -OwningProcess $server.Id -LocalPort $port -State Established -ErrorAction SilentlyContinue)
            if ($remaining.Count -eq 0) { break }
            Start-Sleep -Milliseconds 100
        } while ([DateTime]::UtcNow -lt $disconnectDeadline)
        if ($remaining.Count -ne 0) { throw 'Vendor still has established clients after probe cleanup.' }
        $record.shared_soak = @{ passed = $true; exit = $process.ExitCode; timed_out = $timedOut
            completed = $soak; result = $final; consumed_clicks = $clicks.Count
            vendor_established_sockets_after_close = $remaining.Count }
        $record.long_term_acceptance = $longTerm
        Write-Evidence
    } finally {
        if ($started -and -not $process.HasExited) { $process.Kill($true); [void]$process.WaitForExit(5000) }
        if ($null -ne $stderr -and $stderr.IsCompleted) {
            [IO.File]::WriteAllText($stderrPath, $stderr.GetAwaiter().GetResult(), $utf8)
        }
        $process.Dispose()
    }
}
function Invoke-Probe([int]$Index, [string]$Password, [bool]$WrongPassword) {
    Assert-GuestNetwork
    $networkBefore = Get-GuestSnapshot "probe-$Index.network-before.json"
    $resetGeneration = 0L
    if ($SyntheticLoad -and -not $WrongPassword) { $resetGeneration = Reset-SyntheticLoad $Index }
    $info = [Diagnostics.ProcessStartInfo]::new($script:probePath)
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.StandardOutputEncoding = $utf8
    $info.StandardErrorEncoding = $utf8
    $steady = if ($SyntheticLoad -and -not $WrongPassword) { 3 } else { 1 }
    foreach ($argument in @('127.0.0.1', [string]$ProbePort, '15', "--steady-seconds=$steady")) { $info.ArgumentList.Add($argument) }
    if ($Quality -eq 'wan') { $info.ArgumentList.Add('--wan') }
    if ($SyntheticLoad -and -not $WrongPassword) {
        $info.ArgumentList.Add("--workload=$Workload")
        $info.ArgumentList.Add('--synthetic-canary')
    }
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
        $timedOut = -not $process.WaitForExit(25000)
        if ($timedOut) { $process.Kill($true); $process.WaitForExit() }
        $out = $stdout.GetAwaiter().GetResult()
        $err = $stderr.GetAwaiter().GetResult()
        [IO.File]::WriteAllText((Join-Path $resultPath "probe-$Index.stdout.txt"), $out, $utf8)
        [IO.File]::WriteAllText((Join-Path $resultPath "probe-$Index.stderr.txt"), $err, $utf8)
        $sample = [ordered]@{ index = $Index; wrong_password = $WrongPassword; exit = $process.ExitCode; timed_out = $timedOut; workload = $(if ($WrongPassword) { 'static' } else { $Workload }); quality = $Quality }
        if ($ProbePort -eq 15912) {
            $networkAfter = Get-GuestSnapshot "probe-$Index.network-after.json"
            $sample.guest_network = @{ before=$networkBefore; after=$networkAfter
                observed_drop_delta=([long]$networkAfter.stats.drops - [long]$networkBefore.stats.drops)
                tcp_retrans_seg_delta=([long]$networkAfter.stats.tcp.RetransSegs - [long]$networkBefore.stats.tcp.RetransSegs)
                loss_condition=$record.network.mode
                loss_observed=([long]$networkAfter.stats.drops -gt [long]$networkBefore.stats.drops)
                note='0.1% configuration is not an observation of loss unless drop counters increased; guest-egress TCP-terminating path only.' }
        }
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
            if ($SyntheticLoad) {
                $sample.synthetic_canary = $out -match '(?m)^synthetic_canary=true\r?$'
                $sample.reset_generation = $resetGeneration
                $consumed = $false
                $deadline = [DateTime]::UtcNow.AddSeconds(2)
                do {
                    Assert-LoadAlive
                    $loadState = Read-LoadState 'state.json'
                    $consumed = Test-ConsumedInput $loadState $resetGeneration
                    if ($consumed) { break }
                    Start-Sleep -Milliseconds 50
                } while ([DateTime]::UtcNow -lt $deadline)
                $sample.load_state = $loadState
                $sample.tk_consumption_passed = $consumed
                [IO.File]::WriteAllText((Join-Path $resultPath "load-$Index.final.json"), ($loadState | ConvertTo-Json -Depth 8) + "`n", $utf8)
                $sample.passed = $sample.passed -and $sample.synthetic_canary -and $consumed
            }
            if ($sample.passed) {
                $sample.connected_ms = [long]$connected.Groups[1].Value
                $sample.first_paint_ms = [long]$first.Groups[1].Value
                $sample.complete_paint_ms = [long]$paint.Groups[1].Value
                $sample.framebuffer = @([int]$first.Groups[2].Value, [int]$first.Groups[3].Value)
                $sample.security = 'VncAuth'; $sample.full_pixel_coverage = $true
            }
        }
        if (-not $WrongPassword) {
            $steadyEvidence = [regex]::Match($out, '(?m)^sample_ms=\d+ steady_ms=(\d+)\r?$')
            $sample.steady_and_connected = $steadyEvidence.Success -and [long]$steadyEvidence.Groups[1].Value -ge (1000L * $steady) -and
                $out -match '(?m)^final_connected=true disconnects=0 connection_error=false steady_complete=true\r?$' -and
                $err -notmatch '(?im)^error:'
            $sample.passed = $sample.passed -and $sample.steady_and_connected
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
    Assert-GuestNetwork
    Write-Phase 'Validate runner, probe and unused fixture port'
    if ($ServiceMode) {
        $existing = @(Get-CimInstance Win32_Service -ErrorAction Stop | Where-Object {
            $_.Name -eq 'uvnc_service' -or $_.Name -eq $serviceName -or $_.Name -match '(?i)ultravnc|uvnc' -or $_.PathName -match '(?i)winvnc\.exe'
        })
        if ($existing.Count -ne 0 -or @(Get-CimInstance Win32_Process -Filter "Name='winvnc.exe'" -ErrorAction Stop).Count -ne 0) { throw 'Fresh service calibration refuses every existing UltraVNC registration or winvnc process.' }
        Add-Type @'
using System.Runtime.InteropServices;
public static class AditVncFixtureConsole {
    [DllImport("kernel32.dll")] public static extern uint WTSGetActiveConsoleSessionId();
}
'@
        $script:serviceConsoleSession = [AditVncFixtureConsole]::WTSGetActiveConsoleSessionId()
        if ($serviceConsoleSession -eq [uint32]::MaxValue -or $serviceConsoleSession -eq 0 -or
            (Get-Process -Id $PID).SessionId -ne $serviceConsoleSession) { throw 'Hosted runner has no matching nonzero active console session for the owned Tk target.' }
        $record.console_session = $serviceConsoleSession
    }
    $script:probePath = (Resolve-Path -LiteralPath $Probe).Path
    $record.probe_sha256 = (Get-FileHash -LiteralPath $probePath -Algorithm SHA256).Hash.ToLowerInvariant()
    $record.os = Get-CimInstance Win32_OperatingSystem | Select-Object Caption, Version, BuildNumber, OSArchitecture
    if ($SyntheticLoad) {
        Write-Phase 'Start owned public synthetic Tk target; its fresh state directory is created only by Python'
        $pythonPath = (Get-Command python.exe -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
        $loadScript = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot 'windows-load.py')).Path
        $record.load_script_sha256 = (Get-FileHash -LiteralPath $loadScript -Algorithm SHA256).Hash.ToLowerInvariant()
        $loadProcess = Start-Process -FilePath $pythonPath -ArgumentList @('-u', ('"' + $loadScript + '"'), '--state-dir', ('"' + $loadStateDir + '"')) `
            -RedirectStandardOutput (Join-Path $resultPath 'target.stdout.txt') -RedirectStandardError (Join-Path $resultPath 'target.stderr.txt') -WindowStyle Hidden -PassThru
        $record.load_pid = $loadProcess.Id
        $deadline = [DateTime]::UtcNow.AddSeconds(20)
        $loadReady = $false
        while ([DateTime]::UtcNow -lt $deadline) {
            Assert-LoadAlive
            $metadata = Read-LoadState 'ready.json'
            if ($null -ne $metadata) {
                $record.load_ready = $metadata
                [IO.File]::WriteAllText((Join-Path $resultPath 'target.ready.json'), ($metadata | ConvertTo-Json -Depth 8) + "`n", $utf8)
                if (-not $metadata.ready -or $metadata.closed -or -not $metadata.focused -or
                    $metadata.geometry.width -ne 800 -or $metadata.geometry.height -ne 600 -or
                    $metadata.geometry.x -ne 0 -or $metadata.geometry.y -ne 0 -or $metadata.pid -ne $loadProcess.Id) {
                    throw 'Synthetic target ready metadata does not prove focused800x600 at0,0 with ownedPID.'
                }
                $loadReady = $true
                break
            }
            Start-Sleep -Milliseconds 100
        }
        if (-not $loadReady) { throw 'Synthetic target did not become ready within20s.' }
        if ($ServiceMode -and $loadProcess.SessionId -ne $serviceConsoleSession) { throw 'Owned Tk process is not in the checked active console session.' }
    }
    $existingTcp = @(Get-NetTCPConnection -LocalPort $port -ErrorAction SilentlyContinue)
    $record.initial_closed_tcp = @($existingTcp | Where-Object { $_.State -eq 'TimeWait' -and $_.OwningProcess -eq 0 } |
        Select-Object LocalAddress, LocalPort, RemoteAddress, RemotePort, State, OwningProcess)
    # Sequential fresh fixtures can inherit closed PID0 TCP TIME_WAIT from an
    # earlier wrong-password rejection; that kernel state is not a live server.
    if (@(Get-Process winvnc -ErrorAction SilentlyContinue).Count -ne 0 -or
        @(Get-Service -Name '*uvnc*' -ErrorAction SilentlyContinue).Count -ne 0 -or
        @($existingTcp | Where-Object { $_.State -ne 'TimeWait' -or $_.OwningProcess -ne 0 }).Count -ne 0 -or
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
    $inputsEnabled = if ($SyntheticLoad) { 1 } else { 0 }
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
InputsEnabled=$inputsEnabled
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
    if ($ServiceMode) {
        $script:serviceIni = Join-Path $serverFolder ($serviceName + '.ini')
        Copy-Item -LiteralPath $ini -Destination $serviceIni
        [IO.File]::WriteAllText((Join-Path $serverFolder 'ultravnc.portable'), '', $utf8)
        $record.service_configuration = @{ bootstrap=$ini; service_ini=$serviceIni; bootstrap_sha256=(Get-FileHash -LiteralPath $ini -Algorithm SHA256).Hash.ToLowerInvariant(); service_ini_sha256=(Get-FileHash -LiteralPath $serviceIni -Algorithm SHA256).Hash.ToLowerInvariant(); marker='ultravnc.portable' }
    }
    # The official binary unconditionally starts UDP echo and ignores LoopbackOnly
    # for that socket. Contain only this new CI executable/UDP port before launch.
    if (@(Get-NetFirewallRule -PolicyStore PersistentStore -ErrorAction Stop | Where-Object { $_.Name -eq $firewallRuleName }).Count -ne 0) {
        throw 'Generated firewall name unexpectedly exists; refusing to modify it.'
    }
    Write-Phase 'Create and verify one owned exact-program UDP15911 inbound block; existing profiles/rules remain unchanged'
    $firewallRuleOwned = $true
    [void](New-NetFirewallRule -PolicyStore PersistentStore -Name $firewallRuleName -DisplayName $firewallRuleName `
        -Description 'Disposable Adit hosted-CI UltraVNC echo containment; removed after owned process exits.' `
        -Program $serverPath -Protocol UDP -LocalPort $port -Direction Inbound -Action Block -Profile Any -Enabled True -ErrorAction Stop)
    Assert-UdpFirewallBlock
    if ($ServiceMode) {
        Write-Phase 'Register and auto-start one fresh exact-name service; never fall back to portable mode'
        $record.server.mode = 'unique SCM service with independently attributed LocalSystem RFB worker'
        $script:serviceLaunchUtc = [DateTime]::UtcNow
        $script:serviceInstallAttempted = $true
        $installArguments = @('-config', ('"' + $ini + '"'), '-install', $serviceName)
        $record.service_install_arguments = $installArguments
        $installer = Start-Process -FilePath $serverPath -ArgumentList $installArguments -WorkingDirectory $serverFolder -WindowStyle Hidden -PassThru `
            -RedirectStandardOutput (Join-Path $resultPath 'service-install.stdout.txt') -RedirectStandardError (Join-Path $resultPath 'service-install.stderr.txt')
        try {
            $installDeadline = [DateTime]::UtcNow.AddSeconds(15)
            do {
                $installExited = $installer.WaitForExit(100)
                # The CLI auto-starts the service before it returns. Capture the
                # root while it is alive, even if startup later leaves an orphan.
                try { Capture-ServiceProcesses }
                catch { $record.service_install_capture_error = $_.Exception.Message }
            } while (-not $installExited -and [DateTime]::UtcNow -lt $installDeadline)
            if (-not $installExited) {
                if (-not [string]::Equals($installer.Path, $serverPath, [StringComparison]::OrdinalIgnoreCase)) { throw 'Install process ownership changed.' }
                Stop-Process -InputObject $installer -Force
                [void]$installer.WaitForExit(5000)
                throw 'Service install CLI did not finish within15s; no helper or interactive fallback permitted.'
            }
            $record.service_install_exit = $installer.ExitCode
            if ($installer.ExitCode -ne 0) { throw 'Service install CLI returned failure.' }
            if ($record.Contains('service_install_capture_error')) { throw 'Service identity capture during automatic startup failed; original error retained.' }
        } finally { $installer.Dispose() }
    } else {
        Write-Phase 'Start portable user-mode server with explicit private INI; no installer or service action'
        $server = Start-Process -FilePath $serverPath -ArgumentList @('-config', ('"' + $ini + '"'), '-run') -WorkingDirectory $serverFolder -WindowStyle Hidden -PassThru
        $record.server.pid = $server.Id
    }
    $ready = $false
    $deadline = [DateTime]::UtcNow.AddSeconds(25)
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($null -ne $server) {
            $server.Refresh()
            if ($server.HasExited) { throw "Vendor process exited before listener readiness: $($server.ExitCode)" }
        }
        if (Assert-OwnedListeners) { $ready = $true; break }
        Start-Sleep -Milliseconds 250
    }
    if (-not $ready) { throw 'No owned loopback RFB listener within25s; startup is not acceptance.' }
    if ($SharedSoak) {
        Write-Phase "Shared stability experiment for${DurationSeconds}s with${IdleSeconds}s input-idle windows"
        Invoke-SharedSoak
    } else {
        for ($sample = 1; $sample -le $Samples; $sample++) {
            Write-Phase "Full framebuffer $Quality/$Workload sample $sample of$Samples; synthetic target=$SyntheticLoad"
            $ownedReady = Assert-OwnedListeners
            if ($ServiceMode -and -not $ownedReady) { throw 'SCM service/worker/listener chain disappeared before a measured sample.' }
            Invoke-Probe $sample 'adit-ci' $false
        }
        Write-Phase 'Wrong password must explicitly fail authentication, not merely time out'
        Invoke-Probe ($Samples + 1) 'wrong-ci' $true
    }
    $ownedReady = Assert-OwnedListeners
    if ($ServiceMode -and -not $ownedReady) { throw 'SCM service/worker/listener chain disappeared before the final acceptance check.' }
    if ($ServiceMode) { $record.service_acceptance = $true }
    $record.outcome = 'passed'
} catch {
    $failed = $true
    $record.error = $_.Exception.Message
    Write-Phase ('FAILED: ' + $_.Exception.Message)
} finally {
    if ($ServiceMode -and $serviceInstallAttempted) {
        try { Stop-OwnedService }
        catch { $failed = $true; $record.outcome = 'failed'; $record.cleanup_error = $_.Exception.Message }
        finally { if ($null -ne $server) { $server.Dispose(); $server = $null } }
    }
    if (-not $ServiceMode -and $null -ne $server) {
        try {
            $server.Refresh()
            if (-not $server.HasExited) {
                if ($server.Path -ne $serverPath) { throw 'Owned process identity changed; cleanup refused.' }
                Stop-Process -InputObject $server -Force
                if (-not $server.WaitForExit(5000)) { throw 'Owned vendor process did not exit within5s.' }
            }
            $record.server_cleanup = 'owned vendor process exited'
        } catch { $failed = $true; $record.outcome = 'failed'; $record.cleanup_error = $_.Exception.Message }
        $server.Dispose()
    }
    if ($null -ne $loadProcess) {
        try {
            if (Test-Path -LiteralPath (Join-Path $loadStateDir 'state.json')) {
                Copy-Item -LiteralPath (Join-Path $loadStateDir 'state.json') -Destination (Join-Path $resultPath 'target.final.json')
            }
            $loadProcess.Refresh()
            if (-not $loadProcess.HasExited) {
                $owned = Get-CimInstance Win32_Process -Filter "ProcessId=$($loadProcess.Id)" -ErrorAction Stop
                if (-not [string]::Equals($loadProcess.Path, $pythonPath, [StringComparison]::OrdinalIgnoreCase) -or
                    $null -eq $owned -or [string]::IsNullOrEmpty($owned.CommandLine) -or
                    $owned.CommandLine.IndexOf($loadScript, [StringComparison]::OrdinalIgnoreCase) -lt 0 -or
                    $owned.CommandLine.IndexOf($loadStateDir, [StringComparison]::OrdinalIgnoreCase) -lt 0) { throw 'Synthetic target process ownership not proven; cleanup refused.' }
                Stop-Process -InputObject $loadProcess -Force
                if (-not $loadProcess.WaitForExit(5000)) { throw 'Owned synthetic target did not exit within5s.' }
            }
            $record.load_cleanup = 'owned Python target exited; identity/path and script/state arguments verified before termination'
        } catch { $failed = $true; $record.outcome = 'failed'; $record.load_cleanup_error = $_.Exception.Message }
        $loadProcess.Dispose()
    }
    if ($firewallRuleOwned) {
        try {
            if ($record.Contains('cleanup_error')) { throw 'Vendor shutdown unproven; retaining the owned protective block until runner disposal.' }
            Remove-NetFirewallRule -PolicyStore PersistentStore -Name $firewallRuleName -ErrorAction Stop
            $remainingPersistent = @(Get-NetFirewallRule -PolicyStore PersistentStore -ErrorAction Stop | Where-Object { $_.Name -eq $firewallRuleName })
            $remainingActive = @(Get-NetFirewallRule -PolicyStore ActiveStore -ErrorAction Stop | Where-Object { $_.Name -eq $firewallRuleName })
            if ($remainingPersistent.Count -ne 0 -or $remainingActive.Count -ne 0) { throw 'Owned firewall rule remains after exact-name cleanup.' }
            $record.firewall_cleanup = 'exact owned GUID rule removed after owned server exit; PersistentStore and ActiveStore absence verified'
        } catch { $failed = $true; $record.outcome = 'failed'; $record.firewall_cleanup_error = $_.Exception.Message }
    }
    if ($ServiceMode -and $failed) { $record.service_acceptance = $false }
    # All temporary vendor files belong to this ephemeral runner; no user path is
    # deleted; ServiceMode removes only its fresh exact GUID service and our rule.
    Write-Evidence
}
if ($failed) { throw 'Windows VNC smoke failed; inspect retained result and raw logs.' }
