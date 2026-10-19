"""Temporarily switch only the synthetic acceptance HWND's keyboard layout."""
import argparse
import ctypes as C
from ctypes import wintypes as W
import json
from pathlib import Path
import time


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--pid', type=int, required=True)
    p.add_argument('--layout')
    p.add_argument('--record', type=Path, required=True)
    p.add_argument('--restore', action='store_true')
    args = p.parse_args()
    user = C.WinDLL('user32', use_last_error=True)
    user.GetWindowThreadProcessId.argtypes = [W.HWND, C.POINTER(W.DWORD)]
    user.GetKeyboardLayout.argtypes = [W.DWORD]
    user.GetKeyboardLayout.restype = C.c_void_p
    user.GetWindowTextW.argtypes = [W.HWND, W.LPWSTR, C.c_int]
    user.LoadKeyboardLayoutW.argtypes = [W.LPCWSTR, W.UINT]
    user.LoadKeyboardLayoutW.restype = C.c_void_p
    user.PostMessageW.argtypes = [W.HWND, W.UINT, W.WPARAM, W.LPARAM]
    user.UnloadKeyboardLayout.argtypes = [C.c_void_p]
    callback_type = C.WINFUNCTYPE(W.BOOL, W.HWND, W.LPARAM)
    user.EnumWindows.argtypes = [callback_type, W.LPARAM]
    windows = []

    @callback_type
    def window(hwnd, _):
        owner = W.DWORD()
        thread = user.GetWindowThreadProcessId(hwnd, C.byref(owner))
        title = C.create_unicode_buffer(256)
        user.GetWindowTextW(hwnd, title, 256)
        if owner.value == args.pid and title.value.startswith('Adit SYNTHETIC acceptance'):
            windows.append((hwnd, thread))
        return True

    assert user.EnumWindows(window, 0)
    assert len(windows) == 1, 'Exactly one owned synthetic HWND is required'
    hwnd, thread = windows[0]
    before = user.GetKeyboardLayout(thread)
    if args.restore:
        record = json.loads(args.record.read_text())
        assert record['pid'] == args.pid
        desired = record['before']
    else:
        assert not args.record.exists()
        desired = user.LoadKeyboardLayoutW(args.layout, 0)
        assert desired, C.WinError(C.get_last_error())
        record = dict(pid=args.pid, before=before, desired=desired, klid=args.layout)
    assert user.PostMessageW(hwnd, 0x50, 0, desired)
    end = time.monotonic()+10
    while user.GetKeyboardLayout(thread) != desired:
        assert time.monotonic() < end, 'Window did not accept layout change'
        time.sleep(.05)
    if args.restore:
        result = dict(pid=args.pid, restored=desired, observed=user.GetKeyboardLayout(thread))
        args.record.with_suffix('.restored.json').write_text(json.dumps(result, indent=2)+'\n')
    else:
        record['observed'] = user.GetKeyboardLayout(thread)
        args.record.write_text(json.dumps(record, indent=2)+'\n')


if __name__ == '__main__':
    main()
