param([Parameter(Mandatory)][string]$Directory,
    [ValidatePattern('^[a-z0-9-]{1,64}$')][string]$ResultName='unicode-staged',
    [switch]$BmpCompatibility)
$ErrorActionPreference='Stop'
$root=[IO.Path]::GetFullPath($Directory)
if($PSVersionTable.PSEdition -ne 'Desktop' -or [Diagnostics.Process]::GetCurrentProcess().SessionId -eq 0 -or -not [Environment]::Is64BitProcess -or
    -not (Test-Path -LiteralPath (Join-Path $root 'owner.json')) -or
    (Split-Path $root -Leaf) -notmatch '^AditVncKeys-[a-f0-9]{32}$'){
    throw 'Requires an owned console session directory and 64-bit Windows PowerShell 5.1'
}
$payload=Join-Path $root 'payload'
$result=Join-Path $payload $ResultName
if(Test-Path -LiteralPath $result){throw 'Use a fresh evidence directory; failed runs are retained'}
[void][IO.Directory]::CreateDirectory($result)
$utf8=New-Object Text.UTF8Encoding($false)
$record=[ordered]@{outcome='failed';scope='Portable UltraVNC 1.8.3.0 to a native Windows TextBox through the production TEXT entry; no IME acceptance';clipboard_send_used=$false;cases=@();cleanup_verified=$false;mode=$(if($BmpCompatibility){'bmp_compatibility'}else{'full_unicode'})}
$record.pass_interpretation=$(if($BmpCompatibility){'BMP input and whole-commit rejection are correct; no complete server Unicode support claim'}else{'All recorded native TextBox inputs match their complete Unicode scalars'})
$server=$null;$target=$null;$driver=$null;$baseline=$null;$cu=$null;$lm=$null
$owned=New-Object 'System.Collections.Generic.List[object]'

Add-Type @'
using System;
using System.IO;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
public static class AditUvncNative {
 public static void AtomicReplace(string source,string destination){System.IO.File.Replace(source,destination,null);}
 [StructLayout(LayoutKind.Sequential)] public struct RECT {public int l,t,r,b;}
 [StructLayout(LayoutKind.Sequential)] public struct GUI {public uint cbSize,flags;public IntPtr active,focus,capture,menu,movesize,caret;public RECT rc;}
 [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
 [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr w,out uint pid);
 [DllImport("user32.dll")] public static extern bool GetGUIThreadInfo(uint thread,ref GUI info);
 [DllImport("kernel32.dll",EntryPoint="WritePrivateProfileStructW",CharSet=CharSet.Unicode,SetLastError=true)]
 [return: MarshalAs(UnmanagedType.Bool)] public static extern bool WriteStruct(string section,string key,byte[] value,uint size,string file);
 [DllImport("kernel32.dll",EntryPoint="GetPrivateProfileStructW",CharSet=CharSet.Unicode,SetLastError=true)]
 [return: MarshalAs(UnmanagedType.Bool)] public static extern bool ReadStruct(string section,string key,[Out] byte[] value,uint size,string file);
}
'@
function Save-Json($Path,$Value){
    $absolute=[IO.Path]::GetFullPath($Path)
    if(-not $absolute.StartsWith($result+[IO.Path]::DirectorySeparatorChar,[StringComparison]::OrdinalIgnoreCase)){throw 'Evidence path escaped owned result directory'}
    $temporary=$absolute+'.'+[guid]::NewGuid().ToString('N')+'.tmp'
    try{
        [IO.File]::WriteAllText($temporary,($Value|ConvertTo-Json -Depth 14),$utf8)
        for($attempt=0;$attempt -lt 20;$attempt++){
            try{if([IO.File]::Exists($absolute)){[AditUvncNative]::AtomicReplace($temporary,$absolute)}else{[IO.File]::Move($temporary,$absolute)};return}
            catch [IO.IOException]{if($attempt -eq 19){throw};Start-Sleep -Milliseconds 50}
        }
    }finally{if([IO.File]::Exists($temporary)){[IO.File]::Delete($temporary)}}
}
function Process-Identity([int]$IdValue){
    $p=Get-Process -Id $IdValue -ErrorAction Stop
    [pscustomobject]@{id=$p.Id;start_ticks=$p.StartTime.ToUniversalTime().Ticks;path=$p.MainModule.FileName;session_id=$p.SessionId}
}
function Track-Process($Process){
    $identity=Process-Identity $Process.Id
    [void]$owned.Add($identity)
    return $identity
}
function Stop-Owned($Identity){
    $p=Get-Process -Id $Identity.id -ErrorAction SilentlyContinue
    if(-not $p){return}
    $actual=Process-Identity $Identity.id
    if($actual.start_ticks -ne $Identity.start_ticks -or $actual.path -ne $Identity.path){throw 'PID identity changed; refusing to terminate it'}
    Stop-Process -Id $Identity.id -Force
    if(-not $p.WaitForExit(5000)){throw 'Owned process did not exit'}
}
function Save-Ownership {
    Save-Json (Join-Path $result 'fixture-ownership.json') ([ordered]@{
        schema=2;user_sid=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value
        session_id=[Diagnostics.Process]::GetCurrentProcess().SessionId;started_utc=$record.started_utc
        payload=$payload;result=$result;server=$record.server;owned_processes=$owned.ToArray()
    })
}
function Baseline-State {
    $processes=@(Get-CimInstance Win32_Process -Filter "Name='winvnc.exe'" -ErrorAction SilentlyContinue|Sort-Object ProcessId|Select-Object ProcessId,CreationDate,ExecutablePath,SessionId)
    $services=@(Get-CimInstance Win32_Service -ErrorAction SilentlyContinue|Where-Object {$_.Name -match '(?i)uvnc|ultravnc'}|Sort-Object Name|Select-Object Name,State,StartMode,ProcessId)
    $tcp=@(Get-NetTCPConnection -State Listen -ErrorAction SilentlyContinue|Where-Object {$_.LocalPort -eq 15911}|Sort-Object LocalAddress,LocalPort,OwningProcess|Select-Object LocalAddress,LocalPort,OwningProcess)
    $udp=@(Get-NetUDPEndpoint -LocalPort 15911 -ErrorAction SilentlyContinue|Sort-Object LocalAddress,LocalPort,OwningProcess|Select-Object LocalAddress,LocalPort,OwningProcess)
    # Reading firewall profiles needs elevation; a standard user still gets the
    # process/port comparison, so this stays empty rather than aborting the run.
    $firewall=@(Get-NetFirewallProfile -ErrorAction SilentlyContinue|Sort-Object Name|Select-Object Name,Enabled)
    return [ordered]@{processes=$processes;services=$services;tcp=$tcp;udp=$udp;firewall=$firewall}
}
function Native-Focus {
    $w=[AditUvncNative]::GetForegroundWindow();[uint32]$foregroundPid=0
    $thread=[AditUvncNative]::GetWindowThreadProcessId($w,[ref]$foregroundPid)
    $gui=New-Object AditUvncNative+GUI;$gui.cbSize=[Runtime.InteropServices.Marshal]::SizeOf($gui)
    $ok=[AditUvncNative]::GetGUIThreadInfo($thread,[ref]$gui)
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
    $record.started_utc=[DateTime]::UtcNow.ToString('o')
    $record.os=Get-CimInstance Win32_OperatingSystem|Select-Object Caption,Version,BuildNumber,OSArchitecture
    $record.session_id=[Diagnostics.Process]::GetCurrentProcess().SessionId
    $archive=Join-Path $payload 'UltraVNC_1830.zip'
    if(-not (Test-Path -LiteralPath $archive)){throw 'Pinned vendor archive is required in the owned payload'}
    $record.archive_sha256=(Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    if($record.archive_sha256 -ne '11163a0b9b86321bf6403eb0cf84b81f7230ce32e6291377e1fe5beb987f7e4f'){throw 'Pinned vendor archive checksum mismatch'}
    $driverPath=Join-Path $payload 'vnc_fixture_driver.exe'
    if(-not (Test-Path -LiteralPath $driverPath)){throw 'Production driver is required in the owned payload'}
    $record.driver_sha256=(Get-FileHash -LiteralPath $driverPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $baseline=Baseline-State;$record.before=$baseline
    Save-Json (Join-Path $result 'preflight.json') $record
    Expand-Archive -LiteralPath $archive -DestinationPath (Join-Path $result 'portable')
    $serverPath=Join-Path $result 'portable/x64/winvnc.exe'
    $serverFolder=Split-Path -Parent $serverPath
    $record.server=[ordered]@{
        path_sha256=(Get-FileHash -LiteralPath $serverPath -Algorithm SHA256).Hash.ToLowerInvariant()
        version=[Diagnostics.FileVersionInfo]::GetVersionInfo($serverPath).FileVersion
        mode='portable user application from the pinned official archive; never installed as a service'
        arguments=@('-config','"<owned ini>"','-run')
    }
    $ini=Join-Path $serverFolder 'ultravnc.ini'
    $port=15911
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
InputsEnabled=1
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
path=$result
[poll]
PollFullScreen=1
OnlyPollOnEvent=0
EnableDriver=0
EnableHook=0
EnableVirtual=0
[UltraVNC]
"@ | Set-Content -LiteralPath $ini -Encoding ascii
    # d3des is LSB-first; reverse each fixed-key byte for .NET's DES. Public
    # synthetic fixture password, written through the native struct API only.
    $fixed=[byte[]]@(23,82,107,6,35,78,88,7);$desKey=New-Object byte[] 8
    for($i=0;$i -lt 8;$i++){for($bit=0;$bit -lt 8;$bit++){$desKey[$i]=$desKey[$i] -bor ((($fixed[$i] -shr $bit) -band 1) -shl (7-$bit))}}
    $plain=New-Object byte[] 8;[Text.Encoding]::ASCII.GetBytes('adit-ci').CopyTo($plain,0)
    $des=[Security.Cryptography.DES]::Create()
    try{$des.Key=$desKey;$des.Mode='ECB';$des.Padding='None';$encryptor=$des.CreateEncryptor();try{$encrypted=$encryptor.TransformFinalBlock($plain,0,8)}finally{$encryptor.Dispose()}}
    finally{$des.Dispose();[Array]::Clear($plain,0,8);[Array]::Clear($desKey,0,8)}
    if(-not [AditUvncNative]::WriteStruct('UltraVNC','passwd',$encrypted,8,$ini)){throw 'Native password structure write failed'}
    $readback=New-Object byte[] 8
    if(-not [AditUvncNative]::ReadStruct('UltraVNC','passwd',$readback,8,$ini) -or [BitConverter]::ToString($readback) -ne [BitConverter]::ToString($encrypted)){throw 'Native password structure readback failed'}
    $record.password_format='Public synthetic adit-ci, zero-padded 8; fixed-key VNC DES with reversed key bits; native struct write/read verified; never argv/env'
    $server=Start-Process -FilePath $serverPath -ArgumentList @('-config',('"'+$ini+'"'),'-run') -WorkingDirectory $serverFolder -WindowStyle Hidden -PassThru
    $record.server_identity=Track-Process $server
    Save-Ownership
    $deadline=[DateTime]::UtcNow.AddSeconds(25)
    do{
        $listeners=@(Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue)
        if($listeners.Count){break}
        $server.Refresh()
        if($server.HasExited){throw ('UltraVNC exited before listener readiness, code=' + $server.ExitCode)}
        if([DateTime]::UtcNow -ge $deadline){throw 'UltraVNC listener readiness failed'}
        Start-Sleep -Milliseconds 100
    }while($true)
    $record.fixture_tcp=@($listeners|Select-Object LocalAddress,LocalPort,OwningProcess)
    $record.fixture_udp=@(Get-NetUDPEndpoint -LocalPort $port -ErrorAction SilentlyContinue|Select-Object LocalAddress,LocalPort,OwningProcess)
    if(@($listeners|Where-Object {$_.LocalAddress -ne '127.0.0.1' -or $_.OwningProcess -ne $server.Id}).Count){throw 'Owned server listener is not loopback TCP owned by the fixture'}
    $targetPath=Join-Path $result 'synthetic-uvnc-text.exe'
    Add-Type -ReferencedAssemblies System.dll,System.Core.dll,System.Windows.Forms,System.Drawing.dll,System.Web.Extensions.dll -OutputAssembly $targetPath -OutputType WindowsApplication -TypeDefinition @'
using System;
using System.IO;
using System.Text;
using System.Collections.Generic;
using System.Web.Script.Serialization;
using System.Windows.Forms;
using System.Runtime.InteropServices;
public static class UvncTextTarget {
 [StructLayout(LayoutKind.Sequential)] struct RECT {public int l,t,r,b;}
 [StructLayout(LayoutKind.Sequential)] struct GUI {public uint cbSize,flags;public IntPtr active,focus,capture,menu,movesize,caret;public RECT rc;}
 [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
 [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr w,out uint pid);
 [DllImport("user32.dll")] static extern bool GetGUIThreadInfo(uint thread,ref GUI gui);
 static bool Focused(TextBox box){uint pid;uint thread=GetWindowThreadProcessId(GetForegroundWindow(),out pid);var gui=new GUI();gui.cbSize=(uint)Marshal.SizeOf(gui);return GetGUIThreadInfo(thread,ref gui)&&pid==(uint)System.Diagnostics.Process.GetCurrentProcess().Id&&gui.focus==box.Handle&&box.Focused;}
 static string[] Scalars(string text){var list=new List<string>();for(int i=0;i<text.Length;i++){int v=text[i];if(char.IsHighSurrogate(text[i])&&i+1<text.Length&&char.IsLowSurrogate(text[i+1])){v=char.ConvertToUtf32(text,i);i++;}list.Add("U+"+v.ToString("X4"));}return list.ToArray();}
 [STAThread] public static void Main(string[] args){
  Application.EnableVisualStyles();var path=args[0];
  var form=new Form {Text="ADIT SYNTHETIC UltraVNC text",Width=850,Height=260,TopMost=true};
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
            Save-Ownership
            # A cold VM can take longer than the first window to come up. Only
            # this readiness window retries: no input has been sent yet, and the
            # retry starts a fresh target rather than reusing a half-started one.
            $ready=$false
            for($attempt=1;$attempt -le 2;$attempt++){
                $deadline=[DateTime]::UtcNow.AddSeconds(30)
                while([DateTime]::UtcNow -lt $deadline){
                    if(Test-Path -LiteralPath (Join-Path $casePath 'target-ready.txt')){$ready=$true;break}
                    $target.Refresh()
                    if($target.HasExited){break}
                    Start-Sleep -Milliseconds 50
                }
                if($ready){break}
                $sample.readiness_attempts=$attempt
                if($attempt -eq 1){
                    $sample.readiness_retry_reason=('target exited=' + $target.HasExited)
                    if(-not $target.HasExited){Stop-Owned $targetIdentity}
                    $target=Start-Process -FilePath $targetPath -ArgumentList ('"'+$casePath+'"') -PassThru
                    $targetIdentity=Track-Process $target
                    Save-Ownership
                }
            }
            if(-not $ready){throw ('TextBox readiness failed after retry; target exited=' + $target.HasExited)}
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
            $after=Read-Target (Join-Path $casePath 'actual.json');$focus=Native-Focus
            $sample.after=$after;$sample.native_after=$focus;$sample.driver_exit=$driver.ExitCode
            $sample.close_delta=Counter-Delta $connectedAfter $after
            $delta=$sample.input_delta
            $inputCountsCorrect=if($reject){$delta.press -eq 0 -and $delta.release -eq 0 -and $delta.keypress -eq 0 -and $delta.text_changes -eq 0 -and $connectedAfter.text -ceq ''}else{$delta.press -gt 0 -and $delta.release -gt 0 -and $delta.keypress -gt 0 -and $delta.text_changes -gt 0}
            $noticeCorrect=if($reject){$stdoutText -match '(?m)^text_rejected=unicode_compatibility\r?$' -and $stdoutText -match '(?m)^notice=VNC .+'}else{$stdoutText -notmatch '(?m)^text_rejected='}
            $connectedFocused=($connectedAfter.focused -and $connectedAfterFocus.foreground_pid -eq $target.Id -and $connectedAfterFocus.gui_ok -and $connectedAfterFocus.focus_hwnd -eq $connectedAfter.box_hwnd)
            $sample.checks=[ordered]@{
                driver_ready=($stdoutText -match 'ready_ms=\d+');driver_exit_zero=($driver.ExitCode -eq 0)
                acknowledged_input_phase=($sample.ack_before_received -and $sample.ack_after_received)
                same_target=($connectedAfter.pid -eq $target.Id -and $after.pid -eq $target.Id)
                target_focused=($connectedFocused -and $after.focused -and $focus.foreground_pid -eq $target.Id -and $focus.gui_ok -and $focus.focus_hwnd -eq $after.box_hwnd)
                all_input_focused=($after.unfocused_input -eq 0 -and $after.focus_losses -eq 0)
                input_counts_correct=$inputCountsCorrect;compatibility_notice_correct=$noticeCorrect
                exact_scalars=((@($connectedAfter.scalars) -join ',') -ceq (@($sample.expected_scalars) -join ','))
                final_scalars_unchanged=((@($after.scalars) -join ',') -ceq (@($sample.expected_scalars) -join ','))
            }
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
    foreach($identity in @($owned.ToArray()|Sort-Object start_ticks -Descending)){
        try{Stop-Owned $identity}catch{[void]$cleanupErrors.Add($_.Exception.Message)}
    }
    if($baseline){
        try{
            $record.after=Baseline-State
            $beforeJson=ConvertTo-Json -InputObject $baseline -Depth 10 -Compress
            $afterJson=ConvertTo-Json -InputObject $record.after -Depth 10 -Compress
            $record.original_state_unchanged=($beforeJson -ceq $afterJson)
            if(-not $record.original_state_unchanged){throw 'Original UltraVNC process/service/port/firewall state changed'}
            $record.remaining_fixture_tcp=@(Get-NetTCPConnection -LocalPort 15911 -State Listen -ErrorAction SilentlyContinue).Count
            $record.remaining_fixture_udp=@(Get-NetUDPEndpoint -LocalPort 15911 -ErrorAction SilentlyContinue).Count
            if($record.remaining_fixture_tcp -or $record.remaining_fixture_udp){throw 'Fixture port remains'}
        }catch{[void]$cleanupErrors.Add($_.Exception.Message)}
    }
    $record.owned_processes=$owned.ToArray()
    $record.cleanup_errors=$cleanupErrors.ToArray()
    $record.cleanup_verified=($record.original_state_unchanged -and $cleanupErrors.Count -eq 0)
    if(-not $record.cleanup_verified){$record.outcome='failed'}
    $record.finished_utc=[DateTime]::UtcNow.ToString('o')
    Save-Ownership
    Save-Json (Join-Path $result 'result.json') $record
}
if($record.outcome -ne 'passed'){exit 1}
