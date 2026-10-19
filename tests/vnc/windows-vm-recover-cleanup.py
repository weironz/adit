"""Bounded recovery cleanup for the owned VM. Password on stdin only."""
import argparse
import json
from pathlib import Path
import socket
import sys
import time

from pypsrp.client import Client


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--host', required=True)
    parser.add_argument('--user', required=True)
    parser.add_argument('--endpoint', required=True)
    parser.add_argument('--remote-directory', required=True)
    parser.add_argument('--results', type=Path, required=True)
    parser.add_argument('--cleanup-script', type=Path, required=True)
    parser.add_argument('--wait-seconds', type=int, default=14400)
    args = parser.parse_args()
    args.results.mkdir(exist_ok=False)
    password = sys.stdin.readline().rstrip('\r\n')
    if not password:
        raise ValueError('Stdin credential required')
    state = {'outcome': 'waiting_for_vm', 'scope': 'cleanup only; no host power/reset command'}
    deadline = time.monotonic()+args.wait_seconds

    def save():
        state['updated_utc'] = time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())
        (args.results/'status.json').write_text(json.dumps(state, indent=2)+'\n')

    save()
    while time.monotonic() < deadline:
        try:
            with socket.create_connection((args.host, 5985), timeout=2):
                pass
        except OSError:
            time.sleep(15)
            continue
        try:
            with Client(args.host, username=args.user, password=password, ssl=False,
                        auth='ntlm', encryption='always', connection_timeout=10,
                        read_timeout=90, operation_timeout=60) as client:
                files = ['results/native-ready.json', 'results/native-peer/events.jsonl',
                         'results/input-observer/events.jsonl', 'results/german-layout.json',
                         'results/network-drop-actual.txt', 'results/network-drop-before-fix.txt',
                         'power-native/power-before.json', 'power-native/power-result.json',
                         'power-native/native-ready.json', 'power-native/native-peer/events.jsonl']
                for name in files:
                    destination = args.results/name
                    destination.parent.mkdir(exist_ok=True, parents=True)
                    try:
                        client.fetch(args.remote_directory+'\\'+name.replace('/', '\\'), str(destination), configuration_name=args.endpoint)
                    except Exception:
                        # Missing optional evidence must not postpone cleanup.
                        pass
                remote_script = args.remote_directory+'\\cleanup-owned.ps1'
                client.copy(str(args.cleanup_script.resolve()), remote_script, configuration_name=args.endpoint)
                directory = args.remote_directory.replace("'", "''")
                script = ("Set-ExecutionPolicy -Scope Process Bypass -Force;$ErrorActionPreference='Stop';"
                          f"$d='{directory}';$b=Get-Content ($d+'\\bootstrap.json') -Raw|ConvertFrom-Json;"
                          "$a=New-ScheduledTaskAction -Execute powershell.exe -Argument ('-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File '+$d+'\\cleanup-owned.ps1 -Directory '+$d);"
                          "$p=New-ScheduledTaskPrincipal -UserId SYSTEM -LogonType ServiceAccount -RunLevel Highest;"
                          "Register-ScheduledTask -TaskName ($b.name+'-cleanup') -Action $a -Principal $p -Trigger (New-ScheduledTaskTrigger -Once -At (Get-Date).AddSeconds(5)) -Settings (New-ScheduledTaskSettingsSet -StartWhenAvailable)|Out-Null")
                _, streams, failed = client.execute_ps(script, configuration_name=args.endpoint)
                if failed:
                    raise RuntimeError('Cleanup dispatch failed')
                state['outcome'] = 'cleanup_dispatched_unverified'
                save()
                return
        except Exception as error:
            state['last_error_type'] = type(error).__name__
            save()
            time.sleep(15)
    state['outcome'] = 'timeout_vm_unreachable_cleanup_not_completed'
    save()


if __name__ == '__main__':
    main()
