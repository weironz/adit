"""Read-only evidence audit; rejects incomplete runs and preserves scope limits."""
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent


def read(name):
    return json.loads((ROOT/name).read_text(encoding='utf-8-sig'))


def paired(events, symbol):
    values = [event['down'] for event in events if event.get('keysym') == symbol]
    return bool(values) and len(values) % 2 == 0 and values == [True, False]*(len(values)//2)


def main():
    for name, expected in read('manifest.json').items():
        file = ROOT/name
        assert file.is_file() and hashlib.sha256(file.read_bytes()).hexdigest() == expected, name
    final = read('network-final/result.json')
    assert final['outcome'] == 'passed' and final['firewall_restored'] is True
    assert final['static_idle_seconds'] >= 35
    final_events = read('network-final/remote.json')
    assert not any(event.get('failed') for event in final_events)
    assert [e['marker'] for e in final_events if 'marker' in e] == ['ready','disconnected','recovered','resumed']
    stdout = (ROOT/'network-final/client.stdout.txt').read_text(encoding='utf-8')
    assert 'recovered_policy=VncAuth,ZRLE,readonly; session_count=1' in stdout
    assert 'fence_rtt_ms: None' in stdout
    assert read('network-actual/result.json')['outcome'] == 'failed'
    # The intermediate writer said passed before cleanup failed. Its raw record
    # stays unchanged, and the independent decision must reject that whole run.
    assert any(event.get('failed') for event in read('network-fixed/remote.json'))
    assert 'firewall_restored' not in read('network-fixed/result.json')
    ime = read('native/ime-commit.json')['observation']
    assert [(e['keysym'],e['down']) for e in ime[-4:]] == [(0x1004e2d,True),(0x1004e2d,False),(0x1006587,True),(0x1006587,False)]
    english = read('native/english.json')['observation']
    assert paired(english, ord('a')) and paired(english, ord('A'))
    assert paired(read('native/left-win.json')['observation'],0xffeb)
    modifiers = read('native/modifier-observations.json')['observation']
    # This bounded tail starts with releases whose presses predate the window.
    # Verify each last complete pair, not invented balance over omitted events.
    for code in [0xffe3,0xffe4,0xffe1,0xffe9,0xffea]:
        assert [e['down'] for e in modifiers if e.get('keysym') == code][-2:] == [True, False]
    observer = read('native/os-input-observer.json')
    assert observer and all(not e['injected'] and not e['flags'] & 0x10 for e in observer)
    assert read('transport-scope.json')['direct_tcp_keepalive_proven_by_vm_run'] is False
    assert read('native/layout-hang-context.json')['raw_dumps_published'] is False
    assert read('cleanup-status-snapshot.json')['outcome'] != 'passed'
    assert read('ci-aaec.json')['conclusion'] == 'success'
    assert read('symbol-ci.json')['conclusion'] == 'success'
    print(json.dumps({'outcome':'passed','network_gateway_recovery':True,'ime_commit':'中文',
                      'intermediate_cleanup_failure_rejected':True,'direct_vm_tcp_proof':False,
                      'sleep_recovery_passed':False,'vm_cleanup_verified':False},ensure_ascii=False))


if __name__ == '__main__':
    main()
