"""Verify the actual foreground matrix, preserved limitations, and owned cleanup."""
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


def load(name, root=ROOT):
    return json.loads((root / name).read_text(encoding='utf-8-sig'))


def lines(name, root=ROOT):
    return [json.loads(s) for s in (root / name).read_text(encoding='utf-8-sig').splitlines() if s]


def keys(tag, root=ROOT):
    return [(e['down'], e['keysym']) for e in lines(tag + '-peer.jsonl', root) if e['event'] == 'key']


def pair(symbol):
    return [(True, symbol), (False, symbol)]


WL, WR, SL, SR, CL, CR, AL, AR, TAB, ENTER = (
    0xffeb, 0xffec, 0xffe1, 0xffe2, 0xffe3, 0xffe4, 0xffe9, 0xffea, 0xff09, 0xff0d)
win_r = [(True, WL), (True, 114), (False, 114), (False, WL)]
right_before = [(True, SR), (True, WL), (True, 82), (False, 82), (False, WL), (False, SR)]
left_after = [(True, WL), (True, SL), (True, 82), (False, 82), (False, SL), (False, WL)]
left_before = [(True, SL), (True, WL), (True, 82), (False, 82), (False, WL), (False, SL)]
rdp_repeat = [(True, WL), (True, 114), (False, 114), (True, 114), (False, WL), (False, 114)]
basics = [(True, AL), (True, TAB), (False, TAB), (False, AL)] + pair(WL) + pair(WR)
enter = [(True, WL), (True, ENTER), (True, CL), (True, AL),
         (False, ENTER), (False, AL), (False, CL), (False, WL)]
core = win_r + left_after + left_before + rdp_repeat + basics + enter + enter
right_after = [(True, WL), (True, SR), (True, 82), (False, 82), (False, SR), (False, WL)]
right = right_before + right_after + pair(CR) + pair(AR)
word = [edge for char in 'zhongwen' for edge in pair(ord(char))]
escape = [(True, WL), (True, CL), (True, AL), (False, WL), (False, CL), (False, AL)]

expected = []
phases = [('grab', []), ('right-shift-diagnostic', right_before), ('core-matrix', core),
          ('right-modifiers', right), ('shift-toggle', pair(SL) + win_r),
          ('cjk-preedit', pair(SL) + word), ('cjk-mode-preedit', word),
          ('escape-held-win', escape), ('lifecycle', pair(WL) + pair(WL)), ('final', [])]
for tag, delta in phases:
    expected += delta
    actual = keys(tag)
    check(tag + ' exact cumulative wire sequence', actual == expected)
    held = set()
    for down, symbol in actual:
        if down:
            held.add(symbol)
        else:
            check(tag + ' release has owner', symbol in held)
            held.remove(symbol)
    check(tag + ' ends with no held key', not held)
check('final count', len(expected) == 118)

grab = lines('grab-app.jsonl')
dispatcher = [e for e in grab if e['event'] == 'hook_dispatcher'][-1]
check('actual foreground dispatcher', dispatcher['dispatcher_thread'] == dispatcher['foreground_window_thread'])
check('grab armed', [e for e in grab if e['event'] == 'hook_diagnostics'][-1]['armed'])
for tag in ['right-shift-diagnostic', 'core-matrix', 'right-modifiers', 'shift-toggle']:
    check(tag + ' hook remains armed', [e for e in lines(tag + '-app.jsonl') if e['event'] == 'hook_diagnostics'][-1]['armed'])
check('escape disarms', not [e for e in lines('escape-held-win-app.jsonl') if e['event'] == 'hook_diagnostics'][-1]['armed'])
check('final focus cleanup disarms', not [e for e in lines('final-app.jsonl') if e['event'] == 'hook_diagnostics'][-1]['armed'])
aliases = [e for e in lines('final-app.jsonl') if e['event'] == 'window_shift_alias']
check('actual native window E036 down/up',
      [(e['scan'], e['pressed']) for e in aliases] == [(0xe036, True), (0xe036, False)] * 2)
observed_shift = [e for e in lines('final-observer.jsonl') if e['vk'] == 0xa1]
check('independent right Shift source flags retained', len(observed_shift) == 4 and
      all(e['scan'] == 0x36 and e['flags'] & 1 and not e['injected'] for e in observed_shift))
check('CJK attempts are ASCII not an accepted Chinese commit',
      all(symbol < 0x01000000 for _, symbol in keys('cjk-mode-preedit')))

provenance = load('provenance.json')
check('target executable hash matches source build',
      load('native-ready.json')['source_binary_sha256'].lower() == provenance['executable_sha256'])
for path, expected_hash in provenance['source_sha256_lf'].items():
    blob = subprocess.check_output(['git', 'show', provenance['tested_commit'] + ':' + path], cwd=REPO)
    check('tested source ' + path, hashlib.sha256(blob).hexdigest() == expected_hash)

old1 = ROOT.parent / '2026-10-21-modifier-order-vm-prefix'
old2 = ROOT.parent / '2026-10-21-window-shift-alias-vm-failure'
for label, folder, before in [('hook-only', old1, 'numpad-enter-held'), ('window-alias', old2, 'core-matrix')]:
    check(label + ' failed right Shift retained', keys('right-modifiers', folder)[len(keys(before, folder)):] == pair(CR) + pair(AR))
    check(label + ' timeout retained', not [e for e in lines('right-modifiers-app.jsonl', folder) if e['event'] == 'hook_diagnostics'][-1]['armed'])

cleanup = load('cleanup.json')
check('independent owned executable scan empty', not cleanup['audit']['owned_processes_remaining'])
check('fixture listener absent', not cleanup['audit']['listener_15915'])
check('all OS modifiers released', not cleanup['pressed_modifier_keys'])
check('language configuration retained', cleanup['language_tags'] == ['zh-Hans-CN'])
check('three explicit owned roots', len(cleanup['roots']) == 3)
for root in cleanup['roots']:
    check('owned directory removed ' + root['name'], root['directory_absent'])
    state = root['cleanup']
    check('owned processes shares tasks removed ' + root['name'],
          state['share_absent'] and not state['remaining_processes'] and not state['tasks_remaining'])
    check('WinRM retained ' + root['name'], state['winrm'] == 'Stopped')
    check('firewall retained ' + root['name'],
          {p['Name']: p['Enabled'] for p in state['firewall']} == {'Domain': 1, 'Private': 0, 'Public': 0})
check('control driver exited', provenance['driver_exit'] == 0)

if (ROOT / 'sha256.json').exists():
    for name, digest in load('sha256.json').items():
        # Git checkout may use CRLF on Windows; archived text is canonical LF.
        canonical = (ROOT / name).read_bytes().replace(b'\r\n', b'\n')
        check('archive hash ' + name, hashlib.sha256(canonical).hexdigest() == digest)

print(json.dumps({'outcome': 'passed', 'scope': 'basic foreground matrix and evidence integrity',
                  'tested_commit': provenance['tested_commit'], 'checks': len(checks),
                  'final_key_events': 118, 'capture_mode_cjk': 'not accepted; ASCII preparation attempts retained',
                  'remaining': ['international layout and preceding dead-key state',
                                'capture-mode IME revalidation', 'vendor Unicode consumption',
                                'physical hardware repeat certification']}, indent=2))
