$ErrorActionPreference='Stop'
$d='C:\ProgramData\AditVncKeys-2aa748064b34498fb59db43a6805bdde'
$env:TEMP=$d+'\results\stateless-tmp';$env:TMP=$env:TEMP;$env:ADIT_NATIVE_HOOK_SELFTEST='1'
New-Item -ItemType Directory -Path $env:TEMP,($d+'\results\stateless-gui')|Out-Null
Expand-Archive -LiteralPath ($d+'\payload\stateless-integrated.zip') -DestinationPath ($d+'\payload\stateless-integrated')
& ($d+'\payload\windows-vm-native.ps1') -Directory $d -ResultsDirectory ($d+'\results\stateless-gui') -ObserveInput -NativeExecutable ($d+'\payload\stateless-integrated\vnc_native_acceptance.exe')
