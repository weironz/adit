param(
    [Parameter(Mandatory)][string]$Guest,
    [Parameter(Mandatory)][string]$Results,
    [ValidateSet('echo','cmd-probe')][string]$Mode = 'echo'
)
$ErrorActionPreference = 'Stop'
if (-not $IsWindows -or $env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows') { throw 'Disposable Windows GitHub runner only' }
$sources = Get-Content -LiteralPath "$PSScriptRoot/sources.json" -Raw | ConvertFrom-Json
$toolsRoot = Join-Path $env:RUNNER_TEMP 'adit-netem-qemu-tools'
if (Test-Path -LiteralPath $toolsRoot) { throw 'Fresh tools directory required' }
New-Item -ItemType Directory -Path $toolsRoot | Out-Null
$installer = Join-Path $toolsRoot 'qemu-installer.exe'
# Never run the installer or suppress protection. Any download/read/extraction
# or execution failure is terminal, including a Defender quarantine.
Invoke-WebRequest -Uri $sources.qemu.url -OutFile $installer -TimeoutSec 120
if ((Get-FileHash -LiteralPath $installer -Algorithm SHA512).Hash.ToLowerInvariant() -ne $sources.qemu.sha512) { throw 'QEMU official checksum mismatch' }
$signature = Get-AuthenticodeSignature -LiteralPath $installer
[ordered]@{ url=$sources.qemu.url; sha512=$sources.qemu.sha512; checksum_source=$sources.qemu.checksum_source; authenticode_status=[string]$signature.Status; signer_subject=$signature.SignerCertificate.Subject; signer_thumbprint=$signature.SignerCertificate.Thumbprint; note='Official site reports its expired signing certificate; pinned official SHA512 is mandatory. Installer not executed.' } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $toolsRoot 'provenance.json') -Encoding utf8
& 7z x $installer "-o$toolsRoot/extracted" -y | Out-File -LiteralPath (Join-Path $toolsRoot 'extraction.txt') -Encoding utf8
if ($LASTEXITCODE -ne 0) { throw "QEMU extraction failed: $LASTEXITCODE" }
$qemu = @(Get-ChildItem -LiteralPath (Join-Path $toolsRoot 'extracted') -Filter qemu-system-x86_64.exe -File -Recurse)
if ($qemu.Count -ne 1) { throw 'Expected exactly one extracted x86_64 system emulator' }
& $qemu[0].FullName --version | Out-File -LiteralPath (Join-Path $toolsRoot 'qemu-version.txt') -Encoding utf8
if ($LASTEXITCODE -ne 0) { throw "QEMU version failed: $LASTEXITCODE" }
if ($Mode -eq 'cmd-probe') {
    & python "$PSScriptRoot/cmd_probe.py" --qemu $qemu[0].FullName --guest $Guest --results $Results
} else {
    & python "$PSScriptRoot/run.py" --qemu $qemu[0].FullName --guest $Guest --results $Results
}
if ($LASTEXITCODE -ne 0) { throw "Packet-path PoC failed: $LASTEXITCODE" }
