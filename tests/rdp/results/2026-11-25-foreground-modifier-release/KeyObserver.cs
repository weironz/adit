using System;
using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Windows.Forms;

internal static class KeyObserver
{
    [StructLayout(LayoutKind.Sequential)]
    private struct KeyboardEvent
    {
        internal uint Vk, Scan, Flags, Time;
        internal UIntPtr ExtraInfo;
    }

    private delegate IntPtr Hook(int code, IntPtr message, IntPtr data);
    private static readonly Hook Callback = OnKey;
    private static readonly int[] Modifiers = { 0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0x5b, 0x5c };
    private static StreamWriter log;
    private static IntPtr lastWindow;
    private static int lastMask = -1;

    [DllImport("user32.dll", SetLastError = true)]
    private static extern IntPtr SetWindowsHookEx(int kind, Hook hook, IntPtr module, uint thread);
    [DllImport("user32.dll")]
    private static extern bool UnhookWindowsHookEx(IntPtr hook);
    [DllImport("user32.dll")]
    private static extern IntPtr CallNextHookEx(IntPtr hook, int code, IntPtr message, IntPtr data);
    [DllImport("kernel32.dll")]
    private static extern IntPtr GetModuleHandle(string module);
    [DllImport("user32.dll")]
    private static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")]
    private static extern uint GetWindowThreadProcessId(IntPtr window, out int pid);
    [DllImport("user32.dll")]
    private static extern short GetAsyncKeyState(int vk);
    [DllImport("user32.dll")]
    private static extern int GetSystemMetrics(int index);

    private static void Write(string line)
    {
        log.WriteLine(DateTime.UtcNow.ToString("O") + " " + line);
    }

    private static IntPtr OnKey(int code, IntPtr message, IntPtr data)
    {
        if (code == 0)
        {
            var e = (KeyboardEvent)Marshal.PtrToStructure(data, typeof(KeyboardEvent));
            // Test keys only; never record text, window titles, or arbitrary keystrokes.
            if (Array.IndexOf(Modifiers, (int)e.Vk) >= 0 || e.Vk == 0x09 ||
                e.Vk == 0x1b || e.Vk == 0x10 || e.Vk == 0x11 || e.Vk == 0x12)
                Write(string.Format("key vk={0:X2} scan={1:X2} flags={2:X2} edge={3} injected={4}",
                    e.Vk, e.Scan, e.Flags, (e.Flags & 0x80) == 0 ? "down" : "up",
                    (e.Flags & 0x10) != 0));
        }
        return CallNextHookEx(IntPtr.Zero, code, message, data);
    }

    private static void Sample(object sender, EventArgs args)
    {
        var window = GetForegroundWindow();
        if (window != lastWindow)
        {
            int pid;
            GetWindowThreadProcessId(window, out pid);
            string name = "-";
            try { using (var p = Process.GetProcessById(pid)) { name = p.ProcessName; } }
            catch (ArgumentException) { }
            Write(string.Format("foreground hwnd={0:X} pid={1} process={2}", window.ToInt64(), pid, name));
            lastWindow = window;
        }
        var mask = 0;
        for (int i = 0; i < Modifiers.Length; i++)
            if ((GetAsyncKeyState(Modifiers[i]) & 0x8000) != 0)
                mask |= 1 << i;
        if (mask != lastMask)
        {
            Write(string.Format("async mask={0:X2}", mask));
            lastMask = mask;
        }
    }

    [STAThread]
    private static void Main(string[] args)
    {
        using (log = new StreamWriter(args[0], true) { AutoFlush = true })
        using (var timer = new Timer { Interval = 15 })
        {
            var hook = SetWindowsHookEx(13, Callback, GetModuleHandle(null), 0);
            if (hook == IntPtr.Zero)
                throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
            try
            {
                Write(string.Format("ready session={0} remote={1} user={2} pid={3}",
                    Process.GetCurrentProcess().SessionId, GetSystemMetrics(0x1000),
                    Environment.UserName, Process.GetCurrentProcess().Id));
                timer.Tick += Sample;
                timer.Start();
                Application.Run();
            }
            finally { UnhookWindowsHookEx(hook); }
        }
    }
}
