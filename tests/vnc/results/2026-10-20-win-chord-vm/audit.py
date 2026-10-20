"""Audit saved VM evidence, including deliberately retained ordering failures."""
import hashlib
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parents[3]
COMMIT = '44f73b94b22f6918fab1e793f6b8915d319e6402'
checks = []


def check(name, condition):
    if not condition:
        raise AssertionError(name)
    checks.append(name)


def load(name):
    return json.loads((ROOT / name).read_text(encoding='utf-8-sig'))


def lines(name):
    return [json.loads(s) for s in (ROOT / name).read_text(encoding='utf-8-sig').splitlines() if s]


def keys(tag):
    return [(e['down'], e['keysym']) for e in lines(tag + '-peer.jsonl') if e['event'] == 'key']


candidate = load('candidate-provenance.json')
check('target executable hash matches frozen candidate',
      load('native-ready.json')['source_binary_sha256'].lower() == candidate['executable_sha256'])
for path, expected in candidate['source_sha256_lf'].items():
    blob = subprocess.check_output(['git', 'show', COMMIT + ':' + path], cwd=REPO)
    check('tested source blob ' + path, hashlib.sha256(blob).hexdigest() == expected)

check('ordinary R has no premature peer key', keys('ime-preflight') == [])
observed = lines('ime-preflight-observer.jsonl')
check('ordinary R is noninjected on actual foreground thread',
      len(observed) == 2 and all(e['scan'] == 0x13 and not e['injected']
                                and e['foreground_thread'] == 7776 for e in observed))
grab = lines('grab-app.jsonl')
check('real grab dispatcher matches foreground thread',
      any(e['event'] == 'hook_dispatcher' and e['dispatcher_thread'] == e['foreground_window_thread'] == 7776 for e in grab))
check('hook armed and probe delivered',
      any(e['event'] == 'hook_diagnostics' and e['armed'] and e['counters'][2] == 2 for e in grab))
check('Win R exact wire sequence', keys('winr') == [(True, 0xffeb), (True, 114), (False, 114), (False, 0xffeb)])

shifts = keys('shifts-repeat')[len(keys('winr')):]
check('Win then Shift uppercase chord exact', shifts[:6] == [
    (True, 0xffeb), (True, 0xffe1), (True, 82),
    (False, 82), (False, 0xffe1), (False, 0xffeb)])
check('retain Shift release ordering failure', shifts[6:14] == [
    (True, 0xffe1), (True, 0xffeb), (True, 82), (False, 82),
    (False, 0xffeb), (True, 0xffeb), (True, 114), (False, 0xffe1)])
basic = keys('system-basics')[len(keys('shifts-repeat')):]
check('retain Alt release ordering failure', basic == [
    (True, 0xffe9), (True, 0xff09), (False, 0xff09),
    (True, 0xffeb), (False, 0xffe9), (False, 0xffeb),
    (True, 0xffec), (False, 0xffec)])

main_enter = keys('enter-held')[len(keys('system-basics')):]
numpad_and_escape = keys('escape')[len(keys('enter-held')):]
for name, events in [('main Enter', main_enter), ('numpad Enter', numpad_and_escape[:8])]:
    check(name + ' owned down up retained', [e for e in events if e[1] == 0xff0d] == [(True, 0xff0d), (False, 0xff0d)])
check('escape sends no new Enter and releases modifiers', numpad_and_escape[8:] == [
    (True, 0xffe3), (True, 0xffe9), (False, 0xffe3), (False, 0xffe9)])
check('escape disarms hook',
      [e for e in lines('escape-app.jsonl') if e['event'] == 'hook_diagnostics'][-1]['armed'] is False)
check('tab and focus loss each release held Win once', keys('lifecycle')[len(keys('escape')):] == [
    (True, 0xffeb), (False, 0xffeb), (True, 0xffeb), (False, 0xffeb)])

for tag in ['winr', 'shifts-repeat', 'system-basics', 'enter-held', 'escape', 'lifecycle', 'final']:
    held = set()
    for down, symbol in keys(tag):
        if down:
            held.add(symbol)
        else:
            check(tag + ' release has owner ' + str(symbol), symbol in held)
            held.remove(symbol)
    check(tag + ' ends with no held key', not held)
check('final key count', len(keys('final')) == 54)

cleanup = load('cleanup.json')
check('independent PID listener cleanup', not cleanup['audit']['processes_remaining'] and not cleanup['audit']['listener_15915'])
check('old owned resources absent', cleanup['audit']['previous_share_absent'] and not cleanup['audit']['previous_tasks'])
for name, state in [('previous', load('previous-cleanup.json')), ('current', cleanup['cleanup'])]:
    check(name + ' owned resources cleaned', state['share_absent'] and not state['remaining_processes'] and not state['tasks_remaining'])
    check(name + ' WinRM unchanged', state['winrm'] == 'Stopped')
    check(name + ' firewall unchanged', {e['Name']: e['Enabled'] for e in state['firewall']} == {'Domain': 1, 'Private': 0, 'Public': 0})

manifest_path = ROOT / 'sha256.json'
if manifest_path.exists():
    for name, expected in load('sha256.json').items():
        check('archive hash ' + name, hashlib.sha256((ROOT / name).read_bytes()).hexdigest() == expected)

print(json.dumps({'outcome': 'passed', 'scope': 'saved evidence integrity and observations; ordering failures retained',
                  'tested_commit': COMMIT, 'checks': len(checks), 'known_failures': [
                      'Shift-up after subsequent Win-down', 'Alt-up after subsequent Win-down',
                      'window modifiers can follow captured Enter-up', 'repeat request is not hardware auto-repeat proof']}, indent=2))
