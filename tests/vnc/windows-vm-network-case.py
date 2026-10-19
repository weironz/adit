"""Actual Windows firewall packet loss against a private synthetic no-Fence peer.

Credential arrives on stdin. The VM bootstrap endpoint must already be owned by
this test. Never run against a production desktop or an existing evidence folder.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import time

from pypsrp.client import Client


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--host', required=True)
    parser.add_argument('--user', required=True)
    parser.add_argument('--endpoint', required=True)
    parser.add_argument('--probe', type=Path, required=True)
    parser.add_argument('--results', type=Path, required=True)
    parser.add_argument('--remote-directory', required=True)
    args = parser.parse_args()
    args.results.mkdir(exist_ok=False)
    markers = args.results/'markers'
    markers.mkdir()
    password = sys.stdin.readline().rstrip('\r\n')
    client = Client(args.host, username=args.user, password=password, ssl=False,
                    auth='ntlm', encryption='always', connection_timeout=10,
                    read_timeout=90, operation_timeout=60)
    records = []
    process = None
    started = time.monotonic()
    result = {'outcome': 'failed', 'scope': 'Windows client TCP keepalive; actual Windows VM firewall packet loss; synthetic VncAuth/ZRLE server without Fence',
              'probe_sha256': hashlib.sha256(args.probe.read_bytes()).hexdigest()}
    remote = args.remote_directory.replace("'", "''")

    def ps(script):
        # A firewall transition can retire an HTTP authentication context. Use a
        # fresh encrypted connection per operation; never replay a mutation.
        with Client(args.host, username=args.user, password=password, ssl=False,
                    auth='ntlm', encryption='always', connection_timeout=10,
                    read_timeout=90, operation_timeout=60) as connection:
            output, streams, failed = connection.execute_ps(
                "Set-ExecutionPolicy -Scope Process Bypass -Force;$ErrorActionPreference='Stop';"
                f"$d='{remote}';"+script, configuration_name=args.endpoint)
        record = dict(elapsed_ms=round((time.monotonic()-started)*1000), output=output,
                      errors=[str(e) for e in streams.error], failed=failed)
        records.append(record)
        assert not failed, record
        return output

    def wait_marker(name, seconds):
        deadline = time.monotonic()+seconds
        while not (markers/name).exists():
            assert process.poll() is None, f'Probe exited before {name}'
            assert time.monotonic() < deadline, f'Deadline waiting for {name}'
            time.sleep(.05)
        records.append(dict(elapsed_ms=round((time.monotonic()-started)*1000), marker=name))

    try:
        ps("$profiles=@(Get-NetFirewallProfile|Select Name,@{n='Enabled';e={$_.Enabled -eq 1}});$profiles|ConvertTo-Json|Set-Content ($d+'\\results\\original-firewall.json') -Encoding UTF8;"
           "$restore=($profiles|ForEach-Object{'Set-NetFirewallProfile -Name '+$_.Name+' -Enabled '+[string][bool]$_.Enabled}) -join ';';"
           "$a=New-ScheduledTaskAction -Execute powershell.exe -Argument ('-NoProfile -Command '+$restore);"
           "$p=New-ScheduledTaskPrincipal -UserId SYSTEM -LogonType ServiceAccount -RunLevel Highest;"
           "Register-ScheduledTask -TaskName AditVncAcceptance-PolicyGuard-20261019 -Action $a -Principal $p -Trigger (New-ScheduledTaskTrigger -Once -At (Get-Date).AddMinutes(4))|Out-Null;"
           "New-NetFirewallRule -Name AditVncAcceptance-RdpGuard-20261019 -DisplayName AditVncAcceptance-RdpGuard-20261019 -Direction Inbound -Action Allow -LocalPort 3389 -Protocol TCP -RemoteAddress 192.168.72.10 -Profile Any|Out-Null;"
           "Set-NetFirewallProfile -Name Private,Public -Enabled True;"
           "'red'|Set-Content ($d+'\\results\\network-peer-2\\color.txt') -Encoding ASCII;"
           "Get-NetFirewallProfile|Select Name,Enabled|ConvertTo-Json -Compress")
        with (args.results/'client.stdout.txt').open('w') as out, (args.results/'client.stderr.txt').open('w') as err:
            process = subprocess.Popen([str(args.probe.resolve()), args.host, '15916', str(markers.resolve()), '--no-fence'],
                                       stdin=subprocess.PIPE, stdout=out, stderr=err, text=True)
            process.stdin.write('adit-ci\n')
            process.stdin.close()
            wait_marker('ready', 25)
            idle = time.monotonic()
            while time.monotonic()-idle < 35:
                assert process.poll() is None and not (markers/'disconnected').exists()
                time.sleep(.1)
            result['static_idle_seconds'] = time.monotonic()-idle
            ps("pktmon filter add AditVncAcceptance15916 -p 15916;"
               "pktmon start --capture --pkt-size 96 --file-name ($d+'\\results\\network-drop-actual.etl') --log-mode circular --file-size 16;"
               "New-NetFirewallRule -Name AditVncAcceptance-DropIn-20261019 -DisplayName AditVncAcceptance-DropIn-20261019 -Direction Inbound -Action Block -Protocol TCP -LocalPort 15916 -RemoteAddress 192.168.72.10 -Profile Any|Out-Null;"
               "New-NetFirewallRule -Name AditVncAcceptance-DropOut-20261019 -DisplayName AditVncAcceptance-DropOut-20261019 -Direction Outbound -Action Block -Protocol TCP -LocalPort 15916 -RemoteAddress 192.168.72.10 -Profile Any|Out-Null;"
               "Get-NetTCPConnection -LocalPort 15916 -State Established|Select State,OwningProcess|ConvertTo-Json -Compress")
            wait_marker('disconnected', 105)
            ps("'blue'|Set-Content ($d+'\\results\\network-peer-2\\color.txt') -Encoding ASCII;"
               "Remove-NetFirewallRule -Name AditVncAcceptance-DropIn-20261019,AditVncAcceptance-DropOut-20261019")
            wait_marker('recovered', 30)
            ps("'green'|Set-Content ($d+'\\results\\network-peer-2\\color.txt') -Encoding ASCII")
            wait_marker('resumed', 15)
            assert process.wait(timeout=10) == 0
            result['outcome'] = 'passed'
    except Exception as error:
        result['error'] = repr(error)
        raise
    finally:
        if process and process.poll() is None:
            process.kill()
            process.wait(timeout=5)
        try:
            ps("Get-NetFirewallRule -Name AditVncAcceptance-DropIn-20261019,AditVncAcceptance-DropOut-20261019 -ErrorAction SilentlyContinue|Remove-NetFirewallRule;"
               "if(Test-Path ($d+'\\results\\original-firewall.json')){$original=Get-Content ($d+'\\results\\original-firewall.json') -Raw|ConvertFrom-Json;foreach($item in $original){if($item.Enabled -eq $false){Set-NetFirewallProfile -Name $item.Name -Enabled False}else{Set-NetFirewallProfile -Name $item.Name -Enabled True}}};"
               "Get-NetFirewallRule -Name AditVncAcceptance-RdpGuard-20261019 -ErrorAction SilentlyContinue|Remove-NetFirewallRule;"
               "Unregister-ScheduledTask -TaskName AditVncAcceptance-PolicyGuard-20261019 -Confirm:$false -ErrorAction SilentlyContinue;"
               "pktmon counters;pktmon stop;pktmon etl2txt ($d+'\\results\\network-drop-actual.etl') --out ($d+'\\results\\network-drop-actual.txt');pktmon filter remove AditVncAcceptance15916;"
               "Start-Sleep 1;$before=Get-Content ($d+'\\results\\original-firewall.json') -Raw|ConvertFrom-Json;$after=@(Get-NetFirewallProfile|Select Name,@{n='Enabled';e={$_.Enabled -eq 1}});"
               "foreach($profile in $before){$match=@($after|Where-Object Name -eq $profile.Name);if($match.Count -ne 1 -or $match[0].Enabled -ne $profile.Enabled){throw 'Firewall profile was not restored'}};$after|ConvertTo-Json -Compress")
            result['firewall_restored'] = True
        except Exception as error:
            result['outcome'] = 'failed'
            result['cleanup_error'] = repr(error)
            raise
        finally:
            client.close()
            (args.results/'remote.json').write_text(json.dumps(records, indent=2)+'\n', encoding='utf-8')
            (args.results/'result.json').write_text(json.dumps(result, indent=2)+'\n', encoding='utf-8')


if __name__ == '__main__':
    main()
