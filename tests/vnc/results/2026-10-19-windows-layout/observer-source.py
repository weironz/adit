"""Observe only synthetic-window input flags; never inject or swallow keys."""
import argparse
import ctypes as C
from ctypes import wintypes as W
import json
import os
from collections import deque
from pathlib import Path
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--directory', type=Path, required=True)
    parser.add_argument('--pid', type=int, required=True)
    parser.add_argument('--timeout-seconds', type=int, default=1800)
    parser.add_argument('--system-keys-only', action='store_true')
    args = parser.parse_args()
    if not 1 <= args.timeout_seconds <= 1800:
        parser.error('timeout must be between 1 and 1800 seconds')
    args.directory.mkdir(exist_ok=False)
    user = C.WinDLL('user32', use_last_error=True)
    kernel = C.WinDLL('kernel32', use_last_error=True)
    user.GetForegroundWindow.restype = W.HWND
    user.GetWindowThreadProcessId.argtypes = [W.HWND, C.POINTER(W.DWORD)]
    user.CallNextHookEx.argtypes = [W.HANDLE, C.c_int, W.WPARAM, W.LPARAM]
    user.CallNextHookEx.restype = C.c_ssize_t
    kernel.GetModuleHandleW.argtypes = [W.LPCWSTR]
    kernel.GetModuleHandleW.restype = W.HMODULE
    callback_type = C.WINFUNCTYPE(C.c_ssize_t, C.c_int, W.WPARAM, W.LPARAM)
    user.SetWindowsHookExW.argtypes = [C.c_int, callback_type, W.HINSTANCE, W.DWORD]
    user.SetWindowsHookExW.restype = W.HANDLE
    user.UnhookWindowsHookEx.argtypes = [W.HANDLE]

    class Stroke(C.Structure):
        _fields_ = [('vk', W.DWORD), ('scan', W.DWORD), ('flags', W.DWORD),
                    ('time', W.DWORD), ('extra', C.c_size_t)]

    # Limited to the public fixture's matrix; no clipboard or window-title logging.
    allowed = set(range(0x30, 0x5B)) | {8, 9, 13, 16, 17, 18, 20, 27, 32, 37, 38,
              39, 40, 0x5B, 0x5C, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xBA, 0xDE}
    if args.system_keys_only:
        allowed = {9, 13, 27, 0x5B, 0x5C, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5}
    with (args.directory / 'events.jsonl').open('x', encoding='utf-8') as log:
        pending = deque()
        @callback_type
        def callback(code, message, data):
            if code == 0:
                owner = W.DWORD()
                thread = user.GetWindowThreadProcessId(user.GetForegroundWindow(), C.byref(owner))
                value = C.cast(data, C.POINTER(Stroke)).contents
                if owner.value == args.pid and value.vk in allowed:
                    # Keep foreign layout queries out of this callback's tight
                    # Windows deadline. The peer's mapped keys prove the layout.
                    if len(pending) < 2048:
                        pending.append(dict(unix_ms=time.time_ns()//1_000_000,
                        message=message, vk=value.vk, scan=value.scan, flags=value.flags,
                        injected=bool(value.flags & 0x10), foreground_thread=thread))
            return user.CallNextHookEx(None, code, message, data)

        hook = user.SetWindowsHookExW(13, callback, kernel.GetModuleHandleW(None), 0)
        if not hook:
            raise C.WinError(C.get_last_error())
        (args.directory / 'ready.json').write_text(json.dumps({'pid': args.pid,
            'observer_only': True, 'observer_pid': os.getpid(),
            'system_keys_only': args.system_keys_only,
            'unix_ms': time.time_ns()//1_000_000, 'timeout_seconds': args.timeout_seconds}))
        try:
            deadline = time.monotonic()+args.timeout_seconds
            message = W.MSG()
            while time.monotonic() < deadline and not (args.directory/'stop.request').exists():
                while user.PeekMessageW(C.byref(message), None, 0, 0, 1):
                    user.TranslateMessage(C.byref(message))
                    user.DispatchMessageW(C.byref(message))
                while pending:
                    log.write(json.dumps(pending.popleft())+'\n')
                log.flush()
                time.sleep(.005)
        finally:
            assert user.UnhookWindowsHookEx(hook)
            (args.directory / 'stopped.json').write_text('{"hook_removed":true}')


if __name__ == '__main__':
    main()
