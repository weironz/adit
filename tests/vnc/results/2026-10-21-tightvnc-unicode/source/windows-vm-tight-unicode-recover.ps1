param([Parameter(Mandatory)][string]$Directory,
    [ValidatePattern('^[a-z0-9-]{1,64}$')][string]$ResultName='tight-unicode',
    [switch]$StopController,
    [ValidateRange(300,3600)][int]$AfterSeconds=300)
$ErrorActionPreference='Stop'
$root=[IO.Path]::GetFullPath($Directory)
if($PSVersionTable.PSEdition -ne 'Desktop' -or -not [Environment]::Is64BitProcess -or
    -not (Test-Path -LiteralPath (Join-Path $root 'owner.json')) -or
    (Split-Path $root -Leaf) -notmatch '^AditVncKeys-[a-f0-9]{32}$'){
    throw 'Requires the original owned fixture directory and 64-bit Windows PowerShell 5.1'
}
$payload=Join-Path $root 'payload';$result=Join-Path $payload $ResultName
$serverPath=Join-Path $payload 'tvnserver.exe';$driverPath=Join-Path $payload 'vnc_fixture_driver.exe'
$targetPath=Join-Path $result 'synthetic-tight-text.exe'
$allowed=@($serverPath,$driverPath,$targetPath)
$utf8=New-Object Text.UTF8Encoding($false)
$reportPath=Join-Path $result ('recovery-'+[DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ')+'-'+[guid]::NewGuid().ToString('N')+'.json')
$report=[ordered]@{outcome='failed';scope='Exact owned TightVNC fixture recovery; never reconfigure original services or firewall';started_utc=[DateTime]::UtcNow.ToString('o');process_actions=@();unknown_processes=@();registry_removed=$false;original_state_unchanged=$false}
$cu=$null;$lm=$null

Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class AditTightRecoveryNative {
 public static void AtomicReplace(string source,string destination){System.IO.File.Replace(source,destination,null);}
 [DllImport("advapi32.dll",CharSet=CharSet.Unicode)] public static extern int RegQueryInfoKey(Microsoft.Win32.SafeHandles.SafeRegistryHandle h,IntPtr cls,IntPtr clsLen,IntPtr reserved,out uint sub,out uint maxSub,out uint maxClass,out uint values,out uint maxName,out uint maxData,out uint security,out long lastWrite);
}
'@
function Save-Report {
    # One fresh report per invocation, replaced atomically as recovery progresses.
    if(-not [IO.Directory]::Exists($result)){throw 'Original result directory does not exist'}
    $temporary=$reportPath+'.'+[guid]::NewGuid().ToString('N')+'.tmp'
    try{[IO.File]::WriteAllText($temporary,($report|ConvertTo-Json -Depth 15),$utf8);if([IO.File]::Exists($reportPath)){[AditTightRecoveryNative]::AtomicReplace($temporary,$reportPath)}else{[IO.File]::Move($temporary,$reportPath)}}
    finally{if([IO.File]::Exists($temporary)){[IO.File]::Delete($temporary)}}
}
function Process-Identity([int]$IdValue){
    $p=Get-Process -Id $IdValue -ErrorAction SilentlyContinue
    if(-not $p){return $null}
    return [pscustomobject]@{id=$p.Id;start_ticks=$p.StartTime.ToUniversalTime().Ticks;path=$p.MainModule.FileName;session_id=$p.SessionId}
}
function Check-Identity($Identity,[switch]$Controller){
    if(-not $Identity -or [int]$Identity.id -le 0 -or [long]$Identity.start_ticks -le 0){throw 'Missing exact recorded process identity'}
    $path=[IO.Path]::GetFullPath([string]$Identity.path)
    if($Controller){if($path -ne (Join-Path $PSHOME 'powershell.exe')){throw 'Controller is not the recorded Windows PowerShell executable'}}
    elseif($path -notin $allowed){throw 'Recorded process is outside the explicit owned executable allowlist'}
    if([int]$Identity.session_id -ne [Diagnostics.Process]::GetCurrentProcess().SessionId){throw 'Recorded process belongs to another session'}
    $actual=Process-Identity ([int]$Identity.id)
    if(-not $actual){return 'exited'}
    if($actual.start_ticks -ne [long]$Identity.start_ticks){return 'pid_reused'}
    if($actual.path -ne $path -or $actual.session_id -ne [int]$Identity.session_id){throw 'Live process identity does not match manifest'}
    return 'live'
}
function Stop-Exact($Identity,[switch]$Controller){
    $state=Check-Identity $Identity -Controller:$Controller
    if($state -eq 'live'){
        if([int]$Identity.id -eq $PID){throw 'Refusing to stop the recovery process'}
        $p=Get-Process -Id $Identity.id -ErrorAction Stop
        # Recheck immediately before termination; PID reuse never authorizes it.
        if((Check-Identity $Identity -Controller:$Controller) -ne 'live'){throw 'Identity changed before termination'}
        Stop-Process -Id $Identity.id -Force
        if(-not $p.WaitForExit(5000)){throw 'Owned process did not exit'}
    }
    $report.process_actions+=,[ordered]@{identity=$Identity;observed=$state;terminated=($state -eq 'live')}
    Save-Report
}
function Registry-Metadata($Base,$Subkey){
    $k=$Base.OpenSubKey($Subkey,$false)
    if(-not $k){return [ordered]@{exists=$false}}
    try{
        [uint32]$a=0;[uint32]$b=0;[uint32]$c=0;[uint32]$d=0;[uint32]$e=0;[uint32]$f=0;[uint32]$g=0;[long]$stamp=0
        $code=[AditTightRecoveryNative]::RegQueryInfoKey($k.Handle,[IntPtr]::Zero,[IntPtr]::Zero,[IntPtr]::Zero,[ref]$a,[ref]$b,[ref]$c,[ref]$d,[ref]$e,[ref]$f,[ref]$g,[ref]$stamp)
        if($code -ne 0){throw ('Registry metadata query failed: '+$code)}
        return [ordered]@{exists=$true;last_write=$stamp;subkeys=$a;values=$d;security_bytes=$g;sddl=$k.GetAccessControl().GetSecurityDescriptorSddlForm([Security.AccessControl.AccessControlSections]::All)}
    }finally{$k.Dispose()}
}
function Baseline-State {
    $processes=@(Get-CimInstance Win32_Process -Filter "Name='tvnserver.exe'"|Where-Object {$_.ExecutablePath -ne $serverPath}|Sort-Object ProcessId|Select-Object ProcessId,CreationDate,ExecutablePath,SessionId,ParentProcessId)
    $ids=@($processes.ProcessId)
    return [ordered]@{
        processes=$processes
        services=@(Get-CimInstance Win32_Service|Where-Object {$_.Name -eq 'tvnserver' -or $_.ProcessId -in $ids}|Sort-Object Name|Select-Object Name,State,StartMode,ProcessId,StartName)
        tcp=@(Get-NetTCPConnection -State Listen|Where-Object {$_.OwningProcess -in $ids}|Sort-Object LocalAddress,LocalPort,OwningProcess|Select-Object LocalAddress,LocalPort,OwningProcess)
        udp=@(Get-NetUDPEndpoint|Where-Object {$_.OwningProcess -in $ids}|Sort-Object LocalAddress,LocalPort,OwningProcess|Select-Object LocalAddress,LocalPort,OwningProcess)
        firewall=@(Get-NetFirewallProfile|Sort-Object Name|Select-Object Name,Enabled)
        hklm=(Registry-Metadata $lm 'Software\TightVNC\Server')
    }
}
function App-MutexExists {
    try{$m=[Threading.Mutex]::OpenExisting('Local\tvnserverApplication');$m.Dispose();return $true}
    catch [Threading.WaitHandleCannotBeOpenedException]{return $false}
}
try{
    # Windows PowerShell 5.1 otherwise treats our UTF-8-no-BOM JSON as ANSI.
    $preflight=[IO.File]::ReadAllText((Join-Path $result 'preflight.json'),$utf8)|ConvertFrom-Json
    $journal=[IO.File]::ReadAllText((Join-Path $result 'fixture-ownership.json'),$utf8)|ConvertFrom-Json
    $currentSid=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $currentSession=[Diagnostics.Process]::GetCurrentProcess().SessionId
    $token=[string]$journal.registry_owner_token
    if($journal.schema -ne 2 -or $token -notmatch '^[a-f0-9]{32}$' -or $token -cne $preflight.registry_owner_token -or
        $journal.user_sid -cne $currentSid -or $journal.session_id -ne $currentSession -or $currentSession -eq 0 -or
        [IO.Path]::GetFullPath([string]$journal.payload) -ne $payload -or [IO.Path]::GetFullPath([string]$journal.result) -ne $result -or
        -not $journal.registry_owned){throw 'Ownership/SID/session/path mismatch; no recovery mutation authorized'}
    $report.registry_owner_token=$token;$report.user_sid=$currentSid;$report.session_id=$currentSession
    $cu=[Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::CurrentUser,[Microsoft.Win32.RegistryView]::Registry64)
    $lm=[Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine,[Microsoft.Win32.RegistryView]::Registry64)
    $k=$cu.OpenSubKey('Software\TightVNC\Server',$false)
    if($k){try{if($k.GetValueKind('AditAcceptanceOwner') -ne [Microsoft.Win32.RegistryValueKind]::String -or $k.GetValue('AditAcceptanceOwner') -cne $token){throw 'Existing HKCU owner marker does not match; no mutation'}}finally{$k.Dispose()}}
    $controllerState=Check-Identity $journal.controller -Controller
    $report.controller_state=$controllerState
    Save-Report
    if($controllerState -eq 'live'){
        $age=([DateTime]::UtcNow-[DateTime]$journal.started_utc).TotalSeconds
        if(-not $StopController -or $age -lt $AfterSeconds){throw 'Controller is still active; preserve its normal finally until explicit bounded timeout'}
        Stop-Exact $journal.controller -Controller
    }
    $identities=New-Object 'System.Collections.Generic.List[object]'
    foreach($identity in @($journal.owned_processes)){
        [void](Check-Identity $identity)
        [void]$identities.Add($identity)
    }
    # Recover a still-live server's fresh control slave only with a verified
    # live parent chain. Orphans without a recorded identity remain unknown.
    for($round=0;$round -lt 3;$round++){
        foreach($parent in @($identities.ToArray()|Where-Object {$_.path -eq $serverPath})){
            if((Check-Identity $parent) -ne 'live'){continue}
            foreach($child in @(Get-CimInstance Win32_Process -Filter "Name='tvnserver.exe'"|Where-Object {$_.ParentProcessId -eq $parent.id -and $_.ExecutablePath -eq $serverPath})){
                if($child.ProcessId -in @($identities.ToArray().id)){continue}
                $identity=Process-Identity ([int]$child.ProcessId)
                if($identity -and $identity.start_ticks -ge $parent.start_ticks -and $identity.session_id -eq $currentSession -and (Check-Identity $parent) -eq 'live'){
                    [void]$identities.Add($identity)
                }
            }
        }
    }
    $report.recorded_identities=$identities.ToArray();Save-Report
    foreach($identity in @($identities.ToArray()|Sort-Object start_ticks -Descending)){Stop-Exact $identity}
    $remaining=@(Get-CimInstance Win32_Process|Where-Object {$_.ExecutablePath -and $_.ExecutablePath.StartsWith($payload+[IO.Path]::DirectorySeparatorChar,[StringComparison]::OrdinalIgnoreCase)}|Select-Object ProcessId,CreationDate,ExecutablePath,ParentProcessId,SessionId)
    $report.unknown_processes=$remaining
    if($remaining.Count){throw 'Unjournaled processes remain in owned payload; report only, never guess or sweep them'}
    if(App-MutexExists){throw 'App mutex remains after exact cleanup; retain HKCU config'}
    $k=$cu.OpenSubKey('Software\TightVNC\Server',$false)
    if($k){
        try{if($k.GetValue('AditAcceptanceOwner') -cne $token){throw 'Owner marker changed during recovery; retain key'}}finally{$k.Dispose()}
        $cu.DeleteSubKeyTree('Software\TightVNC\Server',$false)
    }
    $k=$cu.OpenSubKey('Software\TightVNC\Server',$false)
    if($k){$k.Dispose();throw 'Owned HKCU key remains'}
    $report.registry_removed=$true
    if($journal.parent_existed -eq $false){
        $p=$cu.OpenSubKey('Software\TightVNC',$false)
        if($p){try{$empty=($p.SubKeyCount -eq 0 -and $p.ValueCount -eq 0)}finally{$p.Dispose()};if($empty){$cu.DeleteSubKey('Software\TightVNC',$false)}else{throw 'New HKCU parent has additional data; retain it'}}
    }
    $report.after=Baseline-State
    $report.original_state_unchanged=((ConvertTo-Json -InputObject $preflight.before -Depth 10 -Compress) -ceq (ConvertTo-Json -InputObject $report.after -Depth 10 -Compress))
    if(-not $report.original_state_unchanged){throw 'Original service/process/ports/firewall/HKLM metadata differs; report only'}
    $report.fixture_tcp=@(Get-NetTCPConnection -LocalPort 15911 -State Listen -ErrorAction SilentlyContinue|Select-Object LocalAddress,LocalPort,OwningProcess)
    $report.fixture_udp=@(Get-NetUDPEndpoint -LocalPort 15911 -ErrorAction SilentlyContinue|Select-Object LocalAddress,LocalPort,OwningProcess)
    if($report.fixture_tcp.Count -or $report.fixture_udp.Count){throw 'Fixture listeners remain; report only'}
    $report.outcome='passed'
}catch{$report.error=$_.Exception.Message}
finally{
    if($cu){$cu.Dispose()};if($lm){$lm.Dispose()}
    $report.finished_utc=[DateTime]::UtcNow.ToString('o')
    Save-Report
}
if($report.outcome -ne 'passed'){exit 1}
