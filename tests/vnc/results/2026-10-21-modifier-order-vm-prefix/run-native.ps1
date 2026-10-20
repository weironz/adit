$ErrorActionPreference='Stop'
$d=([IO.Path]::GetFullPath('C:\ProgramData\AditVncKeys-a8f917e502845c647a9f318cf68d9f11') -replace '\\+','\')
$env:TEMP=$d+'\results\case3tmp'
$env:TMP=$env:TEMP
$env:ADIT_NATIVE_HOOK_SELFTEST='1'
New-Item -ItemType Directory -Path $env:TEMP,($d+'\results\keycase3')|Out-Null
$destination=$d+'\payload\candidate3'
if(Test-Path -LiteralPath $destination){throw 'Fresh candidate destination required'}
Expand-Archive -LiteralPath ($d+'\payload\candidate3.zip') -DestinationPath $destination
& ($d+'\payload\windows-vm-native.ps1') -Directory $d -ResultsDirectory ($d+'\results\keycase3') -ObserveInput -NativeExecutable ($destination+'\vnc_native_acceptance.exe')
