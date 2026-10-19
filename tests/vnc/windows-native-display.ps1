param([int]$Width=0,[int]$Height=0,[Parameter(Mandatory)][string]$Evidence)
$ErrorActionPreference='Stop'
if ($env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows') { throw 'Disposable hosted Windows only' }
Add-Type @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class AditDisplayMode {
 [StructLayout(LayoutKind.Sequential,CharSet=CharSet.Unicode)]
 public struct Mode {
  [MarshalAs(UnmanagedType.ByValTStr,SizeConst=32)] public string device;
  public ushort spec,driver,size,extra;
  public uint fields;
  public int x,y;
  public uint orientation,fixedOutput;
  public short color,duplex,yResolution,ttOption,collate;
  [MarshalAs(UnmanagedType.ByValTStr,SizeConst=32)] public string form;
  public ushort logPixels;
  public uint bits,width,height,flags,frequency,icmMethod,icmIntent,media,dither,reserved1,reserved2,panningWidth,panningHeight;
 }
 [DllImport("user32.dll",CharSet=CharSet.Unicode)] static extern bool EnumDisplaySettings(string device,uint index,ref Mode mode);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)] static extern int ChangeDisplaySettingsEx(string device,ref Mode mode,IntPtr hwnd,uint flags,IntPtr parameters);
 static Mode Empty() {
  int size=Marshal.SizeOf(typeof(Mode));
  if(size!=220) throw new InvalidOperationException("Unexpected DEVMODEW ABI");
  return new Mode{size=(ushort)size};
 }
 public static Mode Current() {
  Mode value=Empty();
  if(!EnumDisplaySettings(null,0xffffffff,ref value)) throw new InvalidOperationException("No current display mode");
  return value;
 }
 public static Mode[] Modes() {
  var result=new List<Mode>();
  for(uint index=0;index<512;index++) {
   Mode value=Empty(); if(!EnumDisplaySettings(null,index,ref value)) return result.ToArray();
   result.Add(value);
  }
  throw new InvalidOperationException("Unbounded display mode enumeration");
 }
 public static int Set(Mode mode) {
  mode.fields=0x180000; // Only physical pixel dimensions; no persisted registry change.
  return ChangeDisplaySettingsEx(null,ref mode,IntPtr.Zero,0,IntPtr.Zero);
 }
}
'@
$before=[AditDisplayMode]::Current()
$modes=@([AditDisplayMode]::Modes())
$record=@{source_sha=$env:GITHUB_SHA;before=$before;requested=@($Width,$Height);supported_modes=@($modes | Select-Object width,height,bits,frequency);outcome='failed';references=@('https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-enumdisplaysettingsw','https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-changedisplaysettingsexw')}
try {
 if($Width -ne 0 -or $Height -ne 0) {
  $selected=@($modes | Where-Object {$_.width -eq $Width -and $_.height -eq $Height -and $_.bits -eq 32})
  if($selected.Count -eq 0) {throw 'Requested resolution is not offered by the display driver'}
  $record.native_return=[AditDisplayMode]::Set($selected[0])
  if($record.native_return -ne 0) {throw 'ChangeDisplaySettingsEx did not apply the supported resolution'}
 }
 $record.after=[AditDisplayMode]::Current()
 if($Width -ne 0 -and ($record.after.width -ne $Width -or $record.after.height -ne $Height)) {throw 'Current native resolution differs from requested mode'}
 $record.outcome='passed'
} catch {$record.error=$_.Exception.Message;throw}
finally {$record | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $Evidence -Encoding UTF8}
