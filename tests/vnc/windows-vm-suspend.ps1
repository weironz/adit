param([Parameter(Mandatory)][string]$Directory,[switch]$Suspend,[string]$ExternalResumeProof)
$ErrorActionPreference='Stop'
if(-not (Test-Path -LiteralPath $Directory)){throw 'An owned evidence directory must already exist'}
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class AditSuspend {
 [DllImport("kernel32.dll",SetLastError=true)] public static extern IntPtr CreateWaitableTimer(IntPtr a,bool manual,string name);
 [DllImport("kernel32.dll",SetLastError=true)] public static extern bool SetWaitableTimer(IntPtr h,ref long due,int period,IntPtr cb,IntPtr arg,bool resume);
 [DllImport("kernel32.dll")] public static extern bool CancelWaitableTimer(IntPtr h);
 [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
 [DllImport("powrprof.dll",SetLastError=true)] public static extern bool SetSuspendState(bool hibernate,bool force,bool disableWake);
}
'@
$record=[ordered]@{outcome='failed';before_utc=(Get-Date).ToUniversalTime().ToString('o');scope='Wake timer observation; suspend requires independently verified external recovery';suspend_requested=[bool]$Suspend}
$timer=[AditSuspend]::CreateWaitableTimer([IntPtr]::Zero,$true,$null)
if($timer -eq [IntPtr]::Zero){throw 'Cannot allocate safety wake timer'}
try {
 $due=-300000000L
 $record.wake_timer_accepted=[AditSuspend]::SetWaitableTimer($timer,[ref]$due,0,[IntPtr]::Zero,[IntPtr]::Zero,$true)
 $record.wake_timer_error=[Runtime.InteropServices.Marshal]::GetLastWin32Error()
 $record|ConvertTo-Json|Set-Content -LiteralPath (Join-Path $Directory 'power-before.json') -Encoding UTF8
 # A VMware guest accepted this timer but did not wake. Timer acceptance alone
 # cannot authorize another unattended suspend without a proven host-side path.
 if(-not $Suspend){$record.outcome='capability_only';return}
 if(-not $record.wake_timer_accepted -or $record.wake_timer_error -ne 0){throw 'Resume timer was not accepted; do not suspend'}
 if(-not $ExternalResumeProof -or -not (Test-Path -LiteralPath $ExternalResumeProof)){throw 'An independently verified external resume path is required'}
 $proof=Get-Content -LiteralPath $ExternalResumeProof -Raw|ConvertFrom-Json
 $uuid=(Get-CimInstance Win32_ComputerSystemProduct).UUID
 $age=([datetime]::UtcNow-[datetime]$proof.verified_utc).TotalMinutes
 if($proof.vm_uuid -ne $uuid -or $proof.resume_verified -ne $true -or $age -lt 0 -or $age -gt 5){throw 'External resume evidence does not identify this VM or is stale'}
 $clock=[Diagnostics.Stopwatch]::StartNew()
 $record.suspend_return=[AditSuspend]::SetSuspendState($false,$false,$false)
 $record.suspend_error=[Runtime.InteropServices.Marshal]::GetLastWin32Error()
 $record.elapsed_ms=$clock.ElapsedMilliseconds
 $record.after_utc=(Get-Date).ToUniversalTime().ToString('o')
 if(-not $record.suspend_return){throw 'Windows rejected the suspend request'}
 $record.outcome='returned'
} catch {$record.error=$_.Exception.Message;throw}
finally {
 [AditSuspend]::CancelWaitableTimer($timer)|Out-Null
 [AditSuspend]::CloseHandle($timer)|Out-Null
 $record|ConvertTo-Json|Set-Content -LiteralPath (Join-Path $Directory 'power-result.json') -Encoding UTF8
}
