param(
    [Parameter(Mandatory)][string]$Directory,
    [ValidatePattern('^[a-z0-9-]{1,64}$')][string]$ResultName = 'clearcodec-pattern'
)
$ErrorActionPreference = 'Stop'
$root = [IO.Path]::GetFullPath($Directory).TrimEnd('\')
$name = Split-Path $root -Leaf
if ($PSVersionTable.PSEdition -ne 'Desktop' -or $PSVersionTable.PSVersion.Major -ne 5 -or
    -not [Environment]::Is64BitProcess -or [Diagnostics.Process]::GetCurrentProcess().SessionId -eq 0 -or
    $root -notmatch '^[a-zA-Z]:\\' -or $name -notmatch '^AditVncKeys-[a-f0-9]{32}$') {
    throw 'Requires a local owned GUID fixture and interactive 64-bit Windows PowerShell 5.1'
}
function Assert-NoReparse([string]$Path) {
    $current = $Path
    while ($current) {
        $item = Get-Item -LiteralPath $current -Force
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw 'Reparse paths are not an owned payload boundary'
        }
        $parent = [IO.Path]::GetDirectoryName($current)
        if ($parent -eq $current) { break }
        $current = $parent
    }
}
Assert-NoReparse $root
$utf8 = New-Object Text.UTF8Encoding($false)
$ownerPath = Join-Path $root 'owner.json'
Assert-NoReparse $ownerPath
$owner = [IO.File]::ReadAllText($ownerPath, $utf8) | ConvertFrom-Json
if ($owner.name -cne $name -or
    -not [string]::Equals([IO.Path]::GetFullPath([string]$owner.directory).TrimEnd('\'), $root, [StringComparison]::OrdinalIgnoreCase) -or
    -not [string]::Equals([string]$owner.computer, [Environment]::MachineName, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'owner.json name, directory and computer must match this owned fixture'
}
$payload = Join-Path $root 'payload'
if (-not [IO.Directory]::Exists($payload)) { throw 'Owned payload directory must already exist' }
Assert-NoReparse $payload
$result = Join-Path $payload $ResultName
if (Test-Path -LiteralPath $result) { throw 'Use a fresh ResultName; previous evidence is retained' }
[void](New-Item -ItemType Directory -Path $result -ErrorAction Stop)
Assert-NoReparse $result
$exe = Join-Path $result 'synthetic-clearcodec-pattern.exe'
$sourcePath = Join-Path $result 'synthetic-clearcodec-pattern.cs'
$reportPath = Join-Path $result 'controller-result.json'
$record = [ordered]@{
    outcome = 'failed'
    scope = 'Synthetic native GDI paint only; ClearCodec cache coverage requires separate real capture replay'
    started_utc = [DateTime]::UtcNow.ToString('o')
    owner_name = $name
    duration_seconds = 60
    interval_ms = 350
    process_identity = $null
    timed_out = $false
    owned_process_stopped = $false
    codec_coverage_claimed = $false
}
$target = $null
$identity = $null
function Save-Report {
    $temporary = $reportPath + '.' + [guid]::NewGuid().ToString('N') + '.tmp'
    try {
        [IO.File]::WriteAllText($temporary, ($record | ConvertTo-Json -Depth 8), $utf8)
        if ([IO.File]::Exists($reportPath)) { [IO.File]::Replace($temporary, $reportPath, $null) }
        else { [IO.File]::Move($temporary, $reportPath) }
    } finally {
        if ([IO.File]::Exists($temporary)) { [IO.File]::Delete($temporary) }
    }
}
function Stop-ExactOwnedProcess {
    if (-not $target -or $target.HasExited) { return }
    if (-not $identity -or $identity.id -ne $target.Id -or
        $identity.start_ticks -ne $target.StartTime.ToUniversalTime().Ticks -or
        $identity.session_id -ne $target.SessionId -or
        -not [string]::Equals($target.MainModule.FileName, $exe, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Owned process identity changed; refusing termination'
    }
    # Use the Process.Start handle, never a process-name search or a new PID lookup.
    $target.Kill()
    if (-not $target.WaitForExit(5000)) { throw 'Exact owned paint process did not exit' }
    $record.owned_process_stopped = $true
}
$source = @'
using System;
using System.Diagnostics;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.Drawing.Imaging;
using System.Globalization;
using System.IO;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
using System.Windows.Forms;

public sealed class AditClearPattern : Form {
    const int VectorCount = 12;
    const int Columns = 8;
    const int Rows = 6;
    const int CellEdge = 48;
    const int DurationMs = 60000;
    readonly int[,] shapes = { {1,32}, {2,16}, {4,8}, {8,4}, {16,2}, {32,1} };
    readonly Bitmap[,] bitmaps = new Bitmap[VectorCount,6];
    readonly string[] hashes = new string[VectorCount];
    readonly string directory;
    readonly StreamWriter paints;
    readonly Stopwatch clock = new Stopwatch();
    readonly System.Windows.Forms.Timer timer = new System.Windows.Forms.Timer();
    long paintSequence;
    long drawCount;
    int step;
    bool completed;

    [DllImport("user32.dll")] static extern bool SetProcessDPIAware();
    [DllImport("gdi32.dll")] static extern bool GdiFlush();

    static string Hex(byte[] bytes) {
        return BitConverter.ToString(bytes).Replace("-", "").ToLowerInvariant();
    }
    static string Hash(byte[] bytes) {
        using (var sha = SHA256.Create()) { return Hex(sha.ComputeHash(bytes)); }
    }
    // Twelve public vectors. A vector's 128 bytes never change when reshaped.
    static byte[] Vector(int id) {
        var bytes = new byte[32 * 4];
        for (int pixel = 0; pixel < 32; pixel++) {
            bytes[pixel * 4] = (byte)((pixel * 17 + id * 29) & 255);
            bytes[pixel * 4 + 1] = (byte)((pixel * 37 + id * 11) & 255);
            bytes[pixel * 4 + 2] = (byte)((pixel * 13 + id * 47) & 255);
            bytes[pixel * 4 + 3] = 255;
        }
        return bytes;
    }
    static Bitmap MakeBitmap(byte[] vector, int width, int height, string expectedHash) {
        var bitmap = new Bitmap(width, height, PixelFormat.Format32bppArgb);
        try {
            var data = bitmap.LockBits(new Rectangle(0, 0, width, height),
                ImageLockMode.ReadWrite, PixelFormat.Format32bppArgb);
            try {
                var actual = new byte[vector.Length];
                for (int row = 0; row < height; row++) {
                    IntPtr address = IntPtr.Add(data.Scan0, row * data.Stride);
                    Marshal.Copy(vector, row * width * 4, address, width * 4);
                    Marshal.Copy(address, actual, row * width * 4, width * 4);
                }
                if (Hash(actual) != expectedHash) {
                    throw new InvalidDataException("Synthetic reshape changed linear BGRA bytes");
                }
            } finally { bitmap.UnlockBits(data); }
            return bitmap;
        } catch { bitmap.Dispose(); throw; }
    }
    public AditClearPattern(string output) {
        directory = output;
        Text = "ADIT synthetic ClearCodec pattern";
        AutoScaleMode = AutoScaleMode.None;
        ClientSize = new Size(Columns * CellEdge, Rows * CellEdge);
        FormBorderStyle = FormBorderStyle.FixedToolWindow;
        MaximizeBox = false;
        MinimizeBox = false;
        ShowInTaskbar = false;
        StartPosition = FormStartPosition.CenterScreen;
        TopMost = true;
        DoubleBuffered = false;
        BackColor = Color.FromArgb(255, 32, 32, 32);
        SetStyle(ControlStyles.AllPaintingInWmPaint | ControlStyles.UserPaint, true);
        for (int id = 0; id < VectorCount; id++) {
            byte[] bytes = Vector(id);
            hashes[id] = Hash(bytes);
            for (int shape = 0; shape < 6; shape++) {
                bitmaps[id, shape] = MakeBitmap(bytes, shapes[shape, 0], shapes[shape, 1], hashes[id]);
            }
        }
        WriteVectorManifest();
        paints = new StreamWriter(new FileStream(Path.Combine(directory, "paint.jsonl"),
            FileMode.CreateNew, FileAccess.Write, FileShare.Read), new UTF8Encoding(false));
        timer.Interval = 350;
        timer.Tick += delegate {
            if (clock.ElapsedMilliseconds >= DurationMs) { completed = true; Close(); return; }
            step++;
            Invalidate();
            Update();
        };
    }
    void WriteVectorManifest() {
        var text = new StringBuilder();
        text.Append("{\"schema\":1,\"pixels_per_vector\":32,\"pixel_order\":\"linear_BGRA\",");
        text.Append("\"alpha\":255,\"vector_count\":12,\"interval_ms\":350,\"duration_ms\":60000,");
        text.Append("\"generator\":\"B=(p*17+id*29)%256;G=(p*37+id*11)%256;R=(p*13+id*47)%256;A=255\",");
        text.Append("\"shapes\":[[1,32],[2,16],[4,8],[8,4],[16,2],[32,1]],\"vectors\":[");
        for (int id = 0; id < VectorCount; id++) {
            if (id != 0) text.Append(",");
            text.Append("{\"vector_id\":").Append(id).Append(",\"sha256\":\"").Append(hashes[id]).Append("\"}");
        }
        text.Append("],\"all_bitmap_linear_hashes_verified\":true,\"codec_coverage_claimed\":false}");
        using (var stream = new StreamWriter(new FileStream(Path.Combine(directory, "vectors.json"),
            FileMode.CreateNew, FileAccess.Write, FileShare.Read), new UTF8Encoding(false))) {
            stream.Write(text.ToString());
        }
    }
    protected override void OnShown(EventArgs args) {
        base.OnShown(args);
        clock.Start();
        timer.Start();
        Invalidate();
        Update();
    }
    protected override void OnPaint(PaintEventArgs args) {
        base.OnPaint(args);
        if (paints == null || !clock.IsRunning || completed) return;
        long sequence = ++paintSequence;
        args.Graphics.CompositingMode = CompositingMode.SourceCopy;
        args.Graphics.Clear(BackColor);
        for (int cell = 0; cell < Columns * Rows; cell++) {
            int id = (cell + step) % VectorCount;
            int shape = (step + id) % 6;
            int x = (cell % Columns) * CellEdge + 8;
            int y = (cell / Columns) * CellEdge + 8;
            // No scaling, text or desktop readback: draw the verified BGRA bitmap.
            args.Graphics.DrawImageUnscaled(bitmaps[id, shape], x, y);
            args.Graphics.Flush(FlushIntention.Sync);
            if (!GdiFlush()) throw new InvalidOperationException("Synthetic GDI flush failed");
            drawCount++;
            paints.WriteLine("{\"utc\":\"" + DateTime.UtcNow.ToString("o", CultureInfo.InvariantCulture)
                + "\",\"seq\":" + sequence + ",\"step\":" + step + ",\"cell\":" + cell
                + ",\"vector_id\":" + id + ",\"width\":" + shapes[shape, 0]
                + ",\"height\":" + shapes[shape, 1] + ",\"sha256\":\"" + hashes[id]
                + "\",\"linear_hash_verified\":true,\"draw\":\"DrawImageUnscaled\"}");
        }
        paints.Flush();
    }
    public void WriteResult() {
        string value = "{\"outcome\":\"" + (completed ? "completed" : "closed_early")
            + "\",\"elapsed_ms\":" + clock.ElapsedMilliseconds
            + ",\"paint_count\":" + paintSequence + ",\"draw_count\":" + drawCount
            + ",\"last_step\":" + step + ",\"codec_coverage_claimed\":false}";
        using (var stream = new StreamWriter(new FileStream(Path.Combine(directory, "window-result.json"),
            FileMode.CreateNew, FileAccess.Write, FileShare.Read), new UTF8Encoding(false))) {
            stream.Write(value);
        }
    }
    protected override void Dispose(bool disposing) {
        if (disposing) {
            timer.Stop();
            timer.Dispose();
            clock.Stop();
            if (paints != null) paints.Dispose();
            foreach (Bitmap bitmap in bitmaps) { if (bitmap != null) bitmap.Dispose(); }
        }
        base.Dispose(disposing);
    }
    [STAThread]
    public static int Main(string[] args) {
        if (args.Length != 1 || !Directory.Exists(args[0]) ||
            !String.Equals(Path.GetFullPath(args[0]).TrimEnd(Path.DirectorySeparatorChar),
                Path.GetDirectoryName(Application.ExecutablePath), StringComparison.OrdinalIgnoreCase)) return 2;
        try {
            SetProcessDPIAware();
            Application.SetUnhandledExceptionMode(UnhandledExceptionMode.ThrowException);
            Application.EnableVisualStyles();
            using (var form = new AditClearPattern(args[0])) {
                Application.Run(form);
                form.WriteResult();
                return form.completed ? 0 : 3;
            }
        } catch (Exception error) {
            // Exception type is diagnostic metadata; never log arbitrary desktop text.
            File.WriteAllText(Path.Combine(args[0], "window-error.json"),
                "{\"outcome\":\"failed\",\"error_type\":\"" + error.GetType().FullName + "\"}",
                new UTF8Encoding(false));
            return 1;
        }
    }
}
'@
try {
    Save-Report
    [IO.File]::WriteAllText($sourcePath, $source, $utf8)
    $compiler = New-Object Microsoft.CSharp.CSharpCodeProvider
    $parameters = New-Object CodeDom.Compiler.CompilerParameters
    $parameters.GenerateExecutable = $true
    $parameters.GenerateInMemory = $false
    $parameters.OutputAssembly = $exe
    $parameters.CompilerOptions = '/target:winexe /platform:x64'
    $parameters.TreatWarningsAsErrors = $true
    $parameters.TempFiles = New-Object CodeDom.Compiler.TempFileCollection($result, $false)
    [void]$parameters.ReferencedAssemblies.Add('System.dll')
    [void]$parameters.ReferencedAssemblies.Add('System.Drawing.dll')
    [void]$parameters.ReferencedAssemblies.Add('System.Windows.Forms.dll')
    try {
        $compiled = $compiler.CompileAssemblyFromSource($parameters, $source)
        if ($compiled.Errors.HasErrors) {
            $record.compiler_errors = @($compiled.Errors | Select-Object ErrorNumber, Line, Column)
            throw 'Synthetic C# compilation failed'
        }
    } finally {
        $compiler.Dispose()
        $parameters.TempFiles.Dispose()
    }
    $record.exe_sha256 = (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLowerInvariant()
    $start = New-Object Diagnostics.ProcessStartInfo
    $start.FileName = $exe
    $start.Arguments = '"' + $result + '"'
    $start.WorkingDirectory = $result
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $target = [Diagnostics.Process]::Start($start)
    # Keep the original kernel process handle for exact-PID timeout cleanup.
    $null = $target.SafeHandle
    $identity = [ordered]@{
        id = $target.Id
        start_ticks = $target.StartTime.ToUniversalTime().Ticks
        path = $target.MainModule.FileName
        session_id = $target.SessionId
    }
    if (-not [string]::Equals($identity.path, $exe, [StringComparison]::OrdinalIgnoreCase) -or
        $identity.session_id -ne [Diagnostics.Process]::GetCurrentProcess().SessionId) {
        throw 'New paint process identity does not match the owned executable and session'
    }
    $record.process_identity = $identity
    Save-Report
    if (-not $target.WaitForExit(80000)) {
        $record.timed_out = $true
        Stop-ExactOwnedProcess
        throw 'Synthetic paint window exceeded its 60-second run and controller grace period'
    }
    $record.exit_code = $target.ExitCode
    $windowPath = Join-Path $result 'window-result.json'
    if ($target.ExitCode -ne 0 -or -not [IO.File]::Exists($windowPath)) { throw 'Synthetic paint process did not complete' }
    $window = [IO.File]::ReadAllText($windowPath, $utf8) | ConvertFrom-Json
    if ($window.outcome -ne 'completed' -or $window.elapsed_ms -lt 60000 -or
        $window.paint_count -le 0 -or $window.draw_count -ne 48 * $window.paint_count) {
        throw 'Native paint result is incomplete'
    }
    $record.native_paint = $window
    $record.outcome = 'completed_native_paint_only'
} catch {
    $record.error_type = $_.Exception.GetType().FullName
    throw
} finally {
    try { Stop-ExactOwnedProcess }
    finally {
        $record.finished_utc = [DateTime]::UtcNow.ToString('o')
        Save-Report
        if ($target) { $target.Dispose() }
    }
}
