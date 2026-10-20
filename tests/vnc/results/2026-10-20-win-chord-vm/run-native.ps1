$ErrorActionPreference='Stop'
$d=([IO.Path]::GetFullPath('C:\ProgramData\AditVncKeys-0e455bba634e4dd3b05235022ad89618') -replace '\\+','\')
$env:TEMP=$d+'\results\case2tmp'
$env:TMP=$env:TEMP
$env:ADIT_NATIVE_HOOK_SELFTEST='1'
New-Item -ItemType Directory -Path $env:TEMP,($d+'\results\keycase2')|Out-Null
$destination=$d+'\payload\candidate2'
if(Test-Path -LiteralPath $destination){throw 'Fresh candidate destination required'}
Expand-Archive -LiteralPath ($d+'\payload\candidate2.zip') -DestinationPath $destination
& ($d+'\payload\windows-vm-native.ps1') -Directory $d -ResultsDirectory ($d+'\results\keycase2') -ObserveInput -NativeExecutable ($destination+'\vnc_native_acceptance.exe')
