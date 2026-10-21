"""Independently verify real Entry consumption and retain the rejected fixture run."""
import hashlib
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parents[3]
checks = []


def check(name, condition):
    if not condition:
        raise AssertionError(name)
    checks.append(name)


def load(path):
    return json.loads((ROOT / path).read_text(encoding='utf-8-sig'))


def scalars(text):
    # Tcl can expose a valid UTF-16 pair. Reject unpaired surrogates rather
    # than discarding them or replacing them with a guessed character.
    normalized = text.encode('utf-16-le', 'surrogatepass').decode('utf-16-le')
    return [f'U+{ord(c):04X}' for c in normalized]


cases = {'ascii': 'abc', 'bmp-cjk': '中文', 'nonbmp-emoji': '😊', 'combining': 'e\u0301'}
servers = [('x11vnc', 15902, 99, '0.9.16'), ('tigervnc', 15903, 100, '1.13.1'),
           ('turbovnc', 15904, 101, '3.3.1')]
second = load('second/result.json')
check('complete second run', second['outcome'] == 'passed' and len(second['servers']) == 3)
check('no clipboard path', not second['clipboard_send_used'])
for i, (name, port, display, version) in enumerate(servers):
    prefix = f'second/server-{i}/'
    server = load(prefix + 'result.json')
    check(name + ' identity', (server['server'], server['host_port'], server['display']) == (name, port, display))
    check(name + ' loopback publication', server['published'] == [f'127.0.0.1:{port}'])
    check(name + ' version', version in server['version']['stdout'] + server['version']['stderr'])
    check(name + ' target cleanup', server['cleanup_verified'] and server['cleanup']['quit'])
    check(name + ' all cases retained', len(server['cases']) == 4)
    for tag, expected in cases.items():
        result = load(prefix + tag + '/result.json')
        before, after = result['before'], result['after']
        label = name + '/' + tag
        check(label + ' exact target model scalars', scalars(after['text']) == scalars(expected) == after['scalars'])
        check(label + ' initially empty', before['text'] == '' and before['scalars'] == [])
        check(label + ' same actual application', before['pid'] == after['pid'] == server['target_identity']['pid'])
        for state in [before, after]:
            check(label + ' actual Entry X11 focus', state['focus']['x_input_focus'] == state['entry_xid']
                  and state['focus']['x_focus_owned'] and state['focus']['tk_entry_focused'])
        check(label + ' all input stayed focused', after['counts']['unfocused_input'] == 0
              and after['counts']['focus_losses'] == 0 and all(e['focused'] for e in after['events']))
        check(label + ' actual press and release counts', after['counts']['press'] == after['counts']['release'] == len(expected))
        check(label + ' no truncation', after['counts']['truncated_events'] == 0)
        check(label + ' production TEXT only', result['commands'] == ['TEXT ' + expected, 'WAIT 1500', 'CLOSE']
              and not result['clipboard_send_used'])
        check(label + ' driver completed', result['driver_exit'] == 0)
        check(label + ' recorded checks consistent', all(result['checks'].values()))

first = load('first/result.json')
check('first rejected run retained', first['outcome'] == 'failed' and len(first['servers']) == 3)
for i in range(3):
    for tag in cases:
        result = load(f'first/server-{i}/{tag}/result.json')
        check(f'first/{i}/{tag} failed before typing', result['outcome'] == 'failed'
              and result['error'] == 'target not empty/focused; no TEXT sent' and 'commands' not in result)
        check(f'first/{i}/{tag} independent focus failure preserved',
              result['before']['focus']['x_input_focus'] == 1
              and not result['before']['focus']['x_focus_owned'])

provenance = load('provenance.json')
check('actual driver hashes match source build', first['driver_sha256'] == second['driver_sha256'] == provenance['driver_sha256'])
for file, digest in provenance['production_source_sha256_lf'].items():
    source = subprocess.check_output(['git', 'show', provenance['production_source_commit'] + ':' + file], cwd=REPO)
    check('frozen source ' + file, hashlib.sha256(source.replace(b'\r\n', b'\n')).hexdigest() == digest)
for file, field in [('run_unicode_text_matrix.py', 'runner_sha256_lf'), ('unicode_text_target.py', 'target_sha256_lf')]:
    source = (REPO / 'tests/vnc' / file).read_bytes().replace(b'\r\n', b'\n')
    check('fixture source ' + file, hashlib.sha256(source).hexdigest() == provenance[field])
cleanup = load('cleanup.json')
check('owned container and published listeners removed', cleanup['container_absent'] and not cleanup['listeners'])
for line in (ROOT / 'SHA256SUMS').read_text().splitlines():
    digest, file = line.split('  ', 1)
    data = (ROOT / file).read_bytes().replace(b'\r\n', b'\n')
    check('archive ' + file, hashlib.sha256(data).hexdigest() == digest)
print(json.dumps({'outcome': 'passed', 'checks': len(checks), 'actual_consumption_cases': 12,
                  'retained_preinput_fixture_failures': 12, 'clipboard_send_used': False}))
