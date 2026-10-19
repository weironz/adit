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
    for folder in ('lock-39176083194', 'service-39177975803', 'service-39229588182'):
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
    print(f"PASS: {len(manifest['files'])} byte hashes; scoped recovery, service, resize, lock and UAC evidence")


if __name__ == '__main__':
    main()
