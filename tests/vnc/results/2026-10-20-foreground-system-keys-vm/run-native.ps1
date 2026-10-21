$ErrorActionPreference='Stop'
$d='C:\ProgramData\AditVncKeys-74c435392ff548f8b04dfb36481e310a'
$env:TEMP=$d+'\results\tmp'
$env:TMP=$env:TEMP
$env:ADIT_NATIVE_HOOK_SELFTEST='1'
New-Item -ItemType Directory -Path $env:TEMP,($d+'\results\keycase')|Out-Null
& ($d+'\payload\windows-vm-native.ps1') -Directory $d -ResultsDirectory ($d+'\results\keycase') -ObserveInput
