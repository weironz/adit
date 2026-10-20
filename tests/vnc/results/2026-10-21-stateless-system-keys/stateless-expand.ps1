$d='C:\ProgramData\AditVncKeys-2aa748064b34498fb59db43a6805bdde'
Expand-Archive -LiteralPath ($d+'\payload\stateless-tests.zip') -DestinationPath ($d+'\payload\native-layout-tests') -Force
& ($d+'\stateless-native-gate.ps1')
