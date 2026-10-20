"""Audit saved native evidence; never inject input or contact a test host."""
import hashlib
import json
import re
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent
checks = 0


def check(condition, label):
    global checks
    assert condition, label
    checks += 1


def obj(name):
    return json.loads((ROOT / name).read_text(encoding='utf-8-sig'))


def rows(name):
    return [json.loads(line) for line in (ROOT / name).read_text(
        encoding='utf-8-sig').splitlines() if line]


def pair(symbol):
    return [(symbol, True), (symbol, False)]


WL, WR, CL, CR, AL, AG, SL, SR, ENTER, TAB, CAPS = (
    0xffeb, 0xffec, 0xffe3, 0xffe4, 0xffe9, 0xfe03,
    0xffe1, 0xffe2, 0xff0d, 0xff09, 0xffe5)


def chord(symbol, win=WL):
    return [(win, True)] + pair(symbol) + [(win, False)]


def altgr(symbol):
    return [(AG, True)] + pair(symbol) + [(AG, False)]


escape = [(CL, True), (AL, True), (CL, False), (AL, False)]
core = [(AL, True)] + pair(TAB) + [(AL, False)] + pair(WR)
core += [(SL, True)] + chord(ord('R')) + [(SL, False)]
core += [(WL, True), (SR, True)] + pair(ord('R')) + [(SR, False), (WL, False)]
core += [(WL, True), (ENTER, True), (CL, True), (AL, True),
         (ENTER, False), (AL, False), (CL, False), (WL, False)]
core += [(WL, True), (CL, True), (AL, True), (WL, False), (CL, False), (AL, False)]

# This run exposed an extra Ctrl before the first AltGr. Retain it exactly and
# explicitly reject the clean AltGr acceptance claim instead of trimming it.
anomalous_altgr = [(CL, True)] + altgr(ord('@')) + altgr(0x010020ac) + chord(ord('r'))
anomalous_altgr += [(CL, True)] + pair(ord('a')) + [(CL, False)] + chord(ord('r'))
anomalous_altgr += [(CR, True)] + pair(ord('a')) + [(CR, False)] + chord(ord('r'))
anomalous_altgr += [(WL, True), (CL, True)] + altgr(ord('@')) + [(WL, False)]

deltas = {
    'grab': [],
    'pending-r': chord(ord('r')),
    'pending-r-finish': pair(ord('é')),
    'pending-e': chord(ord('e')),
    'pending-e-finish': pair(ord('é')),
    'altgr': anomalous_altgr,
    'german-extra': chord(ord('z')) + chord(ord('y')) +
        [(CAPS, True), (WL, True), (CAPS, False)] + pair(ord('!')) + [(WL, False)] +
        pair(CAPS) + chord(ord('r'), WR) + pair(ord('é')),
    'pinyin-switch': escape,
    'pinyin-grab': [],
    'english-and-switch': chord(ord('r')) + escape,
    'chinese-grab': chord(ord('r')),
    'chinese-preedit': [],
    'chinese-commit': pair(0x01004e2d) + pair(0x01006587),
    'core': core,
    'before-focus': [], 'focus-away': [], 'refocus': [],
    'final-rearm': chord(ord('r')), 'final': escape,
}
expected = []
for phase, delta in deltas.items():
    expected += delta
    actual = [(e['keysym'], e['down']) for e in rows('stateless-' + phase + '-peer.jsonl')
              if e['event'] == 'key']
    check(actual == expected, phase + ' exact cumulative wire sequence')
check(len(expected) == 130, 'all 130 remote key events retained')
held = set()
for symbol, down in expected:
    if down:
        check(symbol not in held or symbol == CL,
              'only the explicitly retained anomalous Ctrl may repeat its down')
        held.add(symbol)
    else:
        check(symbol in held, 'release has an owned down')
        held.remove(symbol)
check(not held, 'final remote keys all released')
check(anomalous_altgr[0] == (CL, True) and (CL, False) not in anomalous_altgr[:13],
      'first AltGr Ctrl retention recorded as anomaly, not accepted')


def state(phase):
    return [e['state'] for e in rows('stateless-' + phase + '-app.jsonl')
            if e['event'] == 'native_ime_state'][-1]


for phase in ('pending-r', 'pending-e', 'german-extra'):
    s = state(phase)
    check(s['grab'] and s['foreground'] and s['layout'] == 0x04070407,
          phase + ' actual German foreground capture')
    diagnostics = [e for e in rows('stateless-' + phase + '-app.jsonl')
                   if e['event'] == 'hook_diagnostics'][-1]
    check(diagnostics['armed'] and diagnostics['vnc'], phase + ' real hook armed')
check(state('pinyin-grab')['layout'] == 0x04090409, 'intermediate switch actually English')
check(state('chinese-grab')['layout'] == 0x08040804, 'actual Chinese HKL')
preedit = [e for e in rows('stateless-chinese-preedit-app.jsonl')
           if e['event'] == 'native_ime_event' and e.get('kind') == 'preedit'
           and e.get('scalars', 0) > 0]
check(bool(preedit), 'real native Chinese preedit observed')
check(all(e['grab'] and e['epoch'] == e['current_epoch'] and e['accepts_text']
          for e in preedit), 'preedit belongs to current captured window')
check(state('before-focus')['grab'], 'armed before losing focus')
check(not state('focus-away')['foreground'] and not state('focus-away')['grab'],
      'focus loss disarms')
check(state('refocus')['foreground'] and not state('refocus')['grab'],
      'refocus does not silently rearm')
check(state('final-rearm')['grab'] and not state('final')['grab'],
      'explicit rearm and local escape')
observed = rows('stateless-final-observer.jsonl')
check(bool(observed) and all(not e['injected'] for e in observed),
      'independent OS observer noninjected input')
check(any(e['vk'] == 0xa2 and e['scan'] == 0x21d for e in observed),
      'auxiliary Ctrl full scan preserved')
check('translation unavailable' not in (ROOT / 'stateless-final-stderr.txt').read_text(),
      'no rejected shortcut translation in final run')
gate = obj('stateless-integrated-dead-test.json')
check(gate['exit_code'] == 0, 'integrated native composition isolation passes')
check(gate['executable_sha256'].lower() == obj('provenance.json')['native_test_exe_sha256'],
      'native test executable identity')
check('1 passed; 0 failed; 0 ignored' in (ROOT / 'stateless-integrated-dead-stdout.txt').read_text(),
      'native test actually executed')
check(obj('rejected/dead-isolation2.json')['exit_code'] != 0, 'worker isolation rejected')
desktop = obj('rejected/dead-desktop-probe-result.json')
check(not desktop['passed'] and desktop['restored'] and desktop['closed']
      and desktop['threads_exited'], 'private desktop hypothesis rejected and recovered')
before = obj('stateless-independent-before-cleanup.json')
check(before['language_restored'] and not before['owned_processes'] and not before['listeners'],
      'independent language and process recovery')
final = obj('stateless-cleanup-reports.json')[-1]
check(final['stage'] == 'owned_cleanup_final' and final['directory_absent']
      and final['share_absent'] and not final['tasks'] and not final['processes']
      and not final['listeners'], 'owned GUID resources removed')
check(before['baseline'] == before['languages'] == final['languages'], 'full language baseline restored')
for path, sha in obj('provenance.json')['product_files_sha256'].items():
    content = (ROOT / 'source' / Path(path).name).read_bytes()
    check(hashlib.sha256(content.replace(b'\r\n', b'\n')).hexdigest() == sha,
          path + ' archived source identity')
provenance = obj('provenance.json')
for path, sha in provenance['final_product_files_lf_sha256'].items():
    saved = (ROOT / 'final-source' / Path(path).name).read_bytes().replace(b'\r\n', b'\n')
    git_blob = subprocess.run(['git', 'show', provenance['final_product_git_commit'] + ':' + path],
                              cwd=ROOT, check=True, capture_output=True).stdout
    check(hashlib.sha256(saved).hexdigest() == sha and saved == git_blob,
          path + ' matches final committed Git blob')
tested_hook = (ROOT / 'source/keyboard_hook.rs').read_text(encoding='utf-8')
final_hook = (ROOT / 'final-source/keyboard_hook.rs').read_text(encoding='utf-8')
without_test_helper = re.sub(
    r'\n#\[cfg\(all\(test, windows\)\)\]\npub\(crate\) fn prepare_test_layout\(layout: isize\) \{.*?\n\}\n',
    '', final_hook, count=1, flags=re.S)
check(without_test_helper == tested_hook, 'post-GUI hook change is cfg(test) helper only')
ci = obj('ci-result.json')
check(ci['exit_code'] == 0 and not ci['hosted_actions_started'], 'full local CI completed')
check([x['summed_results'] for x in ci['sections']] ==
      [[430, 0, 11], [0, 0, 0], [145, 0, 4], [202, 0, 0]], 'separate CI counts')
if (ROOT / 'SHA256SUMS').exists():
    for line in (ROOT / 'SHA256SUMS').read_text().splitlines():
        sha, path = line.split('  ', 1)
        b = (ROOT / path).read_bytes()
        if Path(path).suffix != '.png':
            b = b.replace(b'\r\n', b'\n')
        check(hashlib.sha256(b).hexdigest() == sha, path + ' manifest')
print(json.dumps({'checks_passed': checks, 'remote_key_events': 130,
                  'dead_key_shortcuts': 'passed', 'altgr': 'incomplete_stray_ctrl',
                  'cleanup_verified': True}, ensure_ascii=False))
