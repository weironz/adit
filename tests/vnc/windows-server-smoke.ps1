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
    [ValidateRange(1,28800)][int]$DurationSeconds = 7200,
    [ValidateRange(1,600)][int]$IdleSeconds = 120
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (-not $IsWindows -or $env:GITHUB_ACTIONS -ne 'true' -or
    $env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows' -or
    -not $env:RUNNER_NAME -or -not $env:RUNNER_TEMP -or -not $env:GITHUB_SHA) {
    throw 'Only a disposable GitHub-hosted Windows runner may execute this vendor fixture.'
}
if (-not $SyntheticLoad -and $Workload -ne 'static') { throw 'Input workloads require the owned synthetic target.' }
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
$source = '0cd8b41aeb34e52e170b33e24411b3f805e303be'
$record = [ordered]@{
    outcome = 'failed'; source_sha = $env:GITHUB_SHA; run_id = $env:GITHUB_RUN_ID
    scope = "UltraVNC1.8.3.0 portable Windows hosted CI; $Samples $Quality/$Workload loopback samples plus1 wrong-password rejection; not impaired-network or service acceptance"
    sample_count_requested = $Samples; workload = $Workload; quality = $Quality; synthetic_load = [bool]$SyntheticLoad
    network = 'native loopback baseline; no RTT/bandwidth/loss shaping'
    runner = $env:RUNNER_NAME; os = [Environment]::OSVersion.VersionString
    target = "127.0.0.1:$port"; samples = @(); configuration_reference_commit = $source
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
function Assert-OwnedListeners {
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
    foreach ($argument in @('127.0.0.1', [string]$port, '15', "--steady-seconds=$steady")) { $info.ArgumentList.Add($argument) }
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
    }
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
    Write-Phase 'Start portable user-mode server with explicit private INI; no installer or service action'
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
    if ($SharedSoak) {
        Write-Phase "Shared stability experiment for${DurationSeconds}s with${IdleSeconds}s input-idle windows"
        Invoke-SharedSoak
    } else {
        for ($sample = 1; $sample -le $Samples; $sample++) {
            Write-Phase "Full framebuffer $Quality/$Workload sample $sample of$Samples; synthetic target=$SyntheticLoad"
            [void](Assert-OwnedListeners)
            Invoke-Probe $sample 'adit-ci' $false
        }
        Write-Phase 'Wrong password must explicitly fail authentication, not merely time out'
        Invoke-Probe ($Samples + 1) 'wrong-ci' $true
    }
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
    # All temporary vendor files belong to this ephemeral runner; no user path is
    # deleted, no service/profile is changed, and only our one rule is removed.
    Write-Evidence
}
if ($failed) { throw 'Windows VNC smoke failed; inspect retained result and raw logs.' }
