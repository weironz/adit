param([Parameter(Mandatory)][string]$Directory,
    [ValidatePattern('^[a-z0-9-]{1,64}$')][string]$ResultName='vendor-unicode')
$ErrorActionPreference='Stop'
$root=[IO.Path]::GetFullPath($Directory)
if([Diagnostics.Process]::GetCurrentProcess().SessionId -eq 0 -or
    -not (Test-Path (Join-Path $root 'owner.json')) -or
    (Split-Path $root -Leaf) -notmatch '^AditVncKeys-[a-f0-9]{32}$'){
    throw 'Requires an owned, interactive disposable VM test directory'
}
$payload=Join-Path $root 'payload'
$result=Join-Path $payload $ResultName
if(Test-Path $result){throw 'Use a fresh owned result directory'}
New-Item -ItemType Directory $result | Out-Null
$utf8=New-Object Text.UTF8Encoding($false)
$record=@{outcome='failed'; scope='SessionManager Unicode input to portable UltraVNC and an actual Windows TextBox; no native IME acceptance'; clipboard_send_used=$false}
$server=$null;$target=$null;$driver=$null
try {
    if(@(Get-Process winvnc -ErrorAction SilentlyContinue).Count -or
        @(Get-NetTCPConnection -LocalPort 15911 -ErrorAction SilentlyContinue).Count -or
        @(Get-NetUDPEndpoint -LocalPort 15911 -ErrorAction SilentlyContinue).Count){throw 'Existing vendor process or fixture port'}
    $archive=Join-Path $payload 'UltraVNC_1830.zip'
    $hash=(Get-FileHash $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    if($hash -ne '11163a0b9b86321bf6403eb0cf84b81f7230ce32e6291377e1fe5beb987f7e4f'){throw 'Pinned vendor archive checksum mismatch'}
    $record.archive_sha256=$hash
    Expand-Archive $archive (Join-Path $result 'portable')
    $serverPath=Join-Path $result 'portable/x64/winvnc.exe'
    $record.server_sha256=(Get-FileHash $serverPath -Algorithm SHA256).Hash
    $record.server_version=[Diagnostics.FileVersionInfo]::GetVersionInfo($serverPath).FileVersion
    $ini=Join-Path (Split-Path $serverPath) 'ultravnc.ini'
    @'
[admin]
UseRegistry=0
SocketConnect=1
AutoPortSelect=0
PortNumber=15911
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
DebugMode=0
[poll]
PollFullScreen=1
OnlyPollOnEvent=0
EnableDriver=0
EnableHook=0
EnableVirtual=0
[UltraVNC]
'@ | Set-Content -LiteralPath $ini -Encoding ascii
    # Standard VNC password storage uses LSB-first DES key bits. This is a
    # public synthetic password; neither the process arguments nor env hold it.
    $fixed=[byte[]]@(23,82,107,6,35,78,88,7);$key=New-Object byte[] 8
    for($i=0;$i -lt 8;$i++){for($bit=0;$bit -lt 8;$bit++){$key[$i]=$key[$i] -bor ((($fixed[$i] -shr $bit) -band 1) -shl (7-$bit))}}
    $plain=New-Object byte[] 8;[Text.Encoding]::ASCII.GetBytes('adit-ci').CopyTo($plain,0)
    $des=[Security.Cryptography.DES]::Create()
    try{$des.Key=$key;$des.Mode='ECB';$des.Padding='None';$transform=$des.CreateEncryptor();try{$encrypted=$transform.TransformFinalBlock($plain,0,8)}finally{$transform.Dispose()}}
    finally{$des.Dispose();[Array]::Clear($plain,0,8);[Array]::Clear($key,0,8)}
    Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class AditUnicodeIni {
 [DllImport("kernel32.dll", EntryPoint="WritePrivateProfileStructW", CharSet=CharSet.Unicode)]
 public static extern bool Write(string section,string key,byte[] value,uint size,string file);
 [DllImport("kernel32.dll", EntryPoint="GetPrivateProfileStructW", CharSet=CharSet.Unicode)]
 public static extern bool Read(string section,string key,byte[] value,uint size,string file);
}
'@
    if(-not [AditUnicodeIni]::Write('UltraVNC','passwd',$encrypted,8,$ini)){throw 'Password structure write failed'}
    $readback=New-Object byte[] 8
    if(-not [AditUnicodeIni]::Read('UltraVNC','passwd',$readback,8,$ini) -or [BitConverter]::ToString($readback) -ne [BitConverter]::ToString($encrypted)){throw 'Password structure readback failed'}
    $targetPath=Join-Path $result 'synthetic-text.exe'
    $literal=$result.Replace('"','""')
    Add-Type -ReferencedAssemblies System.Windows.Forms,System.Drawing -OutputAssembly $targetPath -OutputType WindowsApplication -TypeDefinition @"
using System;
using System.IO;
using System.Text;
using System.Windows.Forms;
public static class SyntheticText {
 [STAThread] public static void Main() {
  Application.EnableVisualStyles();
  var form=new Form {Text="ADIT SYNTHETIC Unicode target",Width=850,Height=260,TopMost=true};
  var box=new TextBox {Multiline=true,Dock=DockStyle.Fill,ImeMode=ImeMode.Disable};
  var path=@"$literal";
  box.TextChanged+=(s,e)=>File.WriteAllText(Path.Combine(path,"actual-text.txt"),box.Text,new UTF8Encoding(false));
  form.Controls.Add(box);
  form.Shown+=(s,e)=>{form.Activate();box.Focus();File.WriteAllText(Path.Combine(path,"target-ready.txt"),"ready");};
  var timer=new Timer {Interval=120000};timer.Tick+=(s,e)=>form.Close();timer.Start();
  Application.Run(form);
 }
}
"@
    $server=Start-Process $serverPath -ArgumentList @('-config',('"'+$ini+'"'),'-run') -WorkingDirectory (Split-Path $serverPath) -WindowStyle Hidden -PassThru
    $target=Start-Process $targetPath -PassThru
    $deadline=[DateTime]::UtcNow.AddSeconds(25)
    do{
        $listeners=@(Get-NetTCPConnection -LocalPort 15911 -State Listen -ErrorAction SilentlyContinue)
        if($listeners.Count -and (Test-Path (Join-Path $result 'target-ready.txt'))){break}
        if($server.HasExited -or [DateTime]::UtcNow -gt $deadline){throw 'Vendor/target readiness timeout'}
        Start-Sleep -Milliseconds 100
    }while($true)
    $record.tcp=@($listeners|Select-Object LocalAddress,LocalPort,OwningProcess)
    $record.udp=@(Get-NetUDPEndpoint -OwningProcess $server.Id -ErrorAction SilentlyContinue|Select-Object LocalAddress,LocalPort,OwningProcess)
    $record.firewall_profiles=@(Get-NetFirewallProfile|Select-Object Name,Enabled)
    if(@($listeners|Where-Object {$_.LocalAddress -ne '127.0.0.1' -or $_.OwningProcess -ne $server.Id}).Count){throw 'Vendor listener is not owned loopback TCP'}
    $record.target_pid=$target.Id;$record.server_pid=$server.Id
    Add-Type @'
using System;using System.Runtime.InteropServices;
public static class AditUnicodeForeground {
 [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
 [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr w,out uint pid);
}
'@
    [uint32]$foreground=0;[void][AditUnicodeForeground]::GetWindowThreadProcessId([AditUnicodeForeground]::GetForegroundWindow(),[ref]$foreground)
    $record.foreground_pid=$foreground
    if($foreground -ne $target.Id){throw 'Synthetic target is not foreground; no input sent'}
    $expected='a'+[char]0x4e2d+[char]0x6587+[char]::ConvertFromUtf32(0x1f60a)+'e'+[char]0x301
    $info=New-Object Diagnostics.ProcessStartInfo
    $info.FileName=Join-Path $payload 'vnc_fixture_driver.exe';$info.Arguments='127.0.0.1 15911 25'
    $info.UseShellExecute=$false;$info.CreateNoWindow=$true
    $info.RedirectStandardInput=$true
    $info.RedirectStandardOutput=$true;$info.RedirectStandardError=$true
    $driver=New-Object Diagnostics.Process;$driver.StartInfo=$info
    [void]$driver.Start();$stdout=$driver.StandardOutput.ReadToEndAsync();$stderr=$driver.StandardError.ReadToEndAsync()
    # Windows PowerShell 5.1 lacks ProcessStartInfo.StandardInputEncoding.
    # Write UTF-8 explicitly instead of corrupting Unicode through its code page.
    $inputWriter=New-Object IO.StreamWriter($driver.StandardInput.BaseStream,$utf8)
    $inputWriter.WriteLine('adit-ci');$inputWriter.WriteLine('WAIT 500');$inputWriter.WriteLine('TEXT '+$expected);$inputWriter.WriteLine('WAIT 1500');$inputWriter.WriteLine('CLOSE');$inputWriter.Dispose()
    if(-not $driver.WaitForExit(35000)){throw 'Protocol driver timeout'}
    [IO.File]::WriteAllText((Join-Path $result 'driver.stdout.txt'),$stdout.Result,$utf8)
    [IO.File]::WriteAllText((Join-Path $result 'driver.stderr.txt'),$stderr.Result,$utf8)
    $record.driver_exit=$driver.ExitCode
    $record.expected=$expected
    $actualPath=Join-Path $result 'actual-text.txt'
    $record.actual=if(Test-Path $actualPath){[IO.File]::ReadAllText($actualPath,$utf8)}else{''}
    $record.exact_match=($record.actual -ceq $expected)
    if($driver.ExitCode -ne 0 -or -not $record.exact_match){throw 'Actual vendor text consumption differs; see synthetic result'}
    $record.outcome='passed'
}catch{$record.error=$_.Exception.Message}
finally{
    foreach($process in @($driver,$target,$server)){
        if($process -and -not $process.HasExited){Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue;[void]$process.WaitForExit(5000)}
    }
    $record.finished_utc=[DateTime]::UtcNow.ToString('o')
    $record.remaining_fixture_tcp=@(Get-NetTCPConnection -LocalPort 15911 -State Listen -ErrorAction SilentlyContinue).Count
    $record.remaining_fixture_udp=@(Get-NetUDPEndpoint -LocalPort 15911 -ErrorAction SilentlyContinue).Count
    [IO.File]::WriteAllText((Join-Path $result 'result.json'),($record|ConvertTo-Json -Depth 8),$utf8)
}
