"""Read-only cross-check of archived runner assertions, pixels and byte hashes."""
import hashlib
import json
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent


def read(path):
    return json.loads((ROOT / path).read_text(encoding='utf-8-sig'))


def pixels(path):
    return Image.open(ROOT / path).convert('RGB').tobytes()


def main():
    manifest = read('manifest.json')
    for name, expected in manifest['files'].items():
        assert hashlib.sha256((ROOT / name).read_bytes()).hexdigest() == expected, name
    actual = {p.relative_to(ROOT).as_posix() for p in ROOT.rglob('*')
              if p.is_file() and p.name != 'manifest.json' and '__pycache__' not in p.parts}
    assert actual == set(manifest['files']), actual ^ set(manifest['files'])
    recovery = read('recovery-39176249851/result.json')
    assert recovery['outcome'] == 'passed'
    for folder in ('lock-39176083194', 'service-39177975803', 'service-39229588182', 'service-39231770633'):
        result = read(f'{folder}/result.json')
        assert result['outcome'] == 'passed'
        lock = read(f'{folder}/lock-screen/result.json')
        assert lock['outcome'] == 'passed'
        assert lock['before_state']['unlocked'] and lock['locked_state']['locked']
        before = pixels(f'{folder}/lock-screen/unlocked.png')
        after = pixels(f'{folder}/lock-screen/locked.png')
        assert hashlib.sha256(before).hexdigest() == lock['before_frame']['sha256']
        assert hashlib.sha256(after).hexdigest() == lock['locked_frame']['sha256']
        assert sum(a != b for a, b in zip(before, after)) / len(before) > .1
        assert 'absent' in result['service_cleanup']
        assert 'absence verified' in result['firewall_cleanup']
    resize = read('service-39177975803/result.json')['display_resize']
    assert [r['dimensions'] for r in resize] == [[1280, 1024], [1600, 1200], [1024, 768]]
    assert all(r['full_frame'] and r['real_canaries'] for r in resize)
    # This subtest passed in a failed combined run: retain that distinction.
    combined = read('service-39178614367/result.json')
    assert combined['outcome'] == 'failed'
    uac = read('service-39178614367/uac/result.json')
    assert uac['outcome'] == 'passed'
    assert uac['medium_token']['elevation_type'] == 3
    assert uac['secure_desktop_policy'] == 1
    assert uac['input_desktop']['access_denied']
    assert uac['prompt_result']['cancelled'] and uac['account_delete_status'] == 0
    assert uac['original_consent_policy'] == uac['restored_consent_policy']
    assert Image.open(ROOT / 'service-39178614367/uac/uac.png').size == (1024, 768)
    passed = read('service-39229588182/uac/result.json')
    assert passed['outcome'] == 'passed'
    assert passed['medium_token']['elevation_type'] == 3
    assert passed['prompt_result']['cancelled'] and passed['account_delete_status'] == 0
    assert passed['original_consent_policy'] == passed['restored_consent_policy']
    native = read('native-app-39230932522/result.json')
    assert native['outcome'] == 'passed' and len(native['samples']) == 21
    assert native['app_sha256'] == native['binary_provenance']['binary_sha256']
    for sample in native['samples']:
        assert max(abs(sample['remote'][axis] - 100) for axis in ('x', 'y')) <= 2
        assert sample['actual_os_dpi'] == 96
        frame = Image.open(ROOT / 'native-app-39230932522' / sample['case'] / f"window-{sample['index']}.png")
        assert min(frame.convert('RGB').getpixel(tuple(sample['screenshot_target']))) >= 245
    small = [s for s in native['samples'] if 'scale-2-' in s['case'] and s['index'] == 2]
    assert len(small) == 2 and all((s['client_box'][3] - s['client_box'][1]) / 2 < 320 for s in small)
    current_recovery = read('recovery-39230932522/result.json')
    assert current_recovery['outcome'] == 'passed' and current_recovery['connections'] == 2
    dpi = read('native-dpi-39231634543/result.json')
    assert dpi['outcome'] == 'passed' and len(dpi['samples']) == 21
    assert dpi['app_sha256'] == native['app_sha256']
    assert {s['actual_os_dpi'] for s in dpi['samples']} == {96, 144, 192}
    for sample in dpi['samples']:
        assert max(abs(sample['remote'][axis] - 100) for axis in ('x', 'y')) <= 2
    assert len(dpi['cursor_samples']) == 24 and len(dpi['cursor_boundary_samples']) == 6
    for sample in dpi['cursor_samples']:
        assert sample['cursor']['flags'] & 1 == 0
        cursor = Image.open(ROOT / 'native-dpi-39231634543' / sample['case'] / f"cursor-{sample['style']}.png").convert('RGB')
        points = [(x, y) for y in range(cursor.height) for x in range(cursor.width)
                  if (lambda c: c[0] > 180 and c[1] < 100 and c[2] > 180)(cursor.getpixel((x, y)))]
        if sample['style'] == 'hidden':
            assert not points and sample['observed_bounds'] is None
        else:
            xs, ys = zip(*points)
            assert [min(xs), min(ys), max(xs), max(ys)] == sample['observed_bounds']
            assert max(abs(sample['received_click'][axis] - 100) for axis in ('x', 'y')) <= 2
        if sample['style'] == 'color':
            assert sample['toolbar_cursor']['flags'] == 1
            rgb = sample['transparent_hole_rgb']
            assert not (rgb[0] > 180 and rgb[1] < 100 and rgb[2] > 180)
    assert all(s['outside']['flags'] == 1 for s in dpi['cursor_boundary_samples'])
    assert read('native-dpi-39231634543/restore-dpi.json')['after_dpi'] == 96
    assert dpi['cleanup']['app_exited'] and dpi['cleanup']['peer_exit'] == 0
    input_uac = read('service-39231770633/uac/result.json')
    assert input_uac['outcome'] == 'passed' and input_uac['medium_token']['elevation_type'] == 3
    assert input_uac['prompt_result']['cancelled'] and input_uac['prompt_result']['error'] == 1223
    assert input_uac['account_delete_status'] == 0
    assert input_uac['original_consent_policy'] == input_uac['restored_consent_policy']
    assert input_uac['cancellation_path'] == 'production SessionManager VncAuth keysyms Escape down/up'
    assert 'ready_ms=' in (ROOT / 'service-39231770633/uac/input.stdout.txt').read_text()
    print(f"PASS: {len(manifest['files'])} byte hashes; 42 native clicks, 24 cursor checks, recovery, service, resize, lock and UAC input evidence")


if __name__ == '__main__':
    main()
