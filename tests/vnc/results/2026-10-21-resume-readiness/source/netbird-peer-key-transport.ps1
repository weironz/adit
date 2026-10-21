# Read-only NetBird 0.81.2 public peer-key RPC transport.
# Match the official protected-pipe trust boundary and caller identification.
# This transport does not open a TCP fallback or read authentication files.
$ErrorActionPreference = 'Stop'
Add-Type @'
using System;
using System.IO;
using System.IO.Pipes;
using System.Security.Principal;
using System.Threading.Tasks;
public static class AditPeerKeyPipe {
    public static void Run() {
        using (var pipe = new NamedPipeClientStream(
            ".", @"ProtectedPrefix\Administrators\netbird",
            PipeDirection.InOut, PipeOptions.Asynchronous,
            TokenImpersonationLevel.Identification)) {
            pipe.Connect(3000);
            var fromDaemon = pipe.CopyToAsync(Console.OpenStandardOutput());
            var toDaemon = Console.OpenStandardInput().CopyToAsync(pipe);
            Task.WhenAny(fromDaemon, toDaemon, Task.Delay(12000)).GetAwaiter().GetResult();
        }
    }
}
'@
[AditPeerKeyPipe]::Run()
