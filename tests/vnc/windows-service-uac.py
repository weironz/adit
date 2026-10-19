"""Disposable Windows UAC fixture: fresh synthetic account; no user credentials."""
import argparse
import ctypes as C
from ctypes import wintypes as W
import hashlib
import json
import os
from pathlib import Path
import secrets
import shutil
import subprocess
import sys
import time
import winreg


class STARTUPINFO(C.Structure):
    _fields_=[('cb',W.DWORD),('reserved',W.LPWSTR),('desktop',W.LPWSTR),('title',W.LPWSTR),
              ('x',W.DWORD),('y',W.DWORD),('xsize',W.DWORD),('ysize',W.DWORD),('xchars',W.DWORD),('ychars',W.DWORD),
              ('fill',W.DWORD),('flags',W.DWORD),('show',W.WORD),('reserved_size',W.WORD),('reserved_ptr',C.c_void_p),
              ('stdin',W.HANDLE),('stdout',W.HANDLE),('stderr',W.HANDLE)]


class PROCESSINFO(C.Structure):
    _fields_=[('process',W.HANDLE),('thread',W.HANDLE),('pid',W.DWORD),('tid',W.DWORD)]


class USER_INFO_1(C.Structure):
    _fields_=[('name',W.LPWSTR),('password',W.LPWSTR),('age',W.DWORD),('priv',W.DWORD),
              ('home',W.LPWSTR),('comment',W.LPWSTR),('flags',W.DWORD),('script',W.LPWSTR)]


def write(path,value):
    temporary=path.with_suffix('.tmp')
    temporary.write_text(json.dumps(value,indent=2)+'\n',encoding='utf-8')
    os.replace(temporary,path)


def medium(directory):
    adv=C.WinDLL('advapi32',use_last_error=True)
    kernel=C.WinDLL('kernel32',use_last_error=True)
    kernel.GetCurrentProcess.restype=W.HANDLE
    adv.OpenProcessToken.argtypes=[W.HANDLE,W.DWORD,C.POINTER(W.HANDLE)]
    adv.GetTokenInformation.argtypes=[W.HANDLE,C.c_int,C.c_void_p,W.DWORD,C.POINTER(W.DWORD)]
    token=W.HANDLE()
    assert adv.OpenProcessToken(kernel.GetCurrentProcess(),8,C.byref(token))
    try:
        elevation,needed=W.DWORD(),W.DWORD()
        assert adv.GetTokenInformation(token,18,C.byref(elevation),4,C.byref(needed))
    finally:
        kernel.CloseHandle.argtypes=[W.HANDLE]
        kernel.CloseHandle(token)
    write(directory/'medium-token.json',dict(elevation_type=elevation.value,pid=os.getpid()))
    if elevation.value!=3:
        return 3
    shell=C.WinDLL('shell32',use_last_error=True)
    shell.ShellExecuteW.argtypes=[W.HWND,W.LPCWSTR,W.LPCWSTR,W.LPCWSTR,W.LPCWSTR,C.c_int]
    shell.ShellExecuteW.restype=C.c_void_p
    value=shell.ShellExecuteW(None,'runas',str(Path(os.environ['WINDIR'])/'System32/cmd.exe'),'/d /c exit 0',str(directory),0)
    error=C.get_last_error()
    write(directory/'prompt-result.json',dict(return_value=value,error=error,cancelled=bool((value or 0)<=32 and error==1223)))
    return 0


def main(args):
    if os.name!='nt' or os.environ.get('RUNNER_ENVIRONMENT')!='github-hosted':
        raise RuntimeError('disposable hosted Windows only')
    if args.medium:
        return medium(args.results)
    args.results.mkdir(exist_ok=False)
    account='AditCi_'+secrets.token_hex(6)
    public=Path(os.environ['PUBLIC'])/account
    public.mkdir(exist_ok=False)
    net=C.WinDLL('netapi32',use_last_error=True)
    net.NetUserAdd.argtypes=[W.LPCWSTR,W.DWORD,C.c_void_p,C.POINTER(W.DWORD)]
    net.NetUserDel.argtypes=[W.LPCWSTR,W.LPCWSTR]
    net.NetLocalGroupAddMembers.argtypes=[W.LPCWSTR,W.LPCWSTR,W.DWORD,C.c_void_p,W.DWORD]
    adv=C.WinDLL('advapi32',use_last_error=True)
    adv.CreateProcessWithLogonW.argtypes=[W.LPCWSTR,W.LPCWSTR,W.LPCWSTR,W.DWORD,W.LPCWSTR,W.LPWSTR,W.DWORD,C.c_void_p,W.LPCWSTR,C.POINTER(STARTUPINFO),C.POINTER(PROCESSINFO)]
    kernel=C.WinDLL('kernel32',use_last_error=True)
    kernel.WaitForSingleObject.argtypes=[W.HANDLE,W.DWORD]
    kernel.TerminateProcess.argtypes=[W.HANDLE,W.UINT]
    kernel.CloseHandle.argtypes=[W.HANDLE]
    policy=r'SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System'
    key=winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE,policy,0,winreg.KEY_QUERY_VALUE|winreg.KEY_SET_VALUE)
    original,kind=winreg.QueryValueEx(key,'ConsentPromptBehaviorAdmin')
    result=dict(outcome='failed',source_sha=os.environ['GITHUB_SHA'],run_id=os.environ['GITHUB_RUN_ID'],
                scope='Fresh disposable standard administrator with a filtered medium token; actual UAC secure-desktop rendering and Escape cancellation through production Adit SessionManager',
                account=account,public_state=str(public),original_consent_policy=original,
                limitations=['Prompt cancellation uses production SessionManager keysyms; does not pass native UI/physical keyboard hooks, IME or elevation approval.',
                             'No user password is read. Synthetic account password exists only in process memory, never argv, environment, files or logs.'])
    created=False
    process=PROCESSINFO()
    try:
        password='Ad1!'+secrets.token_hex(16)
        info=USER_INFO_1(account,password,0,1,None,'Owned disposable Adit CI UAC fixture',0x10201,None)
        parameter=W.DWORD()
        status=net.NetUserAdd(None,1,C.byref(info),C.byref(parameter))
        assert status==0, f'NetUserAdd failed {status}, parameter {parameter.value}'
        created=True
        member=W.LPWSTR(os.environ['COMPUTERNAME']+'\\'+account)
        assert net.NetLocalGroupAddMembers(None,'Administrators',3,C.byref(member),1)==0
        subprocess.run(['icacls',str(public),'/grant',account+':(OI)(CI)M'],check=True,stdout=subprocess.DEVNULL,timeout=10)
        helper=public/'medium.py'
        shutil.copyfile(__file__,helper)
        winreg.SetValueEx(key,'ConsentPromptBehaviorAdmin',0,kind,5)
        result['active_consent_policy']=winreg.QueryValueEx(key,'ConsentPromptBehaviorAdmin')[0]
        result['secure_desktop_policy']=winreg.QueryValueEx(key,'PromptOnSecureDesktop')[0]
        assert result['secure_desktop_policy']==1
        startup=STARTUPINFO()
        startup.cb=C.sizeof(startup)
        command=C.create_unicode_buffer(subprocess.list2cmdline([sys.executable,str(helper),'--medium','--results',str(public)]))
        inherited={name:os.environ[name] for name in ('SYSTEMROOT','WINDIR','SYSTEMDRIVE','PROGRAMDATA','PUBLIC','TEMP','TMP','PATH','PATHEXT','COMSPEC','RUNNER_ENVIRONMENT','RUNNER_OS') if name in os.environ}
        environment=C.create_unicode_buffer('\0'.join(f'{name}={value}' for name,value in sorted(inherited.items()))+'\0\0')
        assert adv.CreateProcessWithLogonW(account,'.',password,1,None,command,0x400,C.cast(environment,C.c_void_p),str(public),C.byref(startup),C.byref(process)), C.WinError(C.get_last_error())
        del password
        deadline=time.monotonic()+30
        while not (public/'medium-token.json').exists():
            assert time.monotonic()<deadline and kernel.WaitForSingleObject(process.process,0)==258, 'medium helper did not start'
            time.sleep(.1)
        result['medium_token']=json.loads((public/'medium-token.json').read_text(encoding='utf-8'))
        assert result['medium_token']['elevation_type']==3, 'fixture account did not receive a filtered token'
        # AppInfo launches consent.exe independently of the requesting process.
        deadline=time.monotonic()+20
        while True:
            consent=subprocess.check_output(['pwsh','-NoProfile','-Command',
                "Get-CimInstance Win32_Process -Filter \"Name='consent.exe'\" | Select-Object ProcessId,SessionId,ExecutablePath | ConvertTo-Json -Compress"],text=True).strip()
            if consent:
                result['consent']=json.loads(consent)
                break
            assert time.monotonic()<deadline and not (public/'prompt-result.json').exists(), 'no actual UAC consent process'
            time.sleep(.1)
        user=C.WinDLL('user32',use_last_error=True)
        user.OpenInputDesktop.argtypes=[W.DWORD,W.BOOL,W.DWORD]
        user.OpenInputDesktop.restype=W.HANDLE
        user.GetUserObjectInformationW.argtypes=[W.HANDLE,C.c_int,C.c_void_p,W.DWORD,C.POINTER(W.DWORD)]
        user.CloseDesktop.argtypes=[W.HANDLE]
        deadline=time.monotonic()+15
        while True:
            desktop=user.OpenInputDesktop(0,False,1)
            if not desktop:
                error=C.get_last_error()
                assert error==5, C.WinError(error)
                result['input_desktop']=dict(access_denied=True,error=error)
                break
            name=C.create_unicode_buffer(256)
            needed=W.DWORD()
            try:
                assert user.GetUserObjectInformationW(desktop,2,name,C.sizeof(name),C.byref(needed))
                if name.value.lower()!='default':
                    result['input_desktop']=dict(name=name.value)
                    break
            finally:
                user.CloseDesktop(desktop)
            assert time.monotonic()<deadline, 'UAC did not switch away from Default desktop'
            time.sleep(.1)
        snapshot=args.results/'uac.ppm'
        with (args.results/'probe.stdout.txt').open('w',encoding='utf-8') as out,(args.results/'probe.stderr.txt').open('w',encoding='utf-8') as err:
            completed=subprocess.run([str(args.probe.resolve()),'127.0.0.1','15911','20',str(snapshot.resolve()),'--steady-seconds=3'],
                                     input='adit-ci\n',text=True,stdout=out,stderr=err,timeout=30)
        stdout=(args.results/'probe.stdout.txt').read_text(encoding='utf-8')
        assert completed.returncode==0 and 'complete=true' in stdout and 'security: "VncAuth"' in stdout
        assert 'final_connected=true disconnects=0 connection_error=false steady_complete=true' in stdout
        result['snapshot_sha256']=hashlib.sha256(snapshot.read_bytes()).hexdigest()
        result['input_driver_sha256']=hashlib.sha256(args.input_driver.read_bytes()).hexdigest()
        with (args.results/'input.stdout.txt').open('w',encoding='utf-8') as out,(args.results/'input.stderr.txt').open('w',encoding='utf-8') as err:
            subprocess.run([str(args.input_driver.resolve()),'127.0.0.1','15911','20'],check=True,timeout=25,
                           input='adit-ci\nKEY ff1b 1\nWAIT 100\nKEY ff1b 0\nWAIT 500\nCLOSE\n',
                           text=True,stdout=out,stderr=err)
        result['cancellation_path']='production SessionManager VncAuth keysyms Escape down/up'
        deadline=time.monotonic()+15
        while not (public/'prompt-result.json').exists():
            assert time.monotonic()<deadline, 'UAC cancellation not acknowledged'
            time.sleep(.1)
        result['prompt_result']=json.loads((public/'prompt-result.json').read_text(encoding='utf-8'))
        assert result['prompt_result']['cancelled'], result['prompt_result']
        assert kernel.WaitForSingleObject(process.process,5000)==0
        result['outcome']='passed'
    except Exception as error:
        # Never include a native process's command line or an exception object
        # containing the CreateProcessWithLogon password argument.
        result['error']=f'{type(error).__name__}: {error}'
        raise
    finally:
        if process.process:
            if kernel.WaitForSingleObject(process.process,0)==258:
                kernel.TerminateProcess(process.process,1)
                kernel.WaitForSingleObject(process.process,5000)
            kernel.CloseHandle(process.thread)
            kernel.CloseHandle(process.process)
        winreg.SetValueEx(key,'ConsentPromptBehaviorAdmin',0,kind,original)
        result['restored_consent_policy']=winreg.QueryValueEx(key,'ConsentPromptBehaviorAdmin')[0]
        winreg.CloseKey(key)
        if created:
            result['account_delete_status']=net.NetUserDel(None,account)
            if result['account_delete_status']!=0:
                result['outcome']='failed'
        for file in ['medium-token.json','prompt-result.json']:
            if (public/file).exists():
                shutil.copyfile(public/file,args.results/file)
        write(args.results/'result.json',result)
        assert result.get('account_delete_status',0)==0
    return 0


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--medium',action='store_true')
    parser.add_argument('--probe',type=Path)
    parser.add_argument('--input-driver',type=Path)
    parser.add_argument('--results',type=Path,required=True)
    sys.exit(main(parser.parse_args()))
