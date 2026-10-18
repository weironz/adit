param([Parameter(Mandatory)][string]$Results)
$ErrorActionPreference = 'Stop'
if ($env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows') { throw 'Disposable hosted Windows only' }
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class AditTokenCapability {
 [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
 [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
 [DllImport("advapi32.dll", SetLastError=true)] static extern bool OpenProcessToken(IntPtr p, uint access, out IntPtr token);
 [DllImport("advapi32.dll", SetLastError=true)] static extern bool GetTokenInformation(IntPtr token, int kind, IntPtr buffer, int size, out int needed);
 public static int[] Inspect() {
   IntPtr token;
   if (!OpenProcessToken(GetCurrentProcess(),8,out token)) throw new System.ComponentModel.Win32Exception();
   IntPtr buffer = Marshal.AllocHGlobal(IntPtr.Size);
   try {
     int needed;
     if (!GetTokenInformation(token,18,buffer,4,out needed)) throw new System.ComponentModel.Win32Exception();
     int elevation = Marshal.ReadInt32(buffer);
     bool linked = GetTokenInformation(token,19,buffer,IntPtr.Size,out needed);
     int error = linked ? 0 : Marshal.GetLastWin32Error();
     if (linked) CloseHandle(Marshal.ReadIntPtr(buffer));
     return new int[]{elevation,linked?1:0,error};
   } finally { Marshal.FreeHGlobal(buffer); CloseHandle(token); }
 }
}
'@
@{token_elevation_type_and_linked=[AditTokenCapability]::Inspect();source_sha=$env:GITHUB_SHA} | ConvertTo-Json | Set-Content (Join-Path $Results 'token.json') -Encoding UTF8
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
$before = @(Get-Process SystemSettings -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Id)
Start-Process explorer.exe -ArgumentList 'ms-settings:display' -WindowStyle Hidden
$deadline = [DateTime]::UtcNow.AddSeconds(25)
$found = @()
do {
    $found = @(Get-Process SystemSettings -ErrorAction SilentlyContinue | Where-Object { $_.Id -notin $before })
    if ($found.Count -gt 0) { break }
    Start-Sleep -Milliseconds 200
} while ([DateTime]::UtcNow -lt $deadline)
$record = @{fresh_processes=@($found | Select-Object Id,StartTime);existing_pids=$before;controls=@();source_sha=$env:GITHUB_SHA}
$uiDeadline = [DateTime]::UtcNow.AddSeconds(25)
do {
$record.controls = @()
foreach ($process in $found) {
    $condition = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::ProcessIdProperty,$process.Id)
    $windows = [System.Windows.Automation.AutomationElement]::RootElement.FindAll([System.Windows.Automation.TreeScope]::Children,$condition)
    foreach ($window in $windows) {
        $nodes = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants,[System.Windows.Automation.Condition]::TrueCondition)
        foreach ($node in $nodes | Select-Object -First 200) {
            try {
                $current = $node.Current
                $record.controls += @{name=$current.Name;automation_id=$current.AutomationId;type=$current.ControlType.ProgrammaticName;enabled=$current.IsEnabled;offscreen=$current.IsOffscreen}
            } catch { $record.controls += @{error=$_.Exception.Message} }
        }
    }
}
if ($record.controls.Count -gt 10) { break }
Start-Sleep -Milliseconds 250
} while ([DateTime]::UtcNow -lt $uiDeadline)
$record | ConvertTo-Json -Depth 8 | Set-Content (Join-Path $Results 'display-settings-controls.json') -Encoding UTF8
