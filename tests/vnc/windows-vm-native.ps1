param([Parameter(Mandatory)][string]$Directory,[string]$ResultsDirectory,[switch]$ObserveInput,[string]$NativeExecutable)
$ErrorActionPreference='Stop'
$Directory=[IO.Path]::GetFullPath($Directory)
$payload=Join-Path $Directory 'payload'
$results=if($ResultsDirectory){$ResultsDirectory}else{Join-Path $Directory 'results'}
$python=Join-Path $payload 'python/python.exe'
$native=if($NativeExecutable){[IO.Path]::GetFullPath($NativeExecutable)}else{Join-Path $payload 'vnc_native_acceptance.exe'}
if(-not $native.StartsWith(([IO.Path]::GetFullPath($payload)+'\'),[StringComparison]::OrdinalIgnoreCase)) { throw 'Native executable must belong to this payload directory' }
$fixture=$null
$app=$null
$observer=$null
try {
    @{session=[Diagnostics.Process]::GetCurrentProcess().SessionId; languages=@(Get-WinUserLanguageList); user=[Security.Principal.WindowsIdentity]::GetCurrent().Name} |
        ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $results 'interactive.json') -Encoding UTF8
    $fixture=Start-Process $python -ArgumentList @((Join-Path $payload 'native_rfb_fixture.py'),'--state-dir',(Join-Path $results 'native-peer')) -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $results 'peer.stdout.txt') -RedirectStandardError (Join-Path $results 'peer.stderr.txt')
    $deadline=(Get-Date).AddSeconds(20)
    while(-not (Test-Path -LiteralPath (Join-Path $results 'native-peer/ready.json'))) {
        if($fixture.HasExited -or (Get-Date) -gt $deadline){throw 'Synthetic peer did not become ready'}
        Start-Sleep -Milliseconds 100
    }
    $app=Start-Process $native -PassThru -RedirectStandardOutput (Join-Path $results 'app.stdout.txt') -RedirectStandardError (Join-Path $results 'app.stderr.txt')
    if($ObserveInput){$observer=Start-Process $python -ArgumentList @((Join-Path $payload 'windows-vm-input-observer.py'),'--directory',(Join-Path $results 'input-observer'),'--pid',$app.Id) -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $results 'observer.stdout.txt') -RedirectStandardError (Join-Path $results 'observer.stderr.txt')}
    @{app=$app.Id;fixture=$fixture.Id;observer=if($observer){$observer.Id}else{$null};session=[Diagnostics.Process]::GetCurrentProcess().SessionId;source_binary_sha256=(Get-FileHash $native -Algorithm SHA256).Hash} |
        ConvertTo-Json | Set-Content -LiteralPath (Join-Path $results 'native-ready.json') -Encoding UTF8
    $deadline=(Get-Date).AddMinutes(35)
    while(-not $app.HasExited -and -not (Test-Path (Join-Path $results 'native-stop.request'))) {
        if((Get-Date) -gt $deadline){throw 'Native acceptance safety deadline'}
        Start-Sleep -Milliseconds 200
    }
} catch {
    $_.Exception.Message | Set-Content -LiteralPath (Join-Path $results 'native-error.txt') -Encoding UTF8
    throw
} finally {
    foreach($process in @($observer,$app,$fixture)) {
        if($process -and -not $process.HasExited){Stop-Process -Id $process.Id -ErrorAction SilentlyContinue}
    }
    @{owned_processes_stopped=$true} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $results 'native-stopped.json') -Encoding UTF8
}
