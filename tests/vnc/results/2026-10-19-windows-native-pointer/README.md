# Windows native pointer observation, 2026-10-19

This is a partial diagnostic observation of the production view/input path in
an isolated synthetic window. It does not change coordinate mapping or loosen
the acceptance threshold, and does not close #207 or #213.

The binary came from `ee3ab5157bcb19a1be2f2e491605b516c02a1d12`, SHA256
`a7e0d4ad61aa029fb0e068beb2094f5efb39b001b190d665caf95900cd7ddc89`.
The exact observation and fixture sources are under `source/`. This source is
not the main merge commit or a released binary. Actual Windows DPI stayed at
120 (125%); application scale was 100%.

| Stable frame | Screenshot click | Windows cursor | Client coordinate | Peer received |
|---|---|---|---|---|
| 2, 1352x836 | 256,253 | 519,508 | 297.466278,200.140900 | 297,200 |
| 2, 1352x836 | 258,253 | 522,508 | 300.460907,200.140900 | 300,200 |
| 3, 2220x1196 | 256,245 | 320,306 | 300,198.374969 | 300,198 |
| 4, 1352x836 | 258,253 | 522,508 | 300.460907,200.140900 | 300,200 |

The old 3-pixel outlier was reproduced. The peer matches integer truncation of
the client coordinate; adjacent injected clicks change actual Windows position
and remote position by corresponding amounts. This does not establish the exact
cause of screenshot-to-input discrepancy. Snapshots read current native cursor
and own-window geometry asynchronously, rather than the original OS event.
For example, sequence 9 completed 100 ms after the request. JPEG sampling,
logical screenshot coordinates and injected input precision remain limitations.
The fixture sent no cursor shape, so these samples do not test cursor hotspots.

The peer sent four complete frames across explicit resize, native maximize and
restore. The two retained JPEGs show stable frame 2 and frame 4; they are exact
original bytes, not converted or edited. No claim covers every transient frame.
The final border-drag attempt produced canvas input with no native/remote
size change. Rapid border resize remains unverified.

The owned GUI exited normally with code 0; the fixture exited after its stop
request. A separate `Get-NetTCPConnection` snapshot found zero LISTEN sockets
on port 15915. The earlier `connect_ex=10035` was inconclusive and is explicitly
not used as listener-absence proof. No force termination, OS DPI change or
ordinary profile/credential access was needed. Actual 100/150/200% OS DPI,
physical keyboard/IME, and full cursor/focus acceptance remain outstanding.

`manifest.json` hashes every original evidence/source byte and the generated
audit. This README is explanatory and is not an original captured artifact.
