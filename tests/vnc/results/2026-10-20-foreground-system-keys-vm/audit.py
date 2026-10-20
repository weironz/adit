"""Check saved peer/OS observations without operating Windows or connecting."""
import json
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parent
checks = []


def read(name):
    return json.loads((ROOT / name).read_text(encoding='utf-8-sig'))


def lines(name):
    return [json.loads(s) for s in (ROOT / name).read_text(encoding='utf-8-sig').splitlines()]


def keys(name):
    return [(r['keysym'], r['down']) for r in lines(name) if r['event'] == 'key']


def require(name, value):
    checks.append({'check': name, 'passed': bool(value)})
    if not value:
        raise AssertionError(name)


ready = read('native-ready.json')
require('target hash matches accepted local executable', ready['source_binary_sha256'].lower()
        == '6bb645965aebd6667ccc4ef31939e51aba35ee16d5b32b3c5703fc2d2df8fa35')
require('observer belongs to this target', read('observer-ready.json')['pid'] == ready['app'])
matrix = keys('matrix-peer.jsonl')
require('Alt Tab received in order', matrix[:4] == [
    (0xffe9, True), (0xff09, True), (0xff09, False), (0xffe9, False)])
require('left and right Win complete', matrix[4:8] == [
    (0xffeb, True), (0xffeb, False), (0xffec, True), (0xffec, False)])
require('Win R failure retained, no fabricated letter success', matrix[8:] == [
    (0xffeb, True), (0xffeb, False)])
matrix_records = [r for r in lines('matrix-peer.jsonl') if r['event'] == 'key']
start, end = matrix_records[0]['unix_ms'], matrix_records[-1]['unix_ms']
app = lines('app.jsonl')
require('no disarm during the initial actual system matrix', not any(
    r['event'] == 'hook_diagnostics' and not r['armed'] and start <= r['unix_ms'] <= end
    for r in app))
dispatchers = [r for r in app if r['event'] == 'hook_dispatcher']
require('capture dispatched on actual foreground window thread', len(dispatchers) == 3
        and all(r['dispatcher_thread'] == r['foreground_window_thread'] != 0 for r in dispatchers))
require('noninjected Alt and R independently observed',
        {0xa4, 0x52}.issubset({r['vk'] for r in lines('observer.jsonl') if not r['injected']}))
escape = keys('escape-peer.jsonl')
require('escape releases Ctrl and Alt without remote Enter', escape[len(matrix):] == [
    (0xffe3, True), (0xffe9, True), (0xffe3, False), (0xffe9, False)])
require('escape actually disarms', any(r['event'] == 'hook_diagnostics' and not r['armed']
        and r['unix_ms'] >= start for r in lines('escape-app.jsonl')))
require('held Win tab switch and focus loss release once each', keys('peer.jsonl')[-4:] == [
    (0xffeb, True), (0xffeb, False), (0xffeb, True), (0xffeb, False)])
balance = Counter()
for symbol, down in keys('peer.jsonl'):
    balance[symbol] += 1 if down else -1
    require(f'no orphan key up {symbol:x}', balance[symbol] >= 0)
require('all remote keys released', not any(balance.values()))
require('final hook disabled', [r for r in app if r['event'] == 'hook_diagnostics'][-1]['armed'] is False)
require('runner stopped only its owned fixture processes', read('stopped.json')['owned_processes_stopped'])
cleanup = read('case1-cleanup-audit.json')
require('independent owned PID and listener absence', not cleanup['processes'] and not cleanup['listeners'])
result = {'scope': 'actual RDP scan input and synthetic VNC peer; Win R failure preserved',
          'outcome': 'passed', 'checks': checks}
(ROOT / 'audit.json').write_text(json.dumps(result, indent=2) + '\n', encoding='utf-8')
print(json.dumps({'outcome': 'passed', 'checks': len(checks)}))
