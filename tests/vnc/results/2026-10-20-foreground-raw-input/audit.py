"""Audit the saved synthetic evidence; does not operate the desktop."""
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent
checks = []


def read(name):
    return json.loads((ROOT / name).read_text(encoding='utf-8-sig'))


def require(name, condition):
    checks.append({'check': name, 'passed': bool(condition)})
    if not condition:
        raise AssertionError(name)


baseline = read('baseline-provenance.json')
require('original fixture matches captured source hash', hashlib.sha256(
    (ROOT / 'baseline-gui-source.rs.txt').read_bytes()).hexdigest()
    == baseline['fixture_sha256'].lower())
for name, expected, delta in [
    ('baseline-raw-default.json', 'failed', 0),
    ('baseline-raw-disabled.json', 'passed', 2),
]:
    report = read(name)
    require(name + ': outcome and actual foreground', report['outcome'] == expected
            and report['continuous_foreground'])
    require(name + ': ordinary window receives F24', report['ui_f24'] == 2)
    require(name + ': own hook marker delta', report['cycles'][0]['injected_delta'] == delta)

for name, expected, delta in [
    ('baseline-win32-alt.json', 'passed', 2),
    ('baseline-winit-default.json', 'failed', 0),
    ('baseline-winit-never.json', 'passed', 2),
]:
    report = read(name)
    require(name + ': outcome and continuous focus', report['outcome'] == expected
            and report['continuous_foreground'] and report['continuous_focus'])
    require(name + ': window and marker observed separately',
            report['window_tagged_f24_events'] == 2 and report['own_probe_delta'] == delta)

for name in ['first-full-incomplete.json', 'first-minimal-incomplete.json',
             'second-minimal-incomplete.json']:
    require(name + ': incomplete retained', read(name)['outcome'] == 'incomplete')

report = read('full-native-passed.json')
require('full GUI passed all three cycles', report['outcome'] == 'passed'
        and len(report['cycles']) == 3)
for cycle in report['cycles']:
    prefix = f"cycle {cycle['cycle']}: "
    require(prefix + 'actual focus and capture stay active',
            all(cycle[key] for key in ['passed', 'continuous_foreground',
                'continuous_focus', 'continuous_grab', 'grab_on', 'hook_armed']))
    require(prefix + 'own injected F24 delivered but not swallowed',
            cycle['own_probe_delta'] >= 2 and cycle['injected_f24_accepted'] >= 2
            and cycle['swallowed_delta'] == 0)
    require(prefix + 'keyboard suspended and all devices restored',
            all(cycle[key] for key in ['keyboard_removed', 'other_devices_unchanged',
                'raw_restored', 'grab_off', 'hook_off'])
            and cycle['baseline_raw'] == cycle['raw_off'])
require('final hook and capture disabled, exact restoration', report['final_restored']
        and report['grab_final'] is False and report['hook_final']['armed'] is False)
invocation = read('full-native-invocation.json')
require('wrapper successful and owned processes exited', invocation['completed']
        and invocation['exit_code'] == 0 and invocation['outcome'] == 'passed'
        and invocation['owned_processes_exited'])
native = (ROOT / 'native-regressions.txt').read_text(encoding='utf-8-sig')
require('three explicit native regressions passed',
        'test result: ok. 3 passed; 0 failed' in native)
ci = (ROOT / 'local-ci-passed.txt').read_text(encoding='utf-8-sig')
require('feature library and vendor tests passed',
        'test result: ok. 129 passed; 0 failed; 3 ignored' in ci
        and 'test result: ok. 202 passed; 0 failed' in ci)
require('no final compiler error', 'error: could not compile' not in ci)
result = {'scope': 'saved synthetic evidence only; no hardware-input acceptance',
          'outcome': 'passed', 'checks': checks}
(ROOT / 'audit.json').write_text(json.dumps(result, ensure_ascii=False, indent=2)
                                 + '\n', encoding='utf-8')
print(json.dumps({'outcome': 'passed', 'checks': len(checks)}))
