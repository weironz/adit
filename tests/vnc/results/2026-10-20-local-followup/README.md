# 本地后续：ClearCodec 越界修复与最小窗口键盘诊断

2026-10-20，Windows x64 / Rust 1.98.0。基线为草稿 PR #222 的
`f7982d6290889c9ad3fc5cc0422e3a50dea5977b`，测试时改动尚未提交。
本轮没有连接或更改 VM、账户、服务、防火墙、显示设置或电源状态；
没有启动新的托管工作流，没有发布版本。测试窗口均自动退出。

## 确定性修复：#223 的 ClearCodec 越界类别

旧代码允许按像素总数复用 glyph，却按缓存原始行宽取数据。
2×2 的缓存读取为 1×4 时，第 3 行访问到第 20 字节，而缓存只有
16 字节，得到与现场相同的 panic 文本和代码位置。
[修改前记录](glyph-before.txt)包含两个预期失败，分别对应合法宽高比
变化和截短的内部缓存。首次红灯测试直接填充等价缓存；正式回归中的
合法缓存随后改为由完整的合成 ClearCodec 协议数据解码生成。

修复依据 [FreeRDP 的 glyph 读取逻辑](https://github.com/FreeRDP/FreeRDP/blob/master/libfreerdp/codec/clear.c)：
缓存按线性像素流、请求的行宽读取。现在核对实际 byte slice，合法的
1×4 / 4×1 / 较小前缀得到逐像素预期结果；超出缓存或数据截短返回
解码错误。现有 `egfx.rs` 错误分支记录警告并跳过该 tile，不会由这处
切片导致 helper panic。这不等于所有解码错误均强制断开连接。

本轮没有现场 ClearCodec 流或真实屏幕回放，不能断言现场包一定是
2×2→1×4，也未证明 RDP 重连后的 GUI 失去响应同属这个原因。
因此 #223 只更新已修复的越界分项，剩余范围继续开放。

## #210：无需 VM 的最小复现，仍未验收通过

`keyboard_hook_gui_probe` 链接实际 UI 库，采用实际 hook 工作线程，
不启动 VNC、会话管理、配置或凭据库。观察模式在启用前将目标 HWND
设为 0，任何真实输入都不会被此探针拦截。只注入 F24 down/up，并用
进程特有的 `dwExtraInfo` 标记累计本次探针事件；绝对 injected 总数
或旧进程延迟事件不能使测试通过。

下面三项使用同一 GUI EXE，hash 在各 invocation 和 provenance 中。
每项需要完整周期，前台模式还要求周期内持续拥有前台。

| 对照 | 本次 F24 接受数 | 本次标记回调数 | 结果 |
| --- | ---: | ---: | --- |
| [最小窗口前台](foreground-report.json) | 2 | 0 | 失败，前台持续为自身 |
| [后台比较](background-report.json) | 2 | 2 | 通过，仅后台回调投递 |
| [窗口创建前安装](prearm-report.json) | 2 | 0 | 失败，前台持续为自身 |

早期未区分进程的累计计数曾看似支持提前安装。加入周期增量和专属
标记后没有通过；未将该早期记录认证为修复。保持常驻 hook 的另一次
候选也在三次开关中失败，已撤回，未修改产品的 hook 生命周期。

还进行了独立观察进程比较：

- [首次](cross-early-invocation.json)中注入时的前台 PID 与 GUI PID
  不同，虽然 observer 收到两次本次回调，也不能证明前台 GUI 场景通过。
- [延后注入](cross-delayed-invocation.json)时，observer 记录的前台
  PID **48396** 等于 GUI PID；GUI 全部三个周期持续在前台。
  observer 的 F24 接受数为 2，但自身标记回调始终为 0，GUI 自身标记
  回调也为 0。两进程均正常退出。见[完整 observer 输出](cross-delayed-observer.txt)。

这进一步缩小复现条件，并排除了“仅仅移到另一个进程就能通过本次
投递诊断”的假设；没有建立 Windows 输入链的确定根因。
这些是注入事件的回调诊断，不替代实际 Win、Alt+Tab、退出捷径、
持键失焦或 VNC 服务端消费的完整验收，#210 继续开放。

## 本地验证和边界

| 检查 | 结果 / 证据 |
| --- | --- |
| `just ci-vnc` | [最终退出 0](local-ci.txt)，root 414 passed / 0 failed / 11 ignored |
| native feature 库 | 129 passed / 0 failed / 1 ignored，与 root 有重叠，不累计总数 |
| 两工作区 Clippy、SSH integration 编译 | 已包含于上述本地 CI；未运行 Docker integration |
| feature 全 target 最终复核 | [Clippy -D warnings 退出 0](feature-clippy-final.txt) |
| vendor graphics 全测试 | 202 passed / 0 failed，最终正式 wire seed 用例由 local-ci.txt 执行 |
| RDP helper 测试 | [30 passed / 0 failed](helper-tests.txt)，3 个录制回放测试因无 capture 忽略 |
| RDP helper 实际构建 | [退出 0](helper-build.txt)，EXE hash 见 provenance；未替换安装版或做本轮实连 |
| F24 抗 UI 卡顿显式回归 | [1 passed](hook-regression.txt)，吞键计数不增加 |

[首次本地 CI](local-ci-first-failed.txt)在 feature example 上遇到新增
诊断接口的 unused/dead_code，已修正并保留失败记录。standalone vendor
测试还报告 `progressive.rs` 两个既有 warning（unused qualification / 未满足
lint expectation）；本次未改动该文件，不把 vendor 测试称为无警告 lint。

本机检查不认证 Linux/macOS/ARM、物理输入、服务端 Unicode 全兼容、
原生 OS DPI 矩阵或 Windows 睡眠。#205、#212 本轮没有新增实机通过项。
所有结果只包含合成协议、进程/回调计数和构建日志，无账户密码、真实
桌面、剪贴板正文、EXE/PDB 或 dump。

正常重跑入口为 `tests/vnc/local-hook-gui.ps1`，参数与边界见上级
README。归档的 cross-process-reproducer.ps1 保留实际执行的本机路径和
一次性结果目录；需要新目录才能重跑，不会覆盖旧证据。
`provenance.json` 同时提供工作区原始字节及 LF 规范化源码 hash；
`sha256.json` 提供此目录归档字节的 hash。文本日志仅规范化 LF 和
末尾空行，保留成功与失败输出正文。
