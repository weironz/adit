#Requires -Version 5.1
<#
.SYNOPSIS
  Prepare the Windows x64 runner for Adit CI and installer builds. Never publishes.
.DESCRIPTION
  Idempotent under the actual runner identity. Portable tools use its writable
  cache; existing system tools are reused. A bare machine can install Git,
  PowerShell and MSVC/SDK with -InstallSystemDependencies from an already elevated
  shell. This script never changes the runner account or restarts its service.
  -CheckOnly reports missing dependencies without installing/updating anything.
  -BuildOnly omits GitHub CLI and Inno Setup, which ordinary CI does not need.
#>
param([switch]$CheckOnly, [switch]$BuildOnly, [switch]$InstallSystemDependencies)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor 3072
if (-not [Environment]::Is64BitOperatingSystem) { throw 'Windows x64 required' }
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$admin = ([Security.Principal.WindowsPrincipal]$identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if ($InstallSystemDependencies -and -not $admin) { throw '-InstallSystemDependencies requires an already elevated administrator; the runner account is not changed' }
$tools = if ($env:RUNNER_TOOL_CACHE) { $env:RUNNER_TOOL_CACHE } else { Join-Path $env:LOCALAPPDATA 'AditRunnerTools' }
Write-Host "Runner identity: $($identity.Name); administrator=$admin; check_only=$CheckOnly"

function Add-ToolPath([string]$Path) {
    if (($env:PATH -split ';') -notcontains $Path) { $env:PATH = "$Path;$env:PATH" }
    if ($env:GITHUB_PATH) { [IO.File]::AppendAllText($env:GITHUB_PATH, "$Path`n", [Text.UTF8Encoding]::new($false)) }
}
function Set-ToolEnvironment([string]$Name, [string]$Value) {
    [Environment]::SetEnvironmentVariable($Name, $Value, 'Process')
    if ($env:GITHUB_ENV) { [IO.File]::AppendAllText($env:GITHUB_ENV, "$Name=$Value`n", [Text.UTF8Encoding]::new($false)) }
}
function Find-Program([string]$Name) {
    $cmd = Get-Command $Name -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($cmd) { return $cmd.Source }
}
function Download-Verified([string]$Url, [string]$Hash, [string]$Destination) {
    New-Item -ItemType Directory -Path (Split-Path $Destination) -Force | Out-Null
    Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $Destination
    if ((Get-FileHash -LiteralPath $Destination -Algorithm SHA256).Hash.ToLowerInvariant() -cne $Hash) { throw "Download SHA256 mismatch: $Destination" }
    return $Destination
}
function Run-Installer([string]$File, [string]$Arguments, [int]$TimeoutSeconds = 600) {
    $p = Start-Process -FilePath $File -ArgumentList $Arguments -WindowStyle Hidden -PassThru
    if (-not $p.WaitForExit($TimeoutSeconds * 1000)) { throw "Installer timed out (owned PID $($p.Id)); inspect before retrying" }
    $p.Refresh()
    if ($p.ExitCode -notin 0, 3010) { throw "Installer failed with exit code $($p.ExitCode)" }
    if ($p.ExitCode -eq 3010) { throw 'Installer requires a reboot; no reboot was performed' }
}
function Require-SystemInstall([string]$Name) {
    if ($CheckOnly -or -not $InstallSystemDependencies) { throw "$Name missing. Run this script with -InstallSystemDependencies in an already elevated shell; service privileges are never increased automatically." }
}
function Zip-Tool([string]$Name, [string]$Version, [string]$Url, [string]$Hash, [string]$Exe) {
    $dir = Join-Path $tools "adit-$Name-$Version"
    $found = @(Get-ChildItem -LiteralPath $dir -Filter $Exe -File -Recurse -ErrorAction SilentlyContinue)
    if (-not $found.Count) {
        if ($CheckOnly) { throw "$Name missing from runner tool cache" }
        $zip = Download-Verified $Url $Hash (Join-Path $tools "adit-$Name-$Version.zip")
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
        Expand-Archive -LiteralPath $zip -DestinationPath $dir -Force
        Remove-Item -LiteralPath $zip
        $found = @(Get-ChildItem -LiteralPath $dir -Filter $Exe -File -Recurse)
    }
    if ($found.Count -ne 1) { throw "Expected one $Exe in $dir" }
    Add-ToolPath $found[0].DirectoryName
    return $found[0].FullName
}

if (-not (Find-Program git)) {
    Require-SystemInstall 'Git for Windows'
    $gitSetup = Download-Verified 'https://github.com/git-for-windows/git/releases/download/v2.56.0.windows.2/Git-2.56.0.2-64-bit.exe' '52188f917b378f00c70ec136bcf090005f30d44fbc4eba0bce759cc6592d60f6' (Join-Path $tools 'git-setup.exe')
    Run-Installer $gitSetup '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART'
    Add-ToolPath (Join-Path $env:ProgramFiles 'Git\cmd')
}
$bash = Join-Path $env:ProgramFiles 'Git\bin\bash.exe'
if (Test-Path -LiteralPath $bash) { Add-ToolPath (Split-Path $bash) }
if (-not (Find-Program bash)) { throw 'Git Bash is missing' }
if (-not (Find-Program pwsh)) {
    $pwshInstalled = Join-Path $env:ProgramFiles 'PowerShell\7\pwsh.exe'
    if (-not (Test-Path -LiteralPath $pwshInstalled)) {
        Require-SystemInstall 'PowerShell 7'
        $msi = Download-Verified 'https://github.com/PowerShell/PowerShell/releases/download/v7.6.7/PowerShell-7.6.7-win-x64.msi' 'ec575cef9ec56fcd905de0d5eeac9cb7f81a0a65abb87bb5cd0e87fdfe2b8cc0' (Join-Path $tools 'pwsh.msi')
        Run-Installer 'msiexec.exe' "/i `"$msi`" /qn /norestart"
    }
    Add-ToolPath (Split-Path $pwshInstalled)
}

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
function Get-MSVC {
    if (Test-Path -LiteralPath $vswhere) { & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath }
}
if (-not (Get-MSVC) -or -not @(Get-ChildItem 'C:\Program Files (x86)\Windows Kits\10\Lib\*\um\x64\kernel32.lib' -ErrorAction SilentlyContinue).Count) {
    Require-SystemInstall 'MSVC x64 / Windows SDK'
    $vsSetup = Join-Path $tools 'vs-BuildTools.exe'
    New-Item -ItemType Directory -Path $tools -Force | Out-Null
    Invoke-WebRequest -UseBasicParsing -Uri 'https://aka.ms/vs/17/release/vs_BuildTools.exe' -OutFile $vsSetup
    $sig = Get-AuthenticodeSignature -LiteralPath $vsSetup
    if ($sig.Status -ne 'Valid' -or $sig.SignerCertificate.Subject -notmatch 'Microsoft Corporation') { throw 'Visual Studio installer signature mismatch' }
    Run-Installer $vsSetup '--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended' 6000
}
$vc = Get-MSVC
if (-not $vc) { throw 'MSVC x64 compiler unavailable after setup' }
if ($InstallSystemDependencies -and -not $CheckOnly) {
    Set-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem' -Name LongPathsEnabled -Value 1 -Type DWord
    & git config --system core.longpaths true
    if ($LASTEXITCODE -ne 0) { throw 'Git long-path setup failed' }
}

if (-not (Find-Program rustup)) {
    $rust = Join-Path $tools 'adit-rust'
    Set-ToolEnvironment RUSTUP_HOME (Join-Path $rust 'rustup')
    Set-ToolEnvironment CARGO_HOME (Join-Path $rust 'cargo')
    Add-ToolPath (Join-Path $env:CARGO_HOME 'bin')
    if (-not (Find-Program rustup)) {
        if ($CheckOnly) { throw 'Rustup missing' }
        $url = 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe'
        $hashText = (Invoke-WebRequest -UseBasicParsing -Uri ($url + '.sha256')).Content
        if ($hashText -notmatch '^([a-f0-9]{64})\b') { throw 'Invalid rustup checksum response' }
        $init = Download-Verified $url $Matches[1] (Join-Path $tools 'rustup-init.exe')
        & $init -y --no-modify-path --profile minimal --default-toolchain stable
        if ($LASTEXITCODE -ne 0) { throw 'Rustup install failed' }
    }
}
if (-not $CheckOnly) {
    & rustup toolchain install stable --profile minimal --component clippy
    if ($LASTEXITCODE -ne 0) { throw 'Stable Rust/Clippy preparation failed' }
}
& rustc +stable --version
if ($LASTEXITCODE -ne 0) { throw 'Stable Rust unavailable' }
$components = & rustup component list --toolchain stable --installed
if ($LASTEXITCODE -ne 0 -or -not ($components -match '^clippy-')) { throw 'Clippy component missing' }
$just = Zip-Tool 'just' '1.59.0' 'https://github.com/casey/just/releases/download/1.59.0/just-1.59.0-x86_64-pc-windows-msvc.zip' '9c5463e2d0974e31bee71346aa5cbf19167de1585a4abc1bae7b3b04604ea9c0' 'just.exe'
& $just --version
if ($LASTEXITCODE -ne 0) { throw 'just executable failed' }

if (-not $BuildOnly) {
    $gh = Find-Program gh
    if (-not $gh) { $gh = Zip-Tool 'gh' '2.102.1' 'https://github.com/cli/cli/releases/download/v2.102.1/gh_2.102.1_windows_amd64.zip' '843c0dc119f014d9458aeaff96c44c16fc4b8e556c1c4c5df9bab5ba002356e9' 'gh.exe' }
    Add-ToolPath (Split-Path $gh)
    & $gh --version
    if ($LASTEXITCODE -ne 0) { throw 'GitHub CLI unavailable' }
    $inno = Join-Path $tools 'adit-inno-6.7.3'
    $iscc = Join-Path $inno 'ISCC.exe'
    if (-not (Test-Path -LiteralPath $iscc)) {
        if ($CheckOnly) { throw 'Pinned Inno Setup 6.7.3 missing from runner tool cache' }
        $setup = Download-Verified 'https://github.com/jrsoftware/issrc/releases/download/is-6_7_3/innosetup-6.7.3.exe' '9c73c3bae7ed48d44112a0f48e66742c00090bdb5bef71d9d3c056c66e97b732' (Join-Path $tools 'innosetup-6.7.3.exe')
        Run-Installer $setup "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /CURRENTUSER /PORTABLE=1 /DIR=`"$inno`""
    }
    if (-not (Test-Path -LiteralPath $iscc)) { throw 'Inno Setup compiler not found after installation' }
    Add-ToolPath $inno
    Set-ToolEnvironment ADIT_ISCC $iscc
    Write-Host "Inno Setup: $iscc"
}
Write-Host "MSVC: $vc"
Write-Host 'Runner dependencies ready; no service restart, account change, tag or release was performed.'
