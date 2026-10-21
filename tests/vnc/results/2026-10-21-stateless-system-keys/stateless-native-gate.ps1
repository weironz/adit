$ErrorActionPreference='Stop'
$d='C:\ProgramData\AditVncKeys-2aa748064b34498fb59db43a6805bdde'
$exe=$d+'\payload\native-layout-tests\adit_ui-33c7abe516a403ab.exe'
$env:ADIT_VNC_ISOLATED_LAYOUT_TEST='1'
$i=New-Object Diagnostics.ProcessStartInfo;$i.FileName=$exe;$i.Arguments='native_owned_layout_ignores_pending_accent_without_consuming_it --ignored --nocapture';$i.UseShellExecute=$false;$i.CreateNoWindow=$true;$i.RedirectStandardOutput=$true;$i.RedirectStandardError=$true
$p=New-Object Diagnostics.Process;$p.StartInfo=$i;[void]$p.Start();$out=$p.StandardOutput.ReadToEndAsync();$err=$p.StandardError.ReadToEndAsync();if(!$p.WaitForExit(15000)){$p.Kill();throw 'Native isolation test timeout'}
$utf8=New-Object Text.UTF8Encoding($false)
[IO.File]::WriteAllText(($d+'\results\stateless-dead-stdout.txt'),$out.Result,$utf8);[IO.File]::WriteAllText(($d+'\results\stateless-dead-stderr.txt'),$err.Result,$utf8)
@{exit_code=$p.ExitCode;executable_sha256=(Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash;utc=[datetime]::UtcNow.ToString('o')}|ConvertTo-Json|Set-Content -LiteralPath ($d+'\results\stateless-dead-test.json') -Encoding UTF8
