param(
    [Parameter(Mandatory)][ValidateSet(100,150,200)][int]$Scale,
    [Parameter(Mandatory)][string]$Evidence
)
$ErrorActionPreference = 'Stop'
if ($env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows') { throw 'Disposable hosted Windows only' }
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class AditDpiObservation {
 [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr hwnd);
}
'@
$root = [System.Windows.Automation.AutomationElement]::RootElement
$nameCondition = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::NameProperty,'Settings')
$comboCondition = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::AutomationIdProperty,'SystemSettings_Display_Scaling_ItemSizeOverride_ComboBox')
$itemCondition = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::ControlTypeProperty,[System.Windows.Automation.ControlType]::ListItem)
# Reopen the page after changing physical resolution. Settings otherwise keeps
# the scale choices calculated for the original 1024x768 desktop.
foreach ($oldWindow in $root.FindAll([System.Windows.Automation.TreeScope]::Children,$nameCondition)) {
    if ($null -ne $oldWindow.FindFirst([System.Windows.Automation.TreeScope]::Descendants,$comboCondition)) {
        $oldWindow.GetCurrentPattern([System.Windows.Automation.WindowPattern]::Pattern).Close()
    }
}
$closeDeadline=[DateTime]::UtcNow.AddSeconds(5)
while ($root.FindAll([System.Windows.Automation.TreeScope]::Children,$nameCondition).Count -gt 0 -and [DateTime]::UtcNow -lt $closeDeadline) { Start-Sleep -Milliseconds 100 }
Start-Process explorer.exe -ArgumentList 'ms-settings:display' -WindowStyle Hidden
$deadline = [DateTime]::UtcNow.AddSeconds(30)
$record = @{outcome='failed';target_percent=$Scale;source_sha=$env:GITHUB_SHA;choices=@();observations=@()}
try {
    $window = $null
    $combo = $null
    do {
        foreach ($candidate in $root.FindAll([System.Windows.Automation.TreeScope]::Children,$nameCondition)) {
            $control = $candidate.FindFirst([System.Windows.Automation.TreeScope]::Descendants,$comboCondition)
            if ($null -ne $control) { $window=$candidate; $combo=$control; break }
        }
        if ($null -ne $combo) { break }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    if ($null -eq $combo -or -not $combo.Current.IsEnabled) { throw 'No enabled Settings scale ComboBox' }
    $window.GetCurrentPattern([System.Windows.Automation.WindowPattern]::Pattern).SetWindowVisualState([System.Windows.Automation.WindowVisualState]::Maximized)
    $record.before_dpi = [AditDpiObservation]::GetDpiForWindow([IntPtr]$window.Current.NativeWindowHandle)
    $combo.GetCurrentPattern([System.Windows.Automation.ExpandCollapsePattern]::Pattern).Expand()
    $target = $null
    do {
        $items = $root.FindAll([System.Windows.Automation.TreeScope]::Descendants,$itemCondition)
        foreach ($item in $items) {
            $label = $item.Current.Name
            if ($label -match '^\d+%') { $record.choices += $label }
            if ($label -match ('^'+$Scale+'%(?:\s|$)') -and -not $item.Current.IsOffscreen -and $item.Current.IsEnabled) { $target=$item }
        }
        if ($null -ne $target) { break }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    if ($null -eq $target) { throw "No visible selectable $Scale% choice" }
    $record.selected_label = $target.Current.Name
    $target.GetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern).Select()
    do {
        $dpi = [AditDpiObservation]::GetDpiForWindow([IntPtr]$window.Current.NativeWindowHandle)
        $record.observations += @{dpi=$dpi;utc=[DateTime]::UtcNow.ToString('o')}
        if ($dpi -eq (96*$Scale/100)) { break }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    if ($dpi -ne (96*$Scale/100)) { throw 'Actual Settings HWND DPI did not reach requested OS scale' }
    $record.after_dpi=$dpi
    $record.outcome='passed'
} catch {
    $record.error=$_.Exception.Message
    throw
} finally {
    $record.choices = @($record.choices | Sort-Object -Unique)
    $record | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $Evidence -Encoding UTF8
}
