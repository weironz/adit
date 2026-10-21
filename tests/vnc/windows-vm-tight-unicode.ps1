param([Parameter(Mandatory)][string]$Directory,
    [ValidatePattern('^[a-z0-9-]{1,64}$')][string]$ResultName='tight-unicode',
    [switch]$BmpCompatibility)
$ErrorActionPreference='Stop'
$root=[IO.Path]::GetFullPath($Directory)
if($PSVersionTable.PSEdition -ne 'Desktop' -or [Diagnostics.Process]::GetCurrentProcess().SessionId -eq 0 -or -not [Environment]::Is64BitProcess -or
    -not (Test-Path -LiteralPath (Join-Path $root 'owner.json')) -or
    (Split-Path $root -Leaf) -notmatch '^AditVncKeys-[a-f0-9]{32}$'){
    throw 'Requires an owned interactive disposable VM directory and 64-bit Windows PowerShell 5.1'
}
$payload=Join-Path $root 'payload'
$result=Join-Path $payload $ResultName
if(Test-Path -LiteralPath $result){throw 'Use a fresh evidence directory; failed runs are retained'}
[void][IO.Directory]::CreateDirectory($result)
$utf8=New-Object Text.UTF8Encoding($false)
$token=[guid]::NewGuid().ToString('N')
$serverPath=Join-Path $payload 'tvnserver.exe'
$driverPath=Join-Path $payload 'vnc_fixture_driver.exe'
$record=[ordered]@{outcome='failed';scope='TightVNC application mode to native Windows TextBox via production TEXT; no IME acceptance';clipboard_send_used=$false;registry_owner_token=$token;started_utc=[DateTime]::UtcNow.ToString('o');cases=@();cleanup_verified=$false}
$record.mode=if($BmpCompatibility){'bmp_compatibility'}else{'full_unicode'}
$record.pass_interpretation=if($BmpCompatibility){'BMP input and whole-commit rejection are correct; no complete server Unicode support claim'}else{'All four native TextBox inputs match their complete Unicode scalars'}
if($BmpCompatibility){$record.scope+='; compatibility rejection only, not complete server Unicode support'}
$server=$null;$target=$null;$driver=$null;$registryOwned=$false;$parentExisted=$true
$cu=$null;$lm=$null;$key=$null;$baseline=$null
$controllerIdentity=$null
$owned=New-Object 'System.Collections.Generic.List[object]'

Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class AditTightNative {
 public static void AtomicReplace(string source,string destination){System.IO.File.Replace(source,destination,null);}
 [StructLayout(LayoutKind.Sequential)] public struct RECT {public int l,t,r,b;}
 [StructLayout(LayoutKind.Sequential)] public struct GUI {public uint cbSize,flags;public IntPtr active,focus,capture,menu,movesize,caret;public RECT rc;}
 [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
 [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr w,out uint pid);
 [DllImport("user32.dll")] public static extern bool GetGUIThreadInfo(uint thread,ref GUI info);
 [DllImport("advapi32.dll",CharSet=CharSet.Unicode)] public static extern int RegCreateKeyEx(Microsoft.Win32.SafeHandles.SafeRegistryHandle h,string sub,int reserved,string cls,int options,int access,IntPtr sa,out IntPtr key,out int disposition);
 [DllImport("advapi32.dll")] public static extern int RegCloseKey(IntPtr key);
 [DllImport("advapi32.dll",CharSet=CharSet.Unicode)] public static extern int RegQueryInfoKey(Microsoft.Win32.SafeHandles.SafeRegistryHandle h,IntPtr cls,IntPtr clsLen,IntPtr reserved,out uint sub,out uint maxSub,out uint maxClass,out uint values,out uint maxName,out uint maxData,out uint security,out long lastWrite);
}
'@
function Save-Json($Path,$Value){
    $absolute=[IO.Path]::GetFullPath($Path)
    if(-not $absolute.StartsWith($result+[IO.Path]::DirectorySeparatorChar,[StringComparison]::OrdinalIgnoreCase)){throw 'Evidence path escaped owned result directory'}
    $temporary=$absolute+'.'+[guid]::NewGuid().ToString('N')+'.tmp'
    try{
        [IO.File]::WriteAllText($temporary,($Value|ConvertTo-Json -Depth 14),$utf8)
        for($attempt=0;$attempt -lt 20;$attempt++){
            try{if([IO.File]::Exists($absolute)){[AditTightNative]::AtomicReplace($temporary,$absolute)}else{[IO.File]::Move($temporary,$absolute)};return}
            catch [IO.IOException]{if($attempt -eq 19){throw};Start-Sleep -Milliseconds 50}
        }
    }finally{if([IO.File]::Exists($temporary)){[IO.File]::Delete($temporary)}}
}
function Save-Ownership {
    Save-Json (Join-Path $result 'fixture-ownership.json') ([ordered]@{
        schema=2;registry_owner_token=$token;registry_owned=$registryOwned;parent_existed=$parentExisted
        user_sid=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value
        session_id=[Diagnostics.Process]::GetCurrentProcess().SessionId;started_utc=$record.started_utc
        payload=$payload;result=$result;controller=$controllerIdentity;server=$record.server_identity
        owned_processes=$owned.ToArray()
    })
}
function App-MutexExists {
    try{$m=[Threading.Mutex]::OpenExisting('Local\tvnserverApplication');$m.Dispose();return $true}
    catch [Threading.WaitHandleCannotBeOpenedException]{return $false}
}
function Process-Identity([int]$IdValue){
    $p=Get-Process -Id $IdValue -ErrorAction Stop
    [pscustomobject]@{id=$p.Id;start_ticks=$p.StartTime.ToUniversalTime().Ticks;path=$p.MainModule.FileName;session_id=$p.SessionId}
}
function Track-Process($Process){
    $identity=Process-Identity $Process.Id
    [void]$owned.Add($identity)
    Save-Ownership
    return $identity
}
function Track-ServerChildren {
    if(-not $server){return}
    # The private payload path alone is insufficient ownership: require an
    # actual parent chain from the new root, then capture exact creation time.
    for($round=0;$round -lt 3;$round++){
        $ids=@($owned.ToArray()|Where-Object {$_.path -eq $serverPath}|ForEach-Object {$_.id})
        foreach($candidate in @(Get-CimInstance Win32_Process -Filter "Name='tvnserver.exe'")){
            if($candidate.ParentProcessId -in $ids -and $candidate.ExecutablePath -eq $serverPath -and $candidate.ProcessId -notin @($owned.ToArray().id)){
                try{[void]$owned.Add((Process-Identity ([int]$candidate.ProcessId)));Save-Ownership}catch{if(Get-Process -Id $candidate.ProcessId -ErrorAction SilentlyContinue){throw}}
            }
        }
    }
}
function Stop-Owned($Identity){
    $p=Get-Process -Id $Identity.id -ErrorAction SilentlyContinue
    if(-not $p){return}
    $actual=Process-Identity $Identity.id
    if($actual.start_ticks -ne $Identity.start_ticks -or $actual.path -ne $Identity.path){throw 'PID identity changed; refusing to terminate it'}
    Stop-Process -Id $Identity.id -Force
    if(-not $p.WaitForExit(5000)){throw 'Owned process did not exit'}
}
function Registry-Metadata($Base,$Subkey){
    $k=$Base.OpenSubKey($Subkey,$false)
    if(-not $k){return [ordered]@{exists=$false}}
    try{
        [uint32]$a=0;[uint32]$b=0;[uint32]$c=0;[uint32]$d=0;[uint32]$e=0;[uint32]$f=0;[uint32]$g=0;[long]$stamp=0
        $code=[AditTightNative]::RegQueryInfoKey($k.Handle,[IntPtr]::Zero,[IntPtr]::Zero,[IntPtr]::Zero,[ref]$a,[ref]$b,[ref]$c,[ref]$d,[ref]$e,[ref]$f,[ref]$g,[ref]$stamp)
        if($code -ne 0){throw ('Registry metadata query failed: '+$code)}
        return [ordered]@{exists=$true;last_write=$stamp;subkeys=$a;values=$d;security_bytes=$g;sddl=$k.GetAccessControl().GetSecurityDescriptorSddlForm([Security.AccessControl.AccessControlSections]::All)}
    }finally{$k.Dispose()}
}
function Baseline-State {
    # Existing SYSTEM workers may deny VM_READ to a limited interactive token.
    # CIM metadata suffices for unchanged-state comparison; never open their
    # modules, terminate them, or infer ownership from a shared executable name.
    $processes=@(Get-CimInstance Win32_Process -Filter "Name='tvnserver.exe'"|Where-Object {$_.ExecutablePath -ne $serverPath}|Sort-Object ProcessId|Select-Object ProcessId,CreationDate,ExecutablePath,SessionId,ParentProcessId)
    $ids=@($processes.ProcessId)
    $services=@(Get-CimInstance Win32_Service|Where-Object {$_.Name -eq 'tvnserver' -or $_.ProcessId -in $ids}|Sort-Object Name|Select-Object Name,State,StartMode,ProcessId,StartName)
    $tcp=@(Get-NetTCPConnection -State Listen -ErrorAction Stop|Where-Object {$_.OwningProcess -in $ids}|Sort-Object LocalAddress,LocalPort,OwningProcess|Select-Object LocalAddress,LocalPort,OwningProcess)
    $udp=@(Get-NetUDPEndpoint -ErrorAction Stop|Where-Object {$_.OwningProcess -in $ids}|Sort-Object LocalAddress,LocalPort,OwningProcess|Select-Object LocalAddress,LocalPort,OwningProcess)
    return [ordered]@{processes=$processes;services=$services;tcp=$tcp;udp=$udp;firewall=@(Get-NetFirewallProfile|Sort-Object Name|Select-Object Name,Enabled);hklm=(Registry-Metadata $lm 'Software\TightVNC\Server')}
}
function Native-Focus {
    $w=[AditTightNative]::GetForegroundWindow();[uint32]$foregroundPid=0
    $thread=[AditTightNative]::GetWindowThreadProcessId($w,[ref]$foregroundPid)
    $gui=New-Object AditTightNative+GUI;$gui.cbSize=[Runtime.InteropServices.Marshal]::SizeOf($gui)
    $ok=[AditTightNative]::GetGUIThreadInfo($thread,[ref]$gui)
    return [ordered]@{foreground_pid=$foregroundPid;foreground_hwnd=$w.ToInt64();gui_ok=$ok;focus_hwnd=$gui.focus.ToInt64()}
}
function Read-Target($Path){
    for($i=0;$i -lt 20;$i++){
        try{return ([IO.File]::ReadAllText($Path,$utf8)|ConvertFrom-Json)}catch{Start-Sleep -Milliseconds 50}
    }
    throw 'Actual TextBox snapshot is unavailable'
}
function Read-DriverLine($Driver,$Capture,[int]$TimeoutMs){
    if($Capture.eof){return $null}
    if(-not $Capture.pending){$Capture.pending=$Driver.StandardOutput.ReadLineAsync()}
    if($TimeoutMs -le 0 -or -not $Capture.pending.Wait($TimeoutMs)){throw 'Driver stdout line deadline expired'}
    $line=$Capture.pending.Result;$Capture.pending=$null
    if($null -eq $line){$Capture.eof=$true;return $null}
    [void]$Capture.lines.Add($line)
    return $line
}
function Wait-DriverAck($Driver,$Capture,[string]$Token,[int]$TimeoutMs){
    $deadline=[DateTime]::UtcNow.AddMilliseconds($TimeoutMs)
    while($true){
        $remaining=[int][Math]::Max(0,($deadline-[DateTime]::UtcNow).TotalMilliseconds)
        $line=Read-DriverLine $Driver $Capture $remaining
        if($null -eq $line){throw ('Driver EOF before ACK '+$Token)}
        if($line -ceq ('ack='+$Token)){return}
        if($line.StartsWith('ack=')){throw 'Unexpected driver ACK token'}
    }
}
function Drain-DriverStdout($Driver,$Capture){
    $deadline=[DateTime]::UtcNow.AddSeconds(2)
    while(-not $Capture.eof){
        $remaining=[int][Math]::Max(0,($deadline-[DateTime]::UtcNow).TotalMilliseconds)
        [void](Read-DriverLine $Driver $Capture $remaining)
    }
}
function Counter-Delta($Earlier,$Later){
    return [ordered]@{press=($Later.press-$Earlier.press);release=($Later.release-$Earlier.release);keypress=($Later.keypress-$Earlier.keypress);text_changes=($Later.text_changes-$Earlier.text_changes)}
}
function Scalars([string]$Text){
    $values=New-Object 'System.Collections.Generic.List[string]'
    for($i=0;$i -lt $Text.Length;$i++){
        if([char]::IsHighSurrogate($Text[$i]) -and $i+1 -lt $Text.Length -and [char]::IsLowSurrogate($Text[$i+1])){$v=[char]::ConvertToUtf32($Text,$i);$i++}else{$v=[int]$Text[$i]}
        [void]$values.Add(('U+{0:X4}' -f $v))
    }
    return ,($values.ToArray())
}
try{
    $controllerIdentity=Process-Identity $PID
    $record.os=Get-CimInstance Win32_OperatingSystem|Select-Object Caption,Version,BuildNumber,OSArchitecture
    if(-not (Test-Path -LiteralPath $serverPath) -or -not (Test-Path -LiteralPath $driverPath)){throw 'Copied signed tvnserver.exe and production driver are required in owned payload'}
    $record.server_sha256=(Get-FileHash -LiteralPath $serverPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $record.server_version=[Diagnostics.FileVersionInfo]::GetVersionInfo($serverPath).FileVersion
    $signature=Get-AuthenticodeSignature -LiteralPath $serverPath
    $record.server_signature=@{status=[string]$signature.Status;subject=$signature.SignerCertificate.Subject}
    if($record.server_sha256 -ne '68be8f45403afbd5bc2312b39e40689fac9a0f00986f1e5773ba8845b2b050e8' -or $record.server_version -notmatch '^2[,\.]\s*8[,\.]\s*90[,\.]\s*0$' -or $signature.Status -ne 'Valid'){throw 'TightVNC source binary identity/signature mismatch'}
    $record.driver_sha256=(Get-FileHash -LiteralPath $driverPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $cu=[Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::CurrentUser,[Microsoft.Win32.RegistryView]::Registry64)
    $lm=[Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine,[Microsoft.Win32.RegistryView]::Registry64)
    $existing=$cu.OpenSubKey('Software\TightVNC\Server',$false)
    if($existing){$existing.Dispose();throw 'SKIP: existing HKCU Server config; no values were read or modified'}
    $serviceOnly=$lm.OpenSubKey('Software\TightVNC\Server\ServiceOnly',$false)
    if($serviceOnly){$serviceOnly.Dispose();throw 'SKIP: existing ServiceOnly policy; do not change it'}
    if(App-MutexExists){throw 'SKIP: existing TightVNC app mutex; do not affect it'}
    if(@(Get-NetTCPConnection -LocalPort 15911 -State Listen -ErrorAction SilentlyContinue).Count -or @(Get-NetUDPEndpoint -LocalPort 15911 -ErrorAction SilentlyContinue).Count){throw 'Fixture port already exists'}
    $parent=$cu.OpenSubKey('Software\TightVNC',$false);$parentExisted=($null -ne $parent);if($parent){$parent.Dispose()}
    $baseline=Baseline-State;$record.before=$baseline
    Save-Json (Join-Path $result 'preflight.json') $record
    # Disposition proves we created a missing key even if another process races
    # the initial check. Never overwrite an existing user's Server configuration.
    [IntPtr]$newKey=[IntPtr]::Zero;[int]$disposition=0
    $rc=[AditTightNative]::RegCreateKeyEx($cu.Handle,'Software\TightVNC\Server',0,$null,0,0xF013F,[IntPtr]::Zero,[ref]$newKey,[ref]$disposition)
    if($newKey -ne [IntPtr]::Zero){[void][AditTightNative]::RegCloseKey($newKey)}
    if($rc -ne 0 -or $disposition -ne 1){throw 'Registry key was not newly created; no values written'}
    $key=$cu.OpenSubKey('Software\TightVNC\Server',$true)
    $key.SetValue('AditAcceptanceOwner',$token,[Microsoft.Win32.RegistryValueKind]::String);$registryOwned=$true
    Save-Ownership
    $settings=@{RfbPort=15911;AcceptRfbConnections=1;UseVncAuthentication=1;LoopbackOnly=1;AllowLoopback=1;AcceptHttpConnections=0;EnableFileTransfers=0;UseD3D=0;UseMirrorDriver=0;RemoveWallpaper=0;BlockRemoteInput=0;BlockLocalInput=0;LocalInputPriority=0;AlwaysShared=1;NeverShared=0;DisconnectClients=0;DisconnectAction=0;RunControlInterface=0;LogLevel=0}
    foreach($name in $settings.Keys){$key.SetValue($name,[int]$settings[$name],[Microsoft.Win32.RegistryValueKind]::DWord)}
    # VncPassCrypt's fixed storage key and reversed DES bit order are verified
    # against the official 2.8.90 source. This is public fixture authentication.
    $fixed=[byte[]]@(23,82,107,6,35,78,88,7);$desKey=New-Object byte[] 8
    for($i=0;$i -lt 8;$i++){for($bit=0;$bit -lt 8;$bit++){$desKey[$i]=$desKey[$i] -bor ((($fixed[$i] -shr $bit) -band 1) -shl (7-$bit))}}
    $plain=New-Object byte[] 8;[Text.Encoding]::ASCII.GetBytes('adit-ci').CopyTo($plain,0)
    $des=[Security.Cryptography.DES]::Create()
    try{$des.Key=$desKey;$des.Mode='ECB';$des.Padding='None';$enc=$des.CreateEncryptor();try{$encrypted=$enc.TransformFinalBlock($plain,0,8)}finally{$enc.Dispose()}}
    finally{$des.Dispose();[Array]::Clear($plain,0,8);[Array]::Clear($desKey,0,8)}
    $key.SetValue('Password',$encrypted,[Microsoft.Win32.RegistryValueKind]::Binary)
    if($key.GetValueKind('Password') -ne [Microsoft.Win32.RegistryValueKind]::Binary -or [BitConverter]::ToString($key.GetValue('Password')) -ne [BitConverter]::ToString($encrypted)){throw 'Synthetic password readback failed'}
    $key.Dispose();$key=$null
    $server=Start-Process -FilePath $serverPath -ArgumentList '-run' -WorkingDirectory $payload -WindowStyle Hidden -PassThru
    $record.server_identity=Track-Process $server
    Save-Ownership
    $deadline=[DateTime]::UtcNow.AddSeconds(20)
    do{
        Track-ServerChildren
        $listeners=@(Get-NetTCPConnection -LocalPort 15911 -State Listen -ErrorAction SilentlyContinue)
        if($listeners.Count){break}
        if($server.HasExited -or [DateTime]::UtcNow -ge $deadline){throw 'TightVNC app listener readiness failed'}
        Start-Sleep -Milliseconds 100
    }while($true)
    $record.fixture_tcp=@(Get-NetTCPConnection -State Listen|Where-Object {$_.OwningProcess -in @($owned.ToArray().id)}|Select-Object LocalAddress,LocalPort,OwningProcess)
    $record.fixture_udp=@(Get-NetUDPEndpoint -ErrorAction SilentlyContinue|Where-Object {$_.OwningProcess -in @($owned.ToArray().id)}|Select-Object LocalAddress,LocalPort,OwningProcess)
    if(@($record.fixture_tcp|Where-Object {$_.LocalAddress -ne '127.0.0.1' -or $_.LocalPort -ne 15911 -or $_.OwningProcess -ne $server.Id}).Count -or $record.fixture_tcp.Count -ne 1 -or $record.fixture_udp.Count){throw 'Owned server has unexpected non-loopback/extra listeners'}
    $targetPath=Join-Path $result 'synthetic-tight-text.exe'
    Add-Type -ReferencedAssemblies System.dll,System.Core.dll,System.Windows.Forms.dll,System.Drawing.dll,System.Web.Extensions.dll -OutputAssembly $targetPath -OutputType WindowsApplication -TypeDefinition @'
using System;
using System.IO;
using System.Text;
using System.Collections.Generic;
using System.Web.Script.Serialization;
using System.Windows.Forms;
using System.Runtime.InteropServices;
public static class TightTextTarget {
 [StructLayout(LayoutKind.Sequential)] struct RECT {public int l,t,r,b;}
 [StructLayout(LayoutKind.Sequential)] struct GUI {public uint cbSize,flags;public IntPtr active,focus,capture,menu,movesize,caret;public RECT rc;}
 [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
 [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr w,out uint pid);
 [DllImport("user32.dll")] static extern bool GetGUIThreadInfo(uint thread,ref GUI gui);
 static bool Focused(TextBox box){uint pid;uint thread=GetWindowThreadProcessId(GetForegroundWindow(),out pid);var gui=new GUI();gui.cbSize=(uint)Marshal.SizeOf(gui);return GetGUIThreadInfo(thread,ref gui)&&pid==(uint)System.Diagnostics.Process.GetCurrentProcess().Id&&gui.focus==box.Handle&&box.Focused;}
 static string[] Scalars(string text){var list=new List<string>();for(int i=0;i<text.Length;i++){int v=text[i];if(char.IsHighSurrogate(text[i])&&i+1<text.Length&&char.IsLowSurrogate(text[i+1])){v=char.ConvertToUtf32(text,i);i++;}list.Add("U+"+v.ToString("X4"));}return list.ToArray();}
 [STAThread] public static void Main(string[] args){
  Application.EnableVisualStyles();var path=args[0];
  var form=new Form {Text="ADIT SYNTHETIC TightVNC text",Width=850,Height=260,TopMost=true};
  var box=new TextBox {Multiline=true,Dock=DockStyle.Fill,ImeMode=ImeMode.Disable,ShortcutsEnabled=false};
  form.Controls.Add(box);int down=0,up=0,chars=0,unfocused=0,losses=0,changes=0;bool last=false;
  Action write=()=>{var data=new {pid=System.Diagnostics.Process.GetCurrentProcess().Id,box_hwnd=box.Handle.ToInt64(),text=box.Text,scalars=Scalars(box.Text),focused=Focused(box),press=down,release=up,keypress=chars,unfocused_input=unfocused,focus_losses=losses,text_changes=changes};File.WriteAllText(Path.Combine(path,"actual.json"),new JavaScriptSerializer().Serialize(data),new UTF8Encoding(false));};
  box.KeyDown+=(s,e)=>{down++;if(!Focused(box))unfocused++;};box.KeyUp+=(s,e)=>{up++;if(!Focused(box))unfocused++;};box.KeyPress+=(s,e)=>{chars++;if(!Focused(box))unfocused++;};
  box.TextChanged+=(s,e)=>{changes++;write();};
  var watch=new Timer {Interval=50};watch.Tick+=(s,e)=>{bool now=Focused(box);if(last&&!now)losses++;last=now;write();};
  form.Shown+=(s,e)=>{form.Activate();box.Focus();last=Focused(box);write();watch.Start();File.WriteAllText(Path.Combine(path,"target-ready.txt"),"ready");};
  var deadline=new Timer {Interval=45000};deadline.Tick+=(s,e)=>form.Close();deadline.Start();Application.Run(form);
 }
}
'@
    $cases=@(@('ascii','abc'),@('bmp-cjk',([string][char]0x4e2d+[char]0x6587)),@('nonbmp-emoji',[char]::ConvertFromUtf32(0x1f60a)),@('combining',('e'+[char]0x301)))
    if($BmpCompatibility){$cases+=,@('mixed-nonbmp',('abc'+[char]::ConvertFromUtf32(0x1f60a)+'xyz'))}
    foreach($item in $cases){
        $casePath=Join-Path $result $item[0];[void][IO.Directory]::CreateDirectory($casePath)
        $reject=($BmpCompatibility -and $item[0] -in @('nonbmp-emoji','mixed-nonbmp'))
        $expected=if($reject){''}else{$item[1]}
        $sample=[ordered]@{case=$item[0];mode=$record.mode;outcome='failed';input=$item[1];input_scalars=(Scalars $item[1]);expected=$expected;expected_scalars=(Scalars $expected);rejection_expected=$reject;clipboard_send_used=$false}
        $target=$null;$driver=$null;$targetIdentity=$null;$driverIdentity=$null;$stdout=$null;$stderr=$null;$writer=$null
        try{
            $target=Start-Process -FilePath $targetPath -ArgumentList ('"'+$casePath+'"') -PassThru
            $targetIdentity=Track-Process $target
            $deadline=[DateTime]::UtcNow.AddSeconds(10)
            while(-not (Test-Path -LiteralPath (Join-Path $casePath 'target-ready.txt'))){if($target.HasExited -or [DateTime]::UtcNow -ge $deadline){throw 'TextBox readiness failed'};Start-Sleep -Milliseconds 50}
            $before=Read-Target (Join-Path $casePath 'actual.json');$focus=Native-Focus
            $sample.before=$before;$sample.native_before=$focus
            if($before.pid -ne $target.Id -or $before.text -ne '' -or -not $before.focused -or $focus.foreground_pid -ne $target.Id -or -not $focus.gui_ok -or $focus.focus_hwnd -ne $before.box_hwnd){throw 'Target is not empty and natively focused; no TEXT sent'}
            $info=New-Object Diagnostics.ProcessStartInfo;$info.FileName=$driverPath;$info.Arguments='127.0.0.1 15911 25';$info.UseShellExecute=$false;$info.CreateNoWindow=$true;$info.RedirectStandardInput=$true;$info.RedirectStandardOutput=$true;$info.RedirectStandardError=$true
            $info.StandardOutputEncoding=$utf8;$info.StandardErrorEncoding=$utf8
            if($BmpCompatibility){$info.Arguments+=' --bmp-only'}
            $driver=New-Object Diagnostics.Process;$driver.StartInfo=$info;[void]$driver.Start();$driverIdentity=Track-Process $driver
            $stdout=@{lines=(New-Object 'System.Collections.Generic.List[string]');pending=$null;eof=$false};$stderr=$driver.StandardError.ReadToEndAsync()
            $writer=New-Object IO.StreamWriter($driver.StandardInput.BaseStream,$utf8)
            $ackBefore='before-'+[guid]::NewGuid().ToString('N');$ackAfter='after-'+[guid]::NewGuid().ToString('N')
            $writer.WriteLine('adit-ci');$writer.WriteLine('WAIT 500');$writer.WriteLine('ACK '+$ackBefore);$writer.Flush()
            Wait-DriverAck $driver $stdout $ackBefore 15000
            $sample.ack_before_received=$true
            $connectedBefore=Read-Target (Join-Path $casePath 'actual.json');$connectedBeforeFocus=Native-Focus
            $sample.connected_before=$connectedBefore;$sample.native_connected_before=$connectedBeforeFocus
            $sample.connection_delta=Counter-Delta $before $connectedBefore
            if($connectedBefore.pid -ne $target.Id -or $connectedBefore.text -ne '' -or -not $connectedBefore.focused -or $connectedBeforeFocus.foreground_pid -ne $target.Id -or -not $connectedBeforeFocus.gui_ok -or $connectedBeforeFocus.focus_hwnd -ne $connectedBefore.box_hwnd){throw 'Connected target is not empty and natively focused; no TEXT sent'}
            $writer.WriteLine('TEXT '+$item[1]);$writer.WriteLine('WAIT 1500');$writer.WriteLine('ACK '+$ackAfter);$writer.Flush()
            Wait-DriverAck $driver $stdout $ackAfter 8000
            $connectedAfter=Read-Target (Join-Path $casePath 'actual.json');$connectedAfterFocus=Native-Focus
            $sample.connected_after=$connectedAfter;$sample.native_connected_after=$connectedAfterFocus
            $sample.input_delta=Counter-Delta $connectedBefore $connectedAfter
            $sample.ack_after_received=$true
            $writer.WriteLine('CLOSE');$writer.Flush();$writer.Dispose();$writer=$null
            if(-not $driver.WaitForExit(35000)){throw 'Production driver timeout'}
            Drain-DriverStdout $driver $stdout
            $stdoutText=[string]::Join([Environment]::NewLine,$stdout.lines.ToArray())
            [IO.File]::WriteAllText((Join-Path $casePath 'driver.stdout.txt'),$stdoutText,$utf8);[IO.File]::WriteAllText((Join-Path $casePath 'driver.stderr.txt'),$stderr.Result,$utf8)
            $after=Read-Target (Join-Path $casePath 'actual.json');$focus=Native-Focus
            $sample.after=$after;$sample.native_after=$focus;$sample.driver_exit=$driver.ExitCode
            $sample.close_delta=Counter-Delta $connectedAfter $after
            $delta=$sample.input_delta
            $inputCountsCorrect=if($reject){$delta.press -eq 0 -and $delta.release -eq 0 -and $delta.keypress -eq 0 -and $delta.text_changes -eq 0 -and $connectedAfter.text -ceq ''}else{$delta.press -gt 0 -and $delta.release -gt 0 -and $delta.keypress -gt 0 -and $delta.text_changes -gt 0}
            $noticeCorrect=if($reject){$stdoutText -match '(?m)^text_rejected=unicode_compatibility\r?$' -and $stdoutText -match '(?m)^notice=VNC .+'}else{$stdoutText -notmatch '(?m)^text_rejected='}
            $connectedFocused=($connectedAfter.focused -and $connectedAfterFocus.foreground_pid -eq $target.Id -and $connectedAfterFocus.gui_ok -and $connectedAfterFocus.focus_hwnd -eq $connectedAfter.box_hwnd)
            $sample.checks=[ordered]@{driver_ready=($stdoutText -match 'ready_ms=\d+');driver_exit_zero=($driver.ExitCode -eq 0);acknowledged_input_phase=($sample.ack_before_received -and $sample.ack_after_received);same_target=($connectedAfter.pid -eq $target.Id -and $after.pid -eq $target.Id);target_focused=($connectedFocused -and $after.focused -and $focus.foreground_pid -eq $target.Id -and $focus.gui_ok -and $focus.focus_hwnd -eq $after.box_hwnd);all_input_focused=($after.unfocused_input -eq 0 -and $after.focus_losses -eq 0);input_counts_correct=$inputCountsCorrect;compatibility_notice_correct=$noticeCorrect;exact_scalars=((@($connectedAfter.scalars) -join ',') -ceq (@($sample.expected_scalars) -join ','));final_scalars_unchanged=((@($after.scalars) -join ',') -ceq (@($connectedAfter.scalars) -join ','))}
            if(@($sample.checks.Values|Where-Object {-not $_}).Count -eq 0){$sample.outcome='passed'}
        }catch{$sample.error=$_.Exception.Message}
        finally{
            if($writer){try{$writer.Dispose()}catch{}}
            try{if($driverIdentity){Stop-Owned $driverIdentity};if($targetIdentity){Stop-Owned $targetIdentity}}catch{$sample.cleanup_error=$_.Exception.Message;$sample.outcome='failed'}
            if($stdout){try{Drain-DriverStdout $driver $stdout}catch{$sample.stdout_error=$_.Exception.Message};[IO.File]::WriteAllText((Join-Path $casePath 'driver.stdout.txt'),[string]::Join([Environment]::NewLine,$stdout.lines.ToArray()),$utf8)}
            if($stderr -and $stderr.Wait(2000)){[IO.File]::WriteAllText((Join-Path $casePath 'driver.stderr.txt'),$stderr.Result,$utf8)}
            Save-Json (Join-Path $casePath 'result.json') $sample
            $record.cases+=,$sample
        }
    }
    if($record.cases.Count -eq $cases.Count -and @($record.cases|Where-Object {$_.outcome -ne 'passed'}).Count -eq 0){$record.outcome='passed'}
}catch{$record.error=$_.Exception.Message}
finally{
    $cleanupErrors=New-Object 'System.Collections.Generic.List[string]'
    if($key){$key.Dispose()}
    try{Track-ServerChildren}catch{[void]$cleanupErrors.Add($_.Exception.Message)}
    foreach($identity in @($owned.ToArray()|Sort-Object start_ticks -Descending)){
        try{Stop-Owned $identity}catch{[void]$cleanupErrors.Add($_.Exception.Message)}
    }
    if($registryOwned){
        try{
            if(App-MutexExists){throw 'App mutex still exists; retaining owned config instead of altering a live instance'}
            $k=$cu.OpenSubKey('Software\TightVNC\Server',$false)
            if(-not $k){throw 'Owned registry key disappeared before cleanup'}
            try{$matches=($k.GetValue('AditAcceptanceOwner') -ceq $token)}finally{$k.Dispose()}
            if(-not $matches){throw 'Registry owner marker mismatch; key retained'}
            $cu.DeleteSubKeyTree('Software\TightVNC\Server',$false)
            $k=$cu.OpenSubKey('Software\TightVNC\Server',$false);if($k){$k.Dispose();throw 'Owned registry key remains'}
            if(-not $parentExisted){$p=$cu.OpenSubKey('Software\TightVNC',$false);if($p){try{$empty=($p.SubKeyCount -eq 0 -and $p.ValueCount -eq 0)}finally{$p.Dispose()};if($empty){$cu.DeleteSubKey('Software\TightVNC',$false)}else{throw 'New parent key has additional data; retained'}}}
            $record.owned_registry_removed=$true
        }catch{[void]$cleanupErrors.Add($_.Exception.Message)}
    }
    if($baseline){
        try{
            $record.after=Baseline-State
            $beforeJson=ConvertTo-Json -InputObject $baseline -Depth 10 -Compress
            $afterJson=ConvertTo-Json -InputObject $record.after -Depth 10 -Compress
            $record.original_state_unchanged=($beforeJson -ceq $afterJson)
            if(-not $record.original_state_unchanged){throw 'Original TightVNC service/process/ports/firewall/HKLM metadata changed'}
            $record.remaining_fixture_tcp=@(Get-NetTCPConnection -LocalPort 15911 -State Listen -ErrorAction SilentlyContinue).Count
            $record.remaining_fixture_udp=@(Get-NetUDPEndpoint -LocalPort 15911 -ErrorAction SilentlyContinue).Count
            if($record.remaining_fixture_tcp -or $record.remaining_fixture_udp){throw 'Fixture port remains'}
        }catch{[void]$cleanupErrors.Add($_.Exception.Message)}
    }
    $record.owned_processes=$owned.ToArray()
    $record.cleanup_errors=$cleanupErrors.ToArray()
    $record.cleanup_verified=($registryOwned -and $record.owned_registry_removed -and $record.original_state_unchanged -and $cleanupErrors.Count -eq 0)
    if(-not $record.cleanup_verified){$record.outcome='failed'}
    if($cu){$cu.Dispose()};if($lm){$lm.Dispose()}
    $record.finished_utc=[DateTime]::UtcNow.ToString('o')
    if($registryOwned){Save-Ownership}
    Save-Json (Join-Path $result 'result.json') $record
}
if($record.outcome -ne 'passed'){exit 1}
