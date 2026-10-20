import ctypes as C, ctypes.wintypes as W, json, threading, uuid
u=C.WinDLL('user32',use_last_error=True); k=C.WinDLL('kernel32',use_last_error=True)
u.GetKeyboardLayoutList.argtypes=[C.c_int,C.POINTER(C.c_void_p)];u.GetKeyboardLayoutList.restype=C.c_int
u.ToUnicodeEx.argtypes=[C.c_uint,C.c_uint,C.POINTER(C.c_ubyte),C.POINTER(C.c_ushort),C.c_int,C.c_uint,C.c_void_p];u.ToUnicodeEx.restype=C.c_int
u.GetThreadDesktop.argtypes=[W.DWORD];u.GetThreadDesktop.restype=C.c_void_p
u.CreateDesktopW.argtypes=[W.LPCWSTR,C.c_void_p,C.c_void_p,W.DWORD,W.DWORD,C.c_void_p];u.CreateDesktopW.restype=C.c_void_p
u.SetThreadDesktop.argtypes=[C.c_void_p];u.SetThreadDesktop.restype=W.BOOL
u.CloseDesktop.argtypes=[C.c_void_p];u.CloseDesktop.restype=W.BOOL
u.PeekMessageW.argtypes=[C.c_void_p,C.c_void_p,W.UINT,W.UINT,W.UINT];u.PeekMessageW.restype=W.BOOL
k.GetCurrentThreadId.restype=W.DWORD
layouts=(C.c_void_p*64)();n=u.GetKeyboardLayoutList(64,layouts)
hkl=next((x for x in layouts[:n] if x & 0xffffffff==0x04070407),None)
if not hkl: raise RuntimeError('preloaded German required')
def translate(vk,scan,flags):
 state=(C.c_ubyte*256)();text=(C.c_ushort*16)();r=u.ToUnicodeEx(vk,scan,state,text,16,flags,hkl)
 return {'return':r,'units':list(text)[:abs(r)]}
def queue(): u.PeekMessageW(C.create_string_buffer(64),None,0,0,0)
ready=threading.Event();go=threading.Event();done=threading.Event();result={'scope':'private desktop translation isolation; no GUI activation','layouts':list(layouts[:n])}
def worker():
 original=u.GetThreadDesktop(k.GetCurrentThreadId());private=None
 try:
  private=u.CreateDesktopW('AditVncTranslate-'+uuid.uuid4().hex,None,None,0,0x83,None)
  if not private: raise C.WinError(C.get_last_error())
  if not u.SetThreadDesktop(private): raise C.WinError(C.get_last_error())
  queue();result['worker_private']=u.GetThreadDesktop(k.GetCurrentThreadId())==private
  ready.set()
  if not go.wait(5): raise RuntimeError('start deadline')
  result['worker_r']=translate(0x52,0x13,4);result['worker_e']=translate(0x45,0x12,4)
 except BaseException as e: result['worker_error']=str(e)
 finally:
  if private:
   result['restored']=bool(u.SetThreadDesktop(original));result['closed']=bool(u.CloseDesktop(private))
  ready.set();done.set()
def composition():
 queue()
 if not ready.wait(5): result['composition_error']='worker deadline';return
 if result.get('worker_error'): return
 result['acute']=translate(0xdd,0x0d,0)
 go.set()
 try:
  if not done.wait(5): result['composition_error']='translation deadline'
 finally: result['composition_e']=translate(0x45,0x12,0)
b=threading.Thread(target=worker);a=threading.Thread(target=composition);b.start();a.start();a.join(8);b.join(8)
result['threads_exited']=not a.is_alive() and not b.is_alive()
result['passed']=result.get('worker_r')=={'return':1,'units':[114]} and result.get('worker_e')=={'return':1,'units':[101]} and result.get('composition_e')=={'return':1,'units':[233]} and result.get('acute',{}).get('return',0)<0 and result.get('restored') and result.get('closed') and result['threads_exited']
print(json.dumps(result))
raise SystemExit(0 if result['passed'] else 1)
