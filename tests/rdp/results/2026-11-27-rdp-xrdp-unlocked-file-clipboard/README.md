# xrdp 无锁文件剪贴板：真实服务端验收（#93）

日期：2026-11-27。服务端：本机 xrdp 0.9.17-2ubuntu3.1 + xorgxrdp 0.2.17（Ubuntu 22.04）。
客户端：真实 `adit-rdp-host`（helper）+ `rdp_probe` 的 `PULL` 命令，走 helper IPC 的
`RequestFileContents` 路径。

## 背景

xrdp 发文件列表时不带剪贴板锁（`clipDataId`），helper 以前整批拒收。现在接收，
但无锁时无法钉住文件：

- 每个 RANGE 回来的字节数必须和公布大小推出的长度完全一致；
- 最后一块多要 1 字节（vendored `ironrdp-cliprdr` 第 1 处 ADIT PATCH 放行），
  文件变长时会多回 1 字节，读取失败；
- 问 FILECONTENTS_SIZE 没用：xrdp 的 `clipboard_send_file_size` 直接返回复制时缓存的
  `cfi->size`（上一版用 SIZE 查询，变长场景实测 `pull_done`，没拦住）；
- 远端换剪贴板（新 FormatList）作废旧 offer；大小不变、内容被替换检测不到；复制时为空、之后变长的文件也检测不到（空文件没有可读的块，按空文件交付）。

## 方法

`drive.py` 以会话用户身份在 xrdp 会话的 X 剪贴板上用 `xclip` 提供
`x-special/gnome-copied-files`，再让 probe `PULL`。密码只经 stdin 传给 probe，
不进 argv、环境变量和日志。

xrdp 0.9 不支持 CredSSP，而发布版 helper 只发起 HYBRID，所以测试用的 helper 是
本地临时打开 TLS 编出来的（`enable_tls: true`），该改动没有提交；剪贴板代码和提交的一致。

## 结果（`results.txt`、`probe.txt`）

| 场景 | 预期 | 结果 |
|---|---|---|
| 不变：200001 B（4 块）、恰好 65536 B、空文件 | `pull_done`，SHA-256 一致 | 通过 |
| 复制后变长（150000 B → 追加） | 最后一块 `file_read_failed`，不落盘 | 通过（offset=131072） |
| 复制后截短（150000 B → 70000 B） | `file_read_failed`，不落盘 | 通过（offset=65536） |

单测（合成，不是真实服务端）另覆盖：读取中途远端换剪贴板失败、有锁 offer 仍带
`clipDataId` 且不多读、vendored crate 只放行无锁且恰好多 1 字节的请求。

## 没测到

- Windows Server 作服务端（有锁，走原路径）。
- xrdp → macOS Finder / Windows Explorer 的 GUI 端到端。
