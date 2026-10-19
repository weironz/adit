"""Read-only audit: real direct TCP loss, recovery, and exact scoped cleanup."""
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent
RUN = '39253156937'


def read(path):
    return json.loads((ROOT/path).read_text(encoding='utf-8-sig'))


def main():
    for name, expected in read('manifest.json').items():
        assert hashlib.sha256((ROOT/name).read_bytes()).hexdigest() == expected, name
    for failed in ['39250884698', '39252193394', '39252709893']:
        assert read(f'{failed}/server/result.json')['outcome'] == 'failed'
        assert read(f'{failed}/server/cleanup.json')['owned_chain_absent']
        assert read(f'{failed}/server/independent-rules-audit.json')['filter_rule_semantics_unchanged']
    result = read(f'{RUN}/client/result.json')
    assert result['exit_code'] == 0
    assert all(result[key] for key in ['ready','disconnected','recovered','resumed'])
    assert read(f'{RUN}/client/preflight.json') == dict(zero_challenge_verified=True, credential_sent=False)
    stdout = (ROOT/RUN/'client/client.stdout.txt').read_text(encoding='utf-8-sig')
    assert 'recovered_policy=VncAuth,ZRLE,readonly; session_count=1' in stdout
    assert 'fence_rtt_ms: None' in stdout
    assert not (ROOT/RUN/'client/client.stderr.txt').read_text(encoding='utf-8-sig').strip()
    provenance = read(f'{RUN}/client/provenance.json')
    assert provenance['source_sha'] == '1c6eac8a408f3ac9390178f97342b62bc1ff5ea7'
    assert len(provenance['probe_sha256']) == 64
    for direction, lines in read(f'{RUN}/server/drop-counters.json').items():
        assert direction in ['INPUT','OUTPUT'] and len(lines) == 1
        assert int(lines[0].split()[0]) > 0, direction
    assert read(f'{RUN}/server/result.json')['outcome'] == 'passed'
    cleanup = read(f'{RUN}/server/cleanup.json')
    assert all(cleanup[key] for key in ['owned_chain_absent','original_filter_rules_unchanged',
                                       'original_nat_rules_unchanged','original_native_input_rules_unchanged'])
    events = read(f'{RUN}/server/events.json')
    assert [e['event'] for e in events] == ['red_frame','packet_blackhole_started',
        'packet_blackhole_removed','blue_frame','green_frame']
    assert events[1]['elapsed_ms'] - events[0]['elapsed_ms'] >= 40000
    assert events[2]['elapsed_ms'] - events[1]['elapsed_ms'] >= 45000
    peer = [json.loads(line) for line in (ROOT/RUN/'server/peer/events.jsonl').read_text().splitlines()]
    assert not any(e['event'] == 'error' for e in peer)
    assert [e['rgb'] for e in peer if e['event'] == 'frame'] == [[255,0,0],[0,0,255],[0,255,0]]
    assert len([e for e in peer if e['event'] == 'accepted']) == 3
    processes = read(f'{RUN}/server/independent-process-audit.json')
    assert all(processes[key] for key in ['server_pid_absent','controller_unit_inactive','cleanup_timer_inactive'])
    for name, expected in processes['source_files_sha256'].items():
        assert hashlib.sha256((ROOT/RUN/'sources'/name).read_bytes()).hexdigest() == expected
    # The isolated server was terminated after the client exited. Do not invent
    # a graceful peer shutdown event; all threads exited with their process.
    assert read(f'{RUN}/run.json')['conclusion'] == 'success'
    print(json.dumps(dict(outcome='passed', direct_windows_tcp=True, real_os_packet_drop=True,
        same_session_full_frames=True, cleanup_verified=True, physical_nic_switch=False, os_sleep=False)))


if __name__ == '__main__':
    main()
