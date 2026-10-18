"""Actual lock-screen capture through the owned service; never run on a workstation."""
import argparse
import ctypes as C
from ctypes import wintypes as W
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import time


def session_state():
    wts = C.WinDLL('wtsapi32', use_last_error=True)
    wts.WTSQuerySessionInformationW.argtypes = [W.HANDLE,W.DWORD,C.c_int,C.POINTER(C.c_void_p),C.POINTER(W.DWORD)]
    wts.WTSFreeMemory.argtypes = [C.c_void_p]
    pointer, size = C.c_void_p(), W.DWORD()
    if not wts.WTSQuerySessionInformationW(None, 0xffffffff, 25, C.byref(pointer), C.byref(size)):
        raise C.WinError(C.get_last_error())
    try:
        assert size.value >= 20 and C.c_uint32.from_address(pointer.value).value == 1
        # The level-1 union has 8-byte alignment because it contains LARGE_INTEGER.
        session = C.c_uint32.from_address(pointer.value+8).value
        state = C.c_int32.from_address(pointer.value+12).value
        flags = C.c_int32.from_address(pointer.value+16).value
        own_session = W.DWORD()
        assert C.WinDLL('kernel32').ProcessIdToSessionId(os.getpid(), C.byref(own_session))
        assert session == own_session.value and session > 0
        return dict(session_id=session, connection_state=state, flags=flags,
                    locked=flags == 0, unlocked=flags == 1)
    finally:
        wts.WTSFreeMemory(pointer)


def read_ppm(path):
    header, dimensions, maximum, pixels = path.read_bytes().split(b'\n',3)
    assert header == b'P6' and maximum == b'255'
    width,height = map(int, dimensions.split())
    assert len(pixels) == width*height*3
    return width,height,pixels


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--probe',type=Path,required=True)
    parser.add_argument('--results',type=Path,required=True)
    args = parser.parse_args()
    if os.name != 'nt' or os.environ.get('RUNNER_ENVIRONMENT') != 'github-hosted':
        raise RuntimeError('disposable hosted Windows runner required')
    args.results.mkdir(exist_ok=False)
    result = dict(outcome='failed',source_sha=os.environ['GITHUB_SHA'],run_id=os.environ['GITHUB_RUN_ID'],
                  scope='Fresh production SessionManager VncAuth connections to attributed UltraVNC service before/after actual WTS workstation lock',
                  limitations=['Does not prove unlock, password entry, UAC, secure attention or uninterrupted existing-session transition.',
                               'Disposable runner remains locked until destruction; no user workstation state is changed.'],
                  references=['https://learn.microsoft.com/en-us/windows/win32/api/wtsapi32/ns-wtsapi32-wtsinfoex_level1_w',
                              'https://learn.microsoft.com/en-us/windows/win32/shutdown/how-to-lock-the-workstation'])

    def capture(name):
        snapshot = args.results/f'{name}.ppm'
        with (args.results/f'{name}.stdout.txt').open('w',encoding='utf-8') as out, (args.results/f'{name}.stderr.txt').open('w',encoding='utf-8') as err:
            completed = subprocess.run([str(args.probe.resolve()),'127.0.0.1','15911','20',str(snapshot.resolve()),'--steady-seconds=3'],
                                       input='adit-ci\n',text=True,stdout=out,stderr=err,timeout=30)
        stdout = (args.results/f'{name}.stdout.txt').read_text(encoding='utf-8')
        assert completed.returncode == 0 and re.search(r'^complete=true\r?$',stdout,re.M)
        assert 'final_connected=true disconnects=0 connection_error=false steady_complete=true' in stdout
        assert 'security: "VncAuth"' in stdout
        assert re.search(r'steady_ms=(\d+)',stdout) and int(re.search(r'steady_ms=(\d+)',stdout)[1]) >= 3000
        width,height,pixels = read_ppm(snapshot)
        info = dict(width=width,height=height,sha256=hashlib.sha256(pixels).hexdigest(),
                    colours=len(set(pixels[index:index+3] for index in range(0,len(pixels),3))))
        assert info['colours'] > 8
        return info,pixels

    try:
        result['before_state'] = session_state()
        assert result['before_state']['unlocked'], result['before_state']
        before,before_pixels = capture('unlocked')
        result['before_frame'] = before
        assert C.WinDLL('user32',use_last_error=True).LockWorkStation()
        deadline = time.monotonic()+15
        while True:
            state = session_state()
            if state['locked']:
                break
            assert time.monotonic() < deadline, 'WTS did not confirm locked'
            time.sleep(.1)
        result['locked_state'] = state
        after,after_pixels = capture('locked')
        result['locked_frame'] = after
        assert session_state()['locked']
        assert (before['width'],before['height']) == (after['width'],after['height'])
        assert before['sha256'] != after['sha256']
        changed = sum(a != b for a,b in zip(before_pixels,after_pixels))/len(before_pixels)
        result['changed_channel_fraction'] = changed
        assert changed > .1, 'lock capture is substantially stale'
        # Both independent fixed public canaries must disappear on Winlogon.
        for x,y,wanted in ((40,40,(224,48,32)),(670,470,(32,80,224))):
            offset = (y*before['width']+x)*3
            assert all(abs(a-b)<=24 for a,b in zip(before_pixels[offset:offset+3],wanted)), 'baseline target missing'
            assert any(abs(a-b)>24 for a,b in zip(after_pixels[offset:offset+3],wanted)), 'old desktop canary remained'
        result['outcome']='passed'
    except Exception as error:
        result['error']=repr(error)
        raise
    finally:
        (args.results/'result.json').write_text(json.dumps(result,indent=2)+'\n',encoding='utf-8')


if __name__ == '__main__':
    main()
