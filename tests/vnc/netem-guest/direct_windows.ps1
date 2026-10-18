param([Parameter(Mandatory)][string]$Guest, [Parameter(Mandatory)][string]$Results)
$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
if (-not $IsWindows -or $env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or
    $env:RUNNER_OS -ne 'Windows' -or -not $env:RUNNER_TEMP -or -not $env:GITHUB_SHA) { throw 'Disposable Windows GitHub runner only' }
$toolsRoot=Join-Path $env:RUNNER_TEMP 'adit-netem-direct-tools'
if (Test-Path -LiteralPath $toolsRoot) { throw 'Fresh direct-QEMU tools directory required' }
[void][IO.Directory]::CreateDirectory($toolsRoot)
$url='https://qemu.weilnetz.de/w64/qemu-w64-setup-20260811.exe'
$sha512='5bcf9eed634e8575a37b74f445af41a2fe4106da512d0c30c368301d4c105037fdfab40a5287367a28a957624cddebbc8c07e16c88ab6634f554cdf3d16bf543'
$record=[ordered]@{ outcome='failed'; source_sha=$env:GITHUB_SHA; run_id=$env:GITHUB_RUN_ID; url=$url; expected_sha512=$sha512; scope='One direct host-alias fake calibration. Installer extracted only; no guestfwd/cmd/helper, DLL replacement, VNC, credentials, host networking or protection changes.' }
try {
    $sources=Get-Content -LiteralPath "$PSScriptRoot/sources.json" -Raw | ConvertFrom-Json
    if ($sources.qemu.url -ne $url -or $sources.qemu.sha512 -ne $sha512) { throw 'Fixed source JSON differs' }
    $installer=Join-Path $toolsRoot 'qemu-installer.exe'
    Invoke-WebRequest -Uri $url -OutFile $installer -TimeoutSec 90
    $record.actual_sha512=(Get-FileHash -LiteralPath $installer -Algorithm SHA512).Hash.ToLowerInvariant()
    if ($record.actual_sha512 -ne $sha512) { throw 'Fixed official QEMU checksum mismatch' }
    & 7z x $installer "-o$toolsRoot/extracted" -y 2>&1 | Out-File -LiteralPath (Join-Path $toolsRoot 'extraction.txt') -Encoding utf8
    if ($LASTEXITCODE -ne 0) { throw "QEMU extraction failed: $LASTEXITCODE" }
    $qemu=Join-Path $toolsRoot 'extracted/qemu-system-x86_64.exe'
    if ((Get-FileHash -LiteralPath $qemu -Algorithm SHA256).Hash.ToLowerInvariant() -ne '47d57a6072e0bb3bd98f87926eb129eb1736dfe818c67b3b81ef7ce4edd0b3cd') { throw 'Exact inventoried QEMU executable required' }
    # Any read/extraction/execution or protection failure is terminal. Do not
    # retry downloads, repackage a blocked binary, or alter host protections.
    & python "$PSScriptRoot/direct_probe.py" --qemu $qemu --guest $Guest --results $Results
    if ($LASTEXITCODE -ne 0) { throw "Direct host-alias fake calibration failed: $LASTEXITCODE" }
    $record.outcome='fake_calibration_completed'
} catch {
    $record.error=$_.Exception.Message
    $_ | Out-String | Set-Content -LiteralPath (Join-Path $toolsRoot 'failure.txt') -Encoding utf8
    throw
} finally {
    $record | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $toolsRoot 'provenance.json') -Encoding utf8
}
