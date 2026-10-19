"""Owned Linux synthetic peer: source-restricted ingress and actual packet loss.

No desktop capture or host credentials. Public fixture value adit-ci. Run only
on an explicitly authorized host; an independent systemd timer removes our rules.
"""
import argparse
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time


def command(argv, check=True):
    return subprocess.run(argv, capture_output=True, text=True, check=check)


def rules_sha256(saved):
    # iptables-save includes timestamps and live policy counters without -c.
    # Compare rule semantics, retaining the original snapshots for inspection.
    rules = '\n'.join(re.sub(r' \[\d+:\d+\]$', '', line)
                      for line in saved.splitlines() if not line.startswith('#'))
    return hashlib.sha256(rules.encode()).hexdigest()


def nft_input(table):
    return json.loads(command(['nft', '-j', 'list', 'chain', 'inet', table, 'input']).stdout)


def nft_sha256(snapshot):
    entries = []
    for entry in snapshot['nftables']:
        if 'metainfo' in entry:
            continue
        for item in entry.values():
            item.pop('handle', None)
            for expr in item.get('expr', []):
                if 'counter' in expr:
                    expr['counter'] = {}
        entries.append(entry)
    return hashlib.sha256(json.dumps(entries, sort_keys=True).encode()).hexdigest()


def cleanup(directory):
    state = json.loads((directory/'owner.json').read_text())
    assert state['directory'] == str(directory) and state['chain'].startswith('ADITVNC')
    for pid in [state.get('server_pid'), state.get('controller_pid')]:
        if pid and pid != os.getpid():
            try:
                cmd = Path(f'/proc/{pid}/cmdline').read_bytes()
                if str(directory).encode() in cmd:
                    os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            except FileNotFoundError:
                pass
    for entry in reversed(state['rules']):
        table, chain, rule = entry if len(entry) == 3 else ('filter', *entry)
        command(['iptables', '-w', '5', '-t', table, '-D', chain, *rule], check=False)
    if state.get('chain_created'):
        command(['iptables', '-w', '5', '-F', state['chain']], check=False)
        command(['iptables', '-w', '5', '-X', state['chain']], check=False)
    if state.get('nft_table'):
        for item in nft_input(state['nft_table'])['nftables']:
            rule = item.get('rule', {})
            if rule.get('comment') == state['chain']:
                command(['nft', 'delete', 'rule', 'inet', state['nft_table'], 'input', 'handle', str(rule['handle'])])
    after = command(['iptables-save', '-t', 'filter']).stdout
    (directory/'after-filter.txt').write_text(after)
    result = {'owned_chain_absent': command(['iptables', '-w', '5', '-S', state['chain']], check=False).returncode != 0,
              'filter_rules_sha256_before': state['filter_sha256'],
              'filter_rules_sha256_after': rules_sha256(after)}
    result['original_filter_rules_unchanged'] = result['filter_rules_sha256_before'] == result['filter_rules_sha256_after']
    if 'nat_sha256' in state:
        nat = command(['iptables-save', '-t', 'nat']).stdout
        (directory/'after-nat.txt').write_text(nat)
        result['original_nat_rules_unchanged'] = state['nat_sha256'] == rules_sha256(nat)
    if state.get('nft_table'):
        result['original_native_input_rules_unchanged'] = state['nft_sha256'] == nft_sha256(nft_input(state['nft_table']))
    (directory/'cleanup.json').write_text(json.dumps(result, indent=2)+'\n')


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--directory', type=Path, required=True)
    p.add_argument('--client-ip')
    p.add_argument('--bind')
    p.add_argument('--run-id')
    p.add_argument('--port', type=int, default=15916)
    p.add_argument('--nft-input-table')
    p.add_argument('--cleanup-only', action='store_true')
    args = p.parse_args()
    directory = args.directory.resolve()
    assert directory.parent == Path('/tmp') and directory.name.startswith('adit-vnc-direct-')
    if args.cleanup_only:
        cleanup(directory)
        return
    assert os.geteuid() == 0 and args.run_id.isdigit()
    assert 1 <= args.port <= 65535
    port = str(args.port)
    assert args.nft_input_table is None or re.fullmatch(r'[A-Za-z0-9_]+', args.nft_input_table)
    client = str(ipaddress.IPv4Address(args.client_ip))
    bind = str(ipaddress.IPv4Address(args.bind))
    chain = 'ADITVNC'+args.run_id
    assert len(chain) <= 28 and not (directory/'owner.json').exists()
    before = command(['iptables-save', '-t', 'filter']).stdout
    (directory/'before-filter.txt').write_text(before)
    nat = command(['iptables-save', '-t', 'nat']).stdout
    (directory/'before-nat.txt').write_text(nat)
    state = dict(directory=str(directory), chain=chain, chain_created=False, rules=[], controller_pid=os.getpid(),
                 filter_sha256=rules_sha256(before), nat_sha256=rules_sha256(nat))
    if args.nft_input_table:
        state.update(nft_table=args.nft_input_table, nft_sha256=nft_sha256(nft_input(args.nft_input_table)))
    result = dict(outcome='failed', scope='Direct hosted Windows client to Linux TCP; no application proxy; Linux INPUT/OUTPUT packet drop', client_ip=client)
    server = None
    timer = 'adit-vnc-cleanup-'+args.run_id
    started = time.monotonic()
    events = []

    def emit(event, **fields):
        events.append(dict(elapsed_ms=round((time.monotonic()-started)*1000), event=event, **fields))

    def save():
        (directory/'owner.json').write_text(json.dumps(state, indent=2)+'\n')

    def insert(target, rule, table='filter'):
        command(['iptables', '-w', '5', '-t', table, '-I', target, '1', *rule])
        state['rules'].append((table, target, rule))
        save()

    def peer_events():
        path = directory/'peer'/'events.jsonl'
        if not path.exists():
            return []
        return [json.loads(line) for line in path.read_text().splitlines() if line]

    def await_event(predicate, seconds):
        end = time.monotonic()+seconds
        while True:
            matches = [e for e in peer_events() if predicate(e)]
            if matches:
                return matches[-1]
            assert time.monotonic() < end, 'Fixture event deadline'
            assert server.poll() is None, 'Synthetic server exited'
            time.sleep(.1)

    save()
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    try:
        command(['iptables', '-w', '5', '-N', chain])
        state['chain_created'] = True
        save()
        command(['iptables', '-w', '5', '-A', chain, '-s', client, '-j', 'ACCEPT'])
        command(['iptables', '-w', '5', '-A', chain, '-j', 'DROP'])
        jump = ['-p', 'tcp', '--dport', port, '-m', 'comment', '--comment', chain, '-j', chain]
        insert('INPUT', jump)
        insert('OUTPUT', ['-d', client, '-p', 'tcp', '--sport', port, '-m', 'comment', '--comment', chain, '-j', 'ACCEPT'])
        command(['systemd-run', '--quiet', '--unit='+timer, '--on-active=15min', '--collect',
                 sys.executable, str(directory/'direct-network-server.py'), '--directory', str(directory), '--cleanup-only'])
        if args.nft_input_table:
            # Hosts may also have a native nft input policy before iptables.
            # Permit only this disposable source/port; no global policy change.
            command(['nft', 'insert', 'rule', 'inet', args.nft_input_table, 'input',
                     'ip', 'saddr', client, 'ip', 'daddr', bind, 'tcp', 'dport', port,
                     'counter', 'accept', 'comment', '"'+chain+'"'])
        with (directory/'peer.stdout.txt').open('w') as out, (directory/'peer.stderr.txt').open('w') as err:
            server = subprocess.Popen([sys.executable, str(directory/'windows-vm-network-peer.py'),
                '--bind', bind, '--port', port, '--directory', str(directory/'peer'), '--public-fixture'], stdout=out, stderr=err)
            state['server_pid'] = server.pid
            save()
            await_event(lambda e: e['event']=='frame' and e['rgb']==[255, 0, 0], 600)
            emit('red_frame')
            time.sleep(40)
            drops = [('INPUT', ['-s', client, '-p', 'tcp', '--dport', port, '-m', 'comment', '--comment', chain+'DROP', '-j', 'DROP']),
                     ('OUTPUT', ['-d', client, '-p', 'tcp', '--sport', port, '-m', 'comment', '--comment', chain+'DROP', '-j', 'DROP'])]
            for target, rule in drops:
                insert(target, rule)
            emit('packet_blackhole_started')
            time.sleep(45)
            counters = {table: [line for line in command(['iptables', '-w', '5', '-nvxL', table]).stdout.splitlines() if chain+'DROP' in line] for table in ['INPUT','OUTPUT']}
            (directory/'drop-counters.json').write_text(json.dumps(counters, indent=2)+'\n')
            (directory/'peer'/'color.txt').write_text('blue')
            for target, rule in drops:
                command(['iptables', '-w', '5', '-D', target, *rule])
            emit('packet_blackhole_removed')
            await_event(lambda e: e['event']=='frame' and e['rgb']==[0, 0, 255], 90)
            emit('blue_frame')
            time.sleep(3)
            (directory/'peer'/'color.txt').write_text('green')
            await_event(lambda e: e['event']=='frame' and e['rgb']==[0, 255, 0], 10)
            emit('green_frame')
            time.sleep(2)
            result['outcome'] = 'passed'
    finally:
        (directory/'events.json').write_text(json.dumps(events, indent=2)+'\n')
        (directory/'result.json').write_text(json.dumps(result, indent=2)+'\n')
        if server and server.poll() is None:
            server.terminate()
            server.wait(timeout=5)
        # We are still this controller; cleanup must not terminate it recursively.
        state['controller_pid'] = None
        save()
        cleanup(directory)
        command(['systemctl', 'stop', timer+'.timer'], check=False)


if __name__ == '__main__':
    main()
