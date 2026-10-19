# Cleanup client only. UAC rendering is measured separately by Adit's probe.
$ErrorActionPreference='Stop'
if ($env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows') {throw 'Disposable hosted Windows only'}
$client=[Net.Sockets.TcpClient]::new()
function Read-Exact([int]$Length) {
 $buffer=[byte[]]::new($Length);$offset=0
 while($offset -lt $Length) {$read=$stream.Read($buffer,$offset,$Length-$offset);if($read -eq 0){throw 'RFB cleanup EOF'};$offset+=$read}
 return ,$buffer
}
function Send-Bytes([byte[]]$Data) {$stream.Write($Data,0,$Data.Length)}
try {
 $client.ReceiveTimeout=5000;$client.SendTimeout=5000;$client.Connect('127.0.0.1',15911)
 $stream=$client.GetStream()
 $version=Read-Exact 12
 if([Text.Encoding]::ASCII.GetString($version) -ne "RFB 003.008`n"){throw 'Expected RFB3.8 cleanup peer'}
 Send-Bytes $version
 $count=(Read-Exact 1)[0];if($count -lt 1 -or $count -gt 32){throw 'Invalid RFB security count'}
 $types=Read-Exact $count;if(2 -notin $types){throw 'Cleanup requires VncAuth; never fall back to None'}
 Send-Bytes ([byte[]]@(2))
 $challenge=Read-Exact 16
 $plain=[byte[]]::new(8);[Text.Encoding]::ASCII.GetBytes('adit-ci').CopyTo($plain,0)
 $key=[byte[]]::new(8)
 for($i=0;$i -lt 8;$i++){for($bit=0;$bit -lt 8;$bit++){$key[$i]=$key[$i] -bor ((($plain[$i] -shr $bit) -band 1) -shl (7-$bit))}}
 $des=[Security.Cryptography.DES]::Create()
 try {$des.Key=$key;$des.Mode='ECB';$des.Padding='None';$transform=$des.CreateEncryptor();try{$response=$transform.TransformFinalBlock($challenge,0,16)}finally{$transform.Dispose()}}finally{$des.Dispose();[Array]::Clear($key);[Array]::Clear($plain)}
 Send-Bytes $response
 if(@((Read-Exact 4)|Where-Object{$_ -ne 0}).Count){throw 'Cleanup authentication rejected'}
 Send-Bytes ([byte[]]@(1))
 $init=Read-Exact 24
 $length=([int]$init[20] -shl 24) -bor ([int]$init[21] -shl 16) -bor ([int]$init[22] -shl 8) -bor [int]$init[23]
 if($length -lt 0 -or $length -gt 1048576){throw 'Cleanup server name exceeded bound'}
 [void](Read-Exact $length)
 Send-Bytes ([byte[]]@(4,1,0,0,0,0,255,27,4,0,0,0,0,0,255,27))
 $stream.Flush()
 Start-Sleep -Milliseconds 250
 Write-Output 'Public fixture VncAuth cleanup Escape sent; not an Adit keyboard acceptance.'
} finally {$client.Dispose()}
