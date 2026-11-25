param([ValidateSet('Ctrl', 'Shift', 'Alt')][string]$Key)
$ErrorActionPreference = 'Stop'
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class FocusTest {
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr window, out int pid);
    [DllImport("user32.dll")] public static extern bool ShowWindowAsync(IntPtr window, int command);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr window);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte sc, uint flags, UIntPtr extra);
}
'@
function Mark([string]$Event) { '{0:O} {1} {2}' -f [DateTime]::UtcNow, $Key, $Event }
$window = [FocusTest]::GetForegroundWindow()
$ownerPid = 0
[FocusTest]::GetWindowThreadProcessId($window, [ref]$ownerPid) | Out-Null
if ((Get-Process -Id $ownerPid).ProcessName -ne 'adit') {
    throw 'Focus the connected test RDP tab before running this driver'
}
$vk, $scan = @{ Ctrl = 0xa2, 0x1d; Shift = 0xa0, 0x2a; Alt = 0xa4, 0x38 }[$Key]
[FocusTest]::keybd_event($vk, $scan, 0, [UIntPtr]::Zero)
Mark 'local-down'
try {
    Start-Sleep -Milliseconds 800
    if (-not [FocusTest]::ShowWindowAsync($window, 6)) { throw 'Minimize failed' }
    Mark 'minimize-request'
    Start-Sleep -Milliseconds 1400
    if ([FocusTest]::GetForegroundWindow() -eq $window) { throw 'Test window did not lose focus' }
} finally {
    [FocusTest]::keybd_event($vk, $scan, 2, [UIntPtr]::Zero)
    Mark 'local-up'
    [FocusTest]::ShowWindowAsync($window, 9) | Out-Null
    [FocusTest]::SetForegroundWindow($window) | Out-Null
    Mark 'restore-request'
}
