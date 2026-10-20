"""Audit actual native IME/AltGr evidence, including the failed predecessor."""
import hashlib
import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parents[3]
checks = []


def check(name, condition):
    if not condition:
        raise AssertionError(name)
    checks.append(name)


def load(name):
    return json.loads((ROOT / name).read_text(encoding='utf-8-sig'))


def lines(name):
    return [json.loads(s) for s in (ROOT / name).read_text(encoding='utf-8-sig').splitlines() if s]


def keys(prefix):
    return [(e['down'], e['keysym']) for e in lines(prefix + '-peer.jsonl') if e['event'] == 'key']


def pair(symbol):
    return [(True, symbol), (False, symbol)]


WL, WR, SL, SR, CL, CR, AL, ALTGR, ENTER, TAB = (
    0xffeb, 0xffec, 0xffe1, 0xffe2, 0xffe3, 0xffe4, 0xffe9, 0xfe03, 0xff0d, 0xff09)
CJK = pair(0x01004e2d) + pair(0x01006587)
WIN_R = [(True, WL), (True, 114), (False, 114), (False, WL)]


def chord(symbol):
    return [(True, WL)] + pair(symbol) + [(False, WL)]


def altgr(symbol):
    return [(True, ALTGR)] + pair(symbol) + [(False, ALTGR)]


def exact(prefix, expected):
    actual = keys(prefix)
    check(prefix + ' cumulative wire sequence', actual == expected)
    held = set()
    for down, symbol in actual:
        if down:
            held.add(symbol)
        else:
            check(prefix + ' release owned', symbol in held)
            held.remove(symbol)
    check(prefix + ' no held keys', not held)


def ime(prefix, grab):
    events = lines(prefix + '-app.jsonl')
    compositions = [e for e in events if e['event'] == 'native_ime_event'
                    and e['kind'] == 'preedit' and e['scalars'] > 0 and e['grab'] == grab]
    check(prefix + ' actual preedit', bool(compositions))
    check(prefix + ' matching input epoch', all(e['epoch'] == e['current_epoch']
          and e['accepts_text'] for e in compositions))


exact('before-off-preedit', [])
exact('before-off-commit', CJK)
# An early toolbar positioning click hit Send Ctrl+Alt+Del on the synthetic peer.
# Preserve those six public fixture events rather than silently trimming them.
cad = [(True, CL), (True, AL)] + pair(0xffff) + [(False, AL), (False, CL)]
before = CJK + cad
exact('before-grab-on', before)
before += WIN_R
exact('before-on-win-r', before)
exact('before-on-preedit', before)
before += CJK
exact('before-on-commit', before)
exact('before-rearm-preedit', before)
before += CJK
exact('before-rearm-commit', before)
before += chord(122) + chord(121) + altgr(64) + altgr(0x010020ac)
exact('before-german-chords', before)
before += pair(233)
exact('before-candidate1-final', before)
check('failed predecessor retained', len(before) == 40)
ime('before-off-preedit', False)
ime('before-on-preedit', True)
ime('before-rearm-preedit', True)
failed = lines('before-german-chords-app.jsonl')
check('predecessor actually disarmed after AltGr',
      not [e for e in failed if e['event'] == 'hook_diagnostics'][-1]['armed'])

final = altgr(64) + altgr(0x010020ac) + WIN_R
exact('final-altgr', final)
final += pair(233)
exact('final-accent', final)
final += pair(WL)
exact('final-pending-dead', final)
check('pending dead key unsupported explicitly',
      'translation unavailable: multiple-scalars' in (ROOT / 'final-stderr.txt').read_text())
# Ordinary window character release can follow a later captured Win-down;
# modifier fences do not promise global ordering of all ordinary text.
final += [(True, 233), (True, WL), (False, 233)] + pair(114) + [(False, WL)]
final += chord(122) + chord(121)
exact('final-dead-recovery', final)
final += [(True, CL)] + pair(97) + [(False, CL)] + WIN_R
final += [(True, CR)] + pair(97) + [(False, CR)] + WIN_R
final += [(True, WL)] + altgr(64) + [(False, WL)]
exact('final-real-ctrl-altgr', final)
final += [(True, CL), (True, AL), (False, CL), (False, AL)]
exact('final-pinyin-grab', final)
exact('final-pinyin-preedit', final)
ime('final-pinyin-preedit', True)
final += CJK
exact('final-pinyin-commit', final)
final += [(True, AL)] + pair(TAB) + [(False, AL)] + pair(WR)
final += [(True, SL), (True, WL)] + pair(82) + [(False, WL), (False, SL)]
final += [(True, WL), (True, SR)] + pair(82) + [(False, SR), (False, WL)]
final += [(True, WL), (True, ENTER), (True, CL), (True, AL),
          (False, ENTER), (False, AL), (False, CL), (False, WL)]
final += [(True, WL), (True, CL), (True, AL), (False, WL), (False, CL), (False, AL)]
exact('final-core-after-ime', final)
exact('final-final', final)
check('final event count', len(final) == 92)

for prefix in ['before-grab-on', 'before-on-preedit', 'before-rearm-preedit',
               'final-grab', 'final-altgr', 'final-accent', 'final-pending-dead',
               'final-real-ctrl-altgr', 'final-pinyin-grab', 'final-pinyin-preedit']:
    events = lines(prefix + '-app.jsonl')
    state = [e['state'] for e in events if e['event'] == 'native_ime_state'][-1]
    check(prefix + ' same native thread and foreground',
          state['same_thread'] and state['foreground'] and state['accepts_text'] and state['grab'])
    check(prefix + ' armed', [e for e in events if e['event'] == 'hook_diagnostics'][-1]['armed'])
    if 'pinyin' in prefix or prefix.startswith('before-'):
        check(prefix + ' Chinese IME actually open', state['open'] and state['conversion'] & 1)
    if prefix == 'final-altgr':
        check('wait exceeds old timeout', events[-1]['unix_ms'] -
              [e for e in lines(prefix + '-peer.jsonl') if e['event'] == 'key'][-1]['unix_ms'] > 3000)

for prefix in ['before-focus-away', 'before-refocus']:
    state = [e['state'] for e in lines(prefix + '-app.jsonl') if e['event'] == 'native_ime_state'][-1]
    check(prefix + ' capture retired', not state['grab'])
check('escape disarmed final source', not [e for e in lines('final-final-app.jsonl')
      if e['event'] == 'hook_diagnostics'][-1]['armed'])
observed = lines('final-altgr-observer.jsonl')
aux = [e for e in observed if e['vk'] == 0xa2 and e['scan'] == 0x21d]
check('actual Windows auxiliary Ctrl retained by independent observer',
      len(aux) == 4 and all(not e['injected'] for e in aux))
check('actual native Win source', any(e['vk'] == 0x5b and not e['injected'] for e in observed))

provenance = load('provenance.json')
check('final target EXE hash', load('final-ready.json')['source_binary_sha256'].lower()
      == provenance['executable_sha256'])
check('predecessor target EXE hash', load('before-ready.json')['source_binary_sha256'].lower()
      == provenance['before_executable_sha256'])
for path, digest in provenance['source_sha256_lf'].items():
    blob = subprocess.check_output(['git', 'show', provenance['tested_commit'] + ':' + path], cwd=REPO)
    check('frozen source ' + path, hashlib.sha256(blob.replace(b'\r\n', b'\n')).hexdigest() == digest)
ci = (ROOT / 'just-ci-vnc.log').read_text(encoding='utf-8-sig')
root_ci = ci.split('cargo clippy -p adit-ui --all-targets --features native-vnc-acceptance')[0]
counts = [tuple(map(int, m)) for m in re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored', root_ci)]
check('local root test counts', [sum(c[i] for c in counts) for i in range(3)] == [428, 0, 11])
check('native feature test counts', '143 passed; 0 failed; 3 ignored' in ci)
check('vendor graphics counts', '202 passed; 0 failed; 0 ignored' in ci)
check('explicit native tests', '3 passed; 0 failed; 0 ignored' in (ROOT / 'native-tests.log').read_text())
for name in ['before-stopped.json', 'final-stopped.json']:
    check(name, load(name)['owned_processes_stopped'])
independent = load('independent-before-cleanup.json')
check('language restored independently', independent['language_restored'])
check('before cleanup processes and listener absent', not independent['owned_processes'] and not independent['listeners'])
cleanup = load('cleanup-final.json')
check('owned resources removed', cleanup['directory_absent'] and cleanup['share_absent']
      and not cleanup['processes'] and not cleanup['tasks'] and not cleanup['listeners'])
check('original language data preserved', cleanup['languages'] == independent['baseline'])
check('WinRM and firewall baseline preserved', cleanup['winrm'] == 'Stopped'
      and [p['Enabled'] for p in cleanup['firewall']] == [1, 0, 0])
control = load('control-stopped.json')
check('owned console and driver exited', control['console_exit_ack'] and control['driver_exit'] == 0)
for record in (ROOT / 'SHA256SUMS').read_text().splitlines():
    digest, path = record.split('  ', 1)
    payload = (ROOT / path).read_bytes()
    if Path(path).suffix in {'.json', '.jsonl', '.md', '.py', '.txt', '.log'}:
        payload = payload.replace(b'\r\n', b'\n')
    check('archive ' + path, hashlib.sha256(payload).hexdigest() == digest)
print(json.dumps({'outcome': 'passed', 'checks': len(checks), 'final_key_events': len(final),
                  'before_key_events': len(before),
                  'known_limit': 'pending dead-key Win character skipped with visible notice; vendor Unicode separate'}, ensure_ascii=False))
