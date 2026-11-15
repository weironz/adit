// In-session pointer oracle for #244. Runs *inside* the RDP user's session
// (started by an at-logon scheduled task) and records where Windows itself
// puts that session's pointer. Windows never paints the cursor into the
// framebuffer and never sends a coordinate-bearing pointer PDU, so the only
// server-side observable is the session's own GetCursorPos.
//
// Output (directory from argv[0], default C:\adit-oracle):
//   latest.json  - the newest sample, replaced atomically on every tick
//   samples.jsonl - one line per change of position or topology
//
// Build: csc /nologo /target:winexe /out:CursorLogger.exe CursorLogger.cs
//
// Start it in the test user's session with an at-logon task. An interactive
// token needs no stored password, and it puts the process in the RDP session
// itself rather than in session 0 (PowerShell, as an administrator):
//   $a = New-ScheduledTaskAction -Execute C:\adit-oracle\CursorLogger.exe -Argument C:\adit-oracle
//   $t = New-ScheduledTaskTrigger -AtLogOn -User aditptr
//   $p = New-ScheduledTaskPrincipal -UserId aditptr -LogonType Interactive
//   Register-ScheduledTask AditPointerOracle -Action $a -Trigger $t -Principal $p
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

static class CursorLogger
{
    [StructLayout(LayoutKind.Sequential)] struct POINT { public int X, Y; }
    [StructLayout(LayoutKind.Sequential)] struct RECT { public int L, T, R, B; }
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct MONITORINFOEX
    {
        public int cbSize; public RECT rcMonitor; public RECT rcWork; public int dwFlags;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string szDevice;
    }
    delegate bool MonitorEnumProc(IntPtr hMon, IntPtr hdc, ref RECT rc, IntPtr data);

    [DllImport("user32.dll")] static extern bool GetCursorPos(out POINT p);
    [DllImport("user32.dll")] static extern bool GetPhysicalCursorPos(out POINT p);
    [DllImport("user32.dll")] static extern int GetSystemMetrics(int i);
    [DllImport("user32.dll")] static extern bool EnumDisplayMonitors(IntPtr hdc, IntPtr clip, MonitorEnumProc cb, IntPtr data);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern bool GetMonitorInfo(IntPtr hMon, ref MONITORINFOEX mi);
    [DllImport("user32.dll")] static extern IntPtr SetProcessDpiAwarenessContext(IntPtr ctx);
    [DllImport("user32.dll")] static extern IntPtr SetThreadDpiAwarenessContext(IntPtr ctx);
    [DllImport("shcore.dll")] static extern int GetDpiForMonitor(IntPtr hMon, int type, out uint dx, out uint dy);

    static readonly IntPtr UNAWARE = new IntPtr(-1);
    static readonly IntPtr PER_MONITOR_AWARE_V2 = new IntPtr(-4);
    const int SM_XVIRTUALSCREEN = 76, SM_YVIRTUALSCREEN = 77, SM_CXVIRTUALSCREEN = 78,
              SM_CYVIRTUALSCREEN = 79, SM_CMONITORS = 80;

    static string Pt(bool ok, POINT p) { return ok ? "[" + p.X + "," + p.Y + "]" : "null"; }

    static string Topology()
    {
        var sb = new StringBuilder();
        sb.Append("\"virtual\":[").Append(GetSystemMetrics(SM_XVIRTUALSCREEN)).Append(',')
          .Append(GetSystemMetrics(SM_YVIRTUALSCREEN)).Append(',')
          .Append(GetSystemMetrics(SM_CXVIRTUALSCREEN)).Append(',')
          .Append(GetSystemMetrics(SM_CYVIRTUALSCREEN)).Append("],\"monitor_count\":")
          .Append(GetSystemMetrics(SM_CMONITORS)).Append(",\"monitors\":[");
        var parts = new List<string>();
        EnumDisplayMonitors(IntPtr.Zero, IntPtr.Zero, delegate (IntPtr h, IntPtr hdc, ref RECT rc, IntPtr d)
        {
            var mi = new MONITORINFOEX(); mi.cbSize = Marshal.SizeOf(typeof(MONITORINFOEX));
            GetMonitorInfo(h, ref mi);
            uint dx = 0, dy = 0; GetDpiForMonitor(h, 0, out dx, out dy);
            parts.Add("{\"device\":\"" + mi.szDevice.Replace("\\", "\\\\") + "\",\"rect\":[" +
                mi.rcMonitor.L + "," + mi.rcMonitor.T + "," + mi.rcMonitor.R + "," + mi.rcMonitor.B +
                "],\"primary\":" + ((mi.dwFlags & 1) != 0 ? "true" : "false") + ",\"dpi\":" + dx + "}");
            return true;
        }, IntPtr.Zero);
        sb.Append(string.Join(",", parts.ToArray())).Append(']');
        return sb.ToString();
    }

    static void Main(string[] args)
    {
        // Per-monitor aware, so GetCursorPos returns physical desktop pixels:
        // the same units the RDP wire uses. The DPI-unaware reading is taken
        // separately below, because that is what an unaware app would see.
        SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2);
        string dir = args.Length > 0 ? args[0] : @"C:\adit-oracle";
        Directory.CreateDirectory(dir);
        string latest = Path.Combine(dir, "latest.json"), tmp = latest + ".tmp";
        string log = Path.Combine(dir, "samples.jsonl");
        int session = Process.GetCurrentProcess().SessionId;
        string user = Environment.UserName, last = null;
        long seq = 0;
        File.AppendAllText(log, "{\"kind\":\"start\",\"session\":" + session + ",\"user\":\"" + user +
            "\",\"utc\":\"" + DateTime.UtcNow.ToString("o") + "\"}\n");
        while (true)
        {
            POINT a, ph, un;
            bool okA = GetCursorPos(out a), okP = GetPhysicalCursorPos(out ph);
            IntPtr prev = SetThreadDpiAwarenessContext(UNAWARE);
            bool okU = GetCursorPos(out un);
            SetThreadDpiAwarenessContext(prev);
            string body = "\"cursor\":" + Pt(okA, a) + ",\"physical\":" + Pt(okP, ph) +
                ",\"unaware\":" + Pt(okU, un) + "," + Topology();
            seq++;
            string line = "{\"kind\":\"sample\",\"seq\":" + seq + ",\"session\":" + session +
                ",\"utc\":\"" + DateTime.UtcNow.ToString("o") + "\"," + body + "}";
            try
            {
                File.WriteAllText(tmp, line + "\n");
                if (File.Exists(latest)) File.Replace(tmp, latest, null); else File.Move(tmp, latest);
                if (body != last) { File.AppendAllText(log, line + "\n"); last = body; }
            }
            catch (IOException) { }
            catch (UnauthorizedAccessException) { }
            Thread.Sleep(10);
        }
    }
}
