# #93：PR #258 无锁 xrdp 文件剪贴板复测

## 环境和方法

本轮均为**真实 xrdp → 生产 macOS GUI → 真实访达**，没有用合成 IPC 或回包夹具。macOS arm64 主机通过 QEMU 中的 Debian 13 arm64 连接 xrdp `0.10.1-3.1+deb13u2` 和 xorgxrdp `1:0.10.2-1`。VM 重启后启动了 xrdp 和 sesman；沿用 VM 内的临时 TLS、自签证书及禁用 `drdynvc` 的配置。TLS 1.3 和 CLIPRDR 已连接，服务端协商了 `STREAM_FILECLIP_ENABLED`，未协商剪贴板锁。禁用 GFX 后远端画面有失真，但 Nautilus 可操作；本地访达不受影响。

- #257 基线：`a435f550861e031d12bc9c3fcb30b79a75f6a7c2`。
- #258 测试提交：`3a859aee6ce67699ae432540fe82909f98c55a2f`。
- GUI 和 helper 均由独立 worktree 的本地合并提交 `2eb0c9f12819be65a6737b0ec15dbdef40ee1329` 构建，仅 helper 连接参数在该 worktree 临时改成 TLS，不提交。#257 分支不包含 #258 代码。[构建、临时差异及二进制 SHA-256](build.txt)
- 在远端 Nautilus 选择测试文件，按 Ctrl+C 后等 helper 收到 `FileGroupDescriptorW`，再从 Adit 的「远端文件拖出」拖到空访达目录。变更测试在收到描述后，通过 SSH 就地追加或截短同一个源文件，不再复制；放下后用 `find` 检查全部目标条目（包含隐藏暂存）。[两侧散列、源文件变更时间和目标清单](files.txt) · [xrdp/chansrv 与 helper 日志](protocol.txt)

## 结果

| 场景 | 实测结果 |
| --- | --- |
| 单文件 | **通过。** `one.txt`（34 字节）与 Debian 源文件 SHA-256 一致。[访达截图](01-single.png) |
| 多文件 | **通过。** 一次拖放 `large.bin`（131,083 字节）和 `small.txt`（34 字节），两份 SHA-256 均匹配。`large.bin` 三块回包为 65,536、65,536、11 字节，作为变更测试的正常对照。[访达截图](02-multi.png) |
| 目录（包含空文件） | **整目录不支持。** 两次复制 `Folder`，xrdp chansrv 都报 `clipboard_get_file: file […] is a directory, not supported`；helper 两次收到 `file_count=0`，没有可拖项目，也没有目标可比对。[目录复制后无文件承诺](03-directory.png)。单独进入 `Folder/inner` 复制两个文件则**通过**：`nested.txt` 与 `empty.txt` 的 SHA-256 均匹配，空文件无范围请求。这不是整目录传输。[单独拖入目录内的两个文件](03-inner-files.png) |
| 非空文件复制后追加 | **正确拒绝，无残留。** `grow.bin` 从 131,083 增至 131,101 字节。前两块仍各回 65,536 字节，最后一块回 12 字节（公布大小所需为 11）。目标无文件及隐藏暂存。[访达截图](04-append.png) |
| 非空文件复制后截短 | **正确拒绝，无残留。** `shrink.bin` 从 131,083 减至 131,079 字节。前两块仍各回 65,536 字节，最后一块只回 7 字节（应为 11）。目标无文件及隐藏暂存。[访达截图](05-truncate.png) |
| 零字节文件复制后追加 | **未拒绝。** `empty-grow.txt` 从 0 增至 14 字节；访达仍创建了零字节文件，散列不匹配，helper 未发范围请求。macOS GUI 按公布大小决定读取循环，大小为 0 时跳过读取，因此未触发 helper 的长度校验。[访达截图](06-empty-append.png) |

非空变更测试的日志记录的是服务端成功返回的长度（`is_error=false`），并不表示访达写入成功。日志没有直接打印请求的长度；追加场景的 12 字节末块与源码中无锁末块多请求 1 字节的逻辑一致。只验证上述大小变化；不据此声称无锁传输能检测同长度的内容变化。
