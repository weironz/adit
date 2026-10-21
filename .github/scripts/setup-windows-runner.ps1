$ErrorActionPreference = 'Stop'
if (-not [Environment]::Is64BitOperatingSystem) { throw 'Windows x64 runner required' }
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if (-not (Test-Path -LiteralPath $vswhere)) { throw 'Visual Studio Build Tools are missing' }
$vs = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vs) { throw 'MSVC x64 C++ tools are missing' }
$cl = @(Get-ChildItem -LiteralPath (Join-Path $vs 'VC\Tools\MSVC') -Filter cl.exe -Recurse | Where-Object FullName -like '*\Hostx64\x64\cl.exe')
if (-not $cl.Count) { throw 'MSVC x64 compiler not found' }
$sdk = @(Get-ChildItem 'C:\Program Files (x86)\Windows Kits\10\Lib\*\um\x64\kernel32.lib')
if (-not $sdk.Count) { throw 'Windows SDK x64 libraries not found' }
& pwsh -NoProfile -Command '$PSVersionTable.PSVersion.ToString()'
if ($LASTEXITCODE -ne 0) { throw 'PowerShell 7 unavailable' }

# Install in this runner's writable cache, without elevating its service account.
$tools = Join-Path $env:RUNNER_TOOL_CACHE 'adit-just-1.59.0'
$just = Join-Path $tools 'just.exe'
if (-not (Test-Path -LiteralPath $just)) {
    New-Item -ItemType Directory -Path $tools -Force | Out-Null
    $zip = Join-Path $env:RUNNER_TEMP 'adit-just-1.59.0.zip'
    & curl.exe -fL --retry 3 -o $zip 'https://github.com/casey/just/releases/download/1.59.0/just-1.59.0-x86_64-pc-windows-msvc.zip'
    if ($LASTEXITCODE -ne 0) { throw 'just download failed' }
    if ((Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant() -cne '9c5463e2d0974e31bee71346aa5cbf19167de1585a4abc1bae7b3b04604ea9c0') { throw 'just archive SHA256 mismatch' }
    Expand-Archive -LiteralPath $zip -DestinationPath $tools -Force
    Remove-Item -LiteralPath $zip
}
[IO.File]::AppendAllText($env:GITHUB_PATH, $tools + "`n", [Text.UTF8Encoding]::new($false))
& $just --version
if ($LASTEXITCODE -ne 0) { throw 'just executable failed' }
Write-Output ('MSVC: ' + $cl[0].FullName)
Write-Output ('Windows SDK: ' + $sdk[-1].FullName)
