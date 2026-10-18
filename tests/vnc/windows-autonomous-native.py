"""Native production view/input acceptance, isolated disposable hosted Windows."""
import argparse
import ctypes as C
from ctypes import wintypes as W
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import winreg

from PIL import ImageGrab

U = C.WinDLL('user32', use_last_error=True)
U.SetProcessDpiAwarenessContext.argtypes = [C.c_void_p]
U.SetProcessDpiAwarenessContext(C.c_void_p(-4))
U.GetDpiForWindow.argtypes = [W.HWND]
U.GetDpiForWindow.restype = W.UINT
U.SetWindowPos.argtypes = [W.HWND, W.HWND, C.c_int, C.c_int, C.c_int, C.c_int, W.UINT]
U.GetWindowRect.argtypes = [W.HWND, C.POINTER(W.RECT)]
U.GetClientRect.argtypes = [W.HWND, C.POINTER(W.RECT)]
U.ClientToScreen.argtypes = [W.HWND, C.POINTER(W.POINT)]
U.GetWindowThreadProcessId.argtypes = [W.HWND, C.POINTER(W.DWORD)]
U.SetForegroundWindow.argtypes = [W.HWND]
U.PostMessageW.argtypes = [W.HWND, W.UINT, W.WPARAM, W.LPARAM]
U.IsWindowVisible.argtypes = [W.HWND]
U.GetWindowTextW.argtypes = [W.HWND,W.LPWSTR,C.c_int]
U.GetClassNameW.argtypes = [W.HWND,W.LPWSTR,C.c_int]
CALLBACK = C.WINFUNCTYPE(W.BOOL, W.HWND, W.LPARAM)
U.EnumWindows.argtypes = [CALLBACK, W.LPARAM]


class CURSORINFO(C.Structure):
    _fields_=[('size',W.DWORD),('flags',W.DWORD),('cursor',W.HANDLE),('position',W.POINT)]


def cursor_info():
    value=CURSORINFO()
    value.size=C.sizeof(value)
    assert U.GetCursorInfo(C.byref(value))
    return dict(flags=value.flags,position=[value.position.x,value.position.y])


def magenta_bounds(image):
    rgb=image.convert('RGB')
    points=[(x,y) for y in range(rgb.height) for x in range(rgb.width)
            if rgb.getpixel((x,y))[0]>180 and rgb.getpixel((x,y))[1]<100 and rgb.getpixel((x,y))[2]>180]
    if not points:
        return None
    xs,ys=zip(*points)
    return [min(xs),min(ys),max(xs),max(ys)]


def cursor_checks(hwnd,state,log,directory,box,target,cross_width,result,case):
    cx,cy=target
    ratio=cross_width/25
    absolute=(box[0]+cx,box[1]+cy)
    for style in ('arrow','ibeam','color','hidden'):
        previous=len(read_events(state/'events.jsonl'))
        old_generation=max([e.get('cursor',{}).get('generation',0) for e in read_events(log)
                            if e.get('cursor')],default=0)
        temporary=state/'cursor.tmp'
        temporary.write_text(style,encoding='utf-8')
        deadline=time.monotonic()+1
        while True:
            try:
                os.replace(temporary,state/'cursor.request')
                break
            except PermissionError:
                assert time.monotonic()<deadline
                time.sleep(.01)
        metadata=wait_until(lambda: next((e for e in read_events(state/'events.jsonl')[previous:]
                                         if e['event']=='cursor' and e['style']==style),None))
        wait_until(lambda: any(e.get('cursor') and e['cursor']['generation']>old_generation
                               for e in read_events(log)))
        assert U.SetCursorPos(*absolute)
        wait_until(lambda: cursor_info()['flags']==0)
        region=(absolute[0]-40,absolute[1]-40,absolute[0]+40,absolute[1]+40)
        expected=metadata['opaque_bounds']
        def capture():
            image=ImageGrab.grab(bbox=region)
            bounds=magenta_bounds(image)
            if expected is None:
                return (image,bounds) if bounds is None else None
            wanted=[40+(expected[0]-5)*ratio,40+(expected[1]-7)*ratio,
                    40+(expected[2]-5)*ratio,40+(expected[3]-7)*ratio]
            return (image,bounds) if bounds is not None and max(abs(a-b) for a,b in zip(bounds,wanted))<=2 else None
        image,bounds=wait_until(capture)
        image.save(directory/f'cursor-{style}.png')
        info=cursor_info()
        assert info['position']==list(absolute)
        record=dict(case=case,style=style,actual_os_dpi=U.GetDpiForWindow(hwnd),cursor=info,
                    remote_pixels_per_screen_pixel=1/ratio,observed_bounds=bounds,remote_opaque_bounds=expected)
        result.setdefault('cursor_samples',[]).append(record)
        if style=='color':
            hole=image.convert('RGB').getpixel((round(40+3*ratio),round(40+ratio)))
            record['transparent_hole_rgb']=hole
            assert not (hole[0]>180 and hole[1]<100 and hole[2]>180), 'cursor mask hole was painted opaque'
        # Every visible cursor uses the same real click target and nonzero hotspot.
        if style!='hidden':
            before=len(read_events(state/'events.jsonl'))
            U.mouse_event(0x0002,0,0,0,0)
            time.sleep(.08)
            U.mouse_event(0x0004,0,0,0,0)
            received=wait_until(lambda: next((e for e in read_events(state/'events.jsonl')[before:]
                                             if e['event']=='button' and e['mask']==1),None))
            wait_until(lambda: any(e['event']=='button' and e['mask']==0 for e in read_events(state/'events.jsonl')[before:]))
            record['received_click']=received
            assert max(abs(received['x']-100),abs(received['y']-100))<=2
        if style=='color':
            # Just above the framebuffer is the shipping tab/toolbar, not the
            # synthetic test control strip. OS pointer must reappear there.
            toolbar=(absolute[0],round(absolute[1]-100*ratio-8))
            assert box[1]<toolbar[1]<absolute[1]
            assert U.SetCursorPos(*toolbar)
            wait_until(lambda: cursor_info()['flags']==1)
            wait_until(lambda: magenta_bounds(ImageGrab.grab(bbox=region)) is None)
            record['toolbar_cursor']=cursor_info()
            assert U.SetCursorPos(*absolute)
    assert U.SetCursorPos(0,0)
    wait_until(lambda: cursor_info()['flags']==1)
    result.setdefault('cursor_boundary_samples',[]).append(dict(case=case,outside=cursor_info()))


def wait_until(function, seconds=15):
    deadline = time.monotonic()+seconds
    while True:
        value = function()
        if value:
            return value
        if time.monotonic() >= deadline:
            raise TimeoutError(f'condition not satisfied: {function}')
        time.sleep(.05)


def read_events(path):
    if not path.exists():
        return []
    lines = path.read_text(encoding='utf-8').splitlines()
    # Only the trailing append can be incomplete; earlier lines must parse.
    return [json.loads(line) for line in lines[:-1]] + ([json.loads(lines[-1])] if lines and lines[-1].endswith('}') else [])


def find_window(pid):
    found = []
    @CALLBACK
    def visit(hwnd, _):
        owner = W.DWORD()
        U.GetWindowThreadProcessId(hwnd, C.byref(owner))
        if owner.value == pid and U.IsWindowVisible(hwnd):
            title = C.create_unicode_buffer(512)
            U.GetWindowTextW(hwnd,title,512)
            if title.value.startswith('Adit SYNTHETIC acceptance'):
                found.append(hwnd)
        return True
    U.EnumWindows(visit, 0)
    return found[0] if len(found) == 1 else None


def client_box(hwnd):
    rectangle = W.RECT()
    origin = W.POINT()
    assert U.GetClientRect(hwnd, C.byref(rectangle))
    assert U.ClientToScreen(hwnd, C.byref(origin))
    return (origin.x, origin.y, origin.x+rectangle.right, origin.y+rectangle.bottom)


def crosshairs(image):
    """Find actual rendered plus marks; no assumed canvas/screenshot transform."""
    rgb = image.convert('RGB')
    pixels = rgb.load()
    white = {(x, y) for y in range(65, rgb.height-65) for x in range(rgb.width)
             if min(pixels[x, y]) >= 245}
    crosses = []
    while white:
        seed = white.pop()
        component, pending = [seed], [seed]
        while pending:
            x, y = pending.pop()
            for point in ((x-1,y), (x+1,y), (x,y-1), (x,y+1)):
                if point in white:
                    white.remove(point)
                    pending.append(point)
                    component.append(point)
        xs, ys = zip(*component)
        width, height = max(xs)-min(xs)+1, max(ys)-min(ys)+1
        if min(width, height) < 10 or abs(width-height) > 3 or len(component) > width*height*.65:
            continue
        cx, cy = (min(xs)+max(xs))//2, (min(ys)+max(ys))//2
        row = sum(y == cy for x,y in component)
        column = sum(x == cx for x,y in component)
        if row >= width*.7 and column >= height*.7:
            crosses.append((cx, cy, width, height))
    return sorted(crosses, key=lambda item: (item[1], item[0]))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--app', type=Path, required=True)
    parser.add_argument('--results', type=Path, required=True)
    parser.add_argument('--os-dpi-matrix', action='store_true')
    parser.add_argument('--cursor-matrix', action='store_true')
    args = parser.parse_args()
    if os.name != 'nt' or os.environ.get('RUNNER_ENVIRONMENT') != 'github-hosted':
        raise RuntimeError('disposable hosted Windows runner required')
    args.results.mkdir(exist_ok=False)
    def native_control(script, name, *arguments):
        with (args.results/f'{name}.stdout.txt').open('w',encoding='utf-8') as out, (args.results/f'{name}.stderr.txt').open('w',encoding='utf-8') as err:
            subprocess.run(['powershell','-NoProfile','-ExecutionPolicy','Bypass','-File',
                            str(Path(__file__).with_name(script)),*map(str,arguments)],
                           check=True,timeout=90,stdout=out,stderr=err)
    result = dict(outcome='failed', source_sha=os.environ['GITHUB_SHA'], run_id=os.environ['GITHUB_RUN_ID'],
                  app_sha256=hashlib.sha256(args.app.read_bytes()).hexdigest(), samples=[],
                  scope='Actual Windows HWND, native SetWindowPos, screen pixels, injected mouse and production view/dispatcher against synthetic RFB peer',
                  limitations=['Application scales 100/150/200 are not OS DPI changes.',
                               'Injected pointer is not physical keyboard/IME acceptance.',
                               'Synthetic None peer is not vendor compatibility or secure desktop acceptance.'])
    policy = r'SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System'
    with winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE, policy) as key:
        result['uac_policy'] = {}
        for name in ('EnableLUA','ConsentPromptBehaviorAdmin','PromptOnSecureDesktop'):
            try:
                result['uac_policy'][name] = winreg.QueryValueEx(key, name)[0]
            except FileNotFoundError:
                result['uac_policy'][name] = 'absent; OS default not inferred'
    state = args.results/'peer-state'
    peer_out = (args.results/'peer.stdout.txt').open('w', encoding='utf-8')
    peer_err = (args.results/'peer.stderr.txt').open('w', encoding='utf-8')
    peer = subprocess.Popen([sys.executable, '-u', str(Path(__file__).with_name('native_rfb_fixture.py')),
                             '--state-dir', str(state.resolve())], stdout=peer_out, stderr=peer_err)
    app = None
    initial_display = None
    try:
        wait_until(lambda: (state/'ready.json').exists())
        if args.os_dpi_matrix:
            native_control('windows-desktop-capabilities.ps1','settings','-Results',args.results.resolve())
            native_control('windows-native-display.ps1','display-test-mode','-Width',1600,'-Height',1200,
                           '-Evidence',(args.results/'display-test-mode.json').resolve())
            initial_display=json.loads((args.results/'display-test-mode.json').read_text(encoding='utf-8-sig'))['before']
            cases=[(os_scale,1,fit,False) for os_scale in (100,150,200) for fit in (True,False)] + [(100,1,True,True)]
        else:
            cases=[(100,s,f,False) for s in (1,1.5,2) for f in (True,False)] + [(100,1,True,True)]
        current_os_scale=None
        for os_scale, scale, fit, resize in cases:
            if args.os_dpi_matrix and current_os_scale != os_scale:
                native_control('windows-native-dpi.ps1',f'dpi-{os_scale}','-Scale',os_scale,
                               '-Evidence',(args.results/f'dpi-{os_scale}.json').resolve())
                current_os_scale=os_scale
            name = f'os-{os_scale}-scale-{scale}-fit-{fit}-resize-{resize}'
            directory = args.results/name
            directory.mkdir()
            env = dict(os.environ, ADIT_NATIVE_SCALE=str(scale), ADIT_NATIVE_FIT='1' if fit else '0',
                       ADIT_NATIVE_RESIZE='1' if resize else '0')
            with (directory/'stdout.txt').open('w', encoding='utf-8') as out, (directory/'stderr.txt').open('w', encoding='utf-8') as err:
                app = subprocess.Popen([str(args.app.resolve())], env=env, stdout=out, stderr=err)
                log = Path(tempfile.gettempdir())/f'adit-vnc-native-acceptance-{app.pid}'/'events.jsonl'
                hwnd = wait_until(lambda: find_window(app.pid), 30)
                title, window_class = C.create_unicode_buffer(512), C.create_unicode_buffer(128)
                U.GetWindowTextW(hwnd,title,512)
                U.GetClassNameW(hwnd,window_class,128)
                (directory/'hwnd.json').write_text(json.dumps(dict(hwnd=hwnd,title=title.value,window_class=window_class.value,pid=app.pid)),encoding='utf-8')
                U.SetForegroundWindow(hwnd)
                wait_until(lambda: any(e['event']=='geometry' for e in read_events(log)), 30)
                for index, (width,height) in enumerate(((1000,700),(1200,820),(900,640))):
                    assert app.poll() is None and peer.poll() is None
                    previous = len(read_events(state/'events.jsonl'))
                    client_before = len(read_events(log))
                    assert U.SetWindowPos(hwnd, None, 10, 10, width, height, 0x0040)
                    wait_until(lambda: (lambda r: r.right-r.left == width and r.bottom-r.top == height)(get_rect(hwnd)))
                    if resize:
                        changed = wait_until(lambda: next((e for e in read_events(state/'events.jsonl')[previous:]
                                                           if e['event']=='set_desktop_size'), None))
                        wait_until(lambda: any(e['event']=='geometry' and e['surface']==[changed['width'],changed['height']]
                                               for e in read_events(log)))
                    # Wait for both a stable screenshot and a native window event.
                    wait_until(lambda: any(e['event']=='window' for e in read_events(log)[client_before:]))
                    stable = [None, time.monotonic()]
                    def capture_cross():
                        image = ImageGrab.grab(bbox=client_box(hwnd))
                        found = crosshairs(image)
                        signature = (image.size, tuple(found), hashlib.sha256(image.tobytes()).digest())
                        if signature != stable[0]:
                            stable[:] = [signature,time.monotonic()]
                            return None
                        # WM_SIZE reaches the event loop before DWM presents the
                        # new framebuffer. Clicking the previous GPU image would
                        # compare coordinates from two different window layouts.
                        return (image, found) if found and time.monotonic()-stable[1] >= .4 else None
                    image, found = wait_until(capture_cross, 20)
                    # The first visible grid cross is the public (100,100) target.
                    cx,cy,_,_ = found[0]
                    box = client_box(hwnd)
                    before = len(read_events(state/'events.jsonl'))
                    assert U.SetCursorPos(box[0]+cx, box[1]+cy)
                    time.sleep(.15)
                    U.mouse_event(0x0002,0,0,0,0)
                    time.sleep(.08)
                    U.mouse_event(0x0004,0,0,0,0)
                    received = wait_until(lambda: next((e for e in read_events(state/'events.jsonl')[before:]
                                                       if e['event']=='button' and e['mask']==1), None))
                    wait_until(lambda: any(e['event']=='button' and e['mask']==0 for e in read_events(state/'events.jsonl')[before:]))
                    image.save(directory/f'window-{index}.png')
                    sample = dict(case=name, index=index, window=[width,height], client_box=box,
                                  actual_os_dpi=U.GetDpiForWindow(hwnd), screenshot_target=[cx,cy],
                                  candidates=found, remote=received, error=[abs(received['x']-100),abs(received['y']-100)])
                    result['samples'].append(sample)
                    assert sample['actual_os_dpi'] == 96*os_scale//100, sample
                    assert max(sample['error']) <= 2, sample
                    if not resize:
                        assert (received['width'],received['height']) == (1200,800)
                    if args.cursor_matrix and index==0 and not resize:
                        cursor_checks(hwnd,state,log,directory,box,(cx,cy),found[0][2],result,name)
                U.PostMessageW(hwnd, 0x0010, 0, 0)
                assert app.wait(timeout=10) == 0
                (directory/'client-events.jsonl').write_bytes(log.read_bytes())
                app = None
        result['outcome'] = 'passed'
    except Exception as error:
        result['error'] = repr(error)
        ImageGrab.grab().save(args.results/'failure-screen.png')
        raise
    finally:
        if app is not None:
            log = Path(tempfile.gettempdir())/f'adit-vnc-native-acceptance-{app.pid}'/'events.jsonl'
            if log.exists():
                (args.results/'failed-client-events.jsonl').write_bytes(log.read_bytes())
            if app.poll() is None:
                app.kill()
                app.wait(timeout=5)
        if state.exists():
            (state/'stop.request').touch()
        try:
            peer.wait(timeout=5)
        except subprocess.TimeoutExpired:
            peer.kill()
            peer.wait(timeout=5)
        peer_out.close()
        peer_err.close()
        if initial_display is not None:
            try:
                native_control('windows-native-dpi.ps1','restore-dpi','-Scale',100,
                               '-Evidence',(args.results/'restore-dpi.json').resolve())
                native_control('windows-native-display.ps1','restore-display','-Width',initial_display['width'],
                               '-Height',initial_display['height'],'-Evidence',(args.results/'restore-display.json').resolve())
            except Exception as error:
                result['restore_error']=repr(error)
                result['outcome']='failed'
        result['cleanup'] = dict(app_exited=app is None or app.poll() is not None, peer_exit=peer.returncode)
        (args.results/'result.json').write_text(json.dumps(result, indent=2)+'\n', encoding='utf-8')
        assert 'restore_error' not in result, result.get('restore_error')


def get_rect(hwnd):
    value = W.RECT()
    assert U.GetWindowRect(hwnd, C.byref(value))
    return value


if __name__ == '__main__':
    main()
