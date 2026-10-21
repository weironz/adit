# 本地 CI 与 Windows 系统键、厂商 Unicode 验收

日期：2026-10-20。基线为 main `fd81ff758b085ff259481caa6716c724731aaa86`，
测试期间的改动尚未提交。Windows 编译器为 Rust 1.98.0。没有发布版本，
没有启动新的 GitHub Actions。此归档包含成功和失败记录。

## 本地检查

`just ci-vnc` 退出 0。它包含正常 `just ci`，再检查 native VNC feature 的
全部 target 并运行 feature 库测试。完整输出见 [local-ci.txt](local-ci.txt)。

| 检查 | 结果 | 范围 |
| --- | --- | --- |
| 工作区 build、Clippy，独立 RDP workspace Clippy | 通过 | Windows 本机 |
| 工作区测试 | 414 passed / 0 failed / 11 ignored | 不包含独立 RDP workspace 测试执行 |
| SSH integration `--no-run` | 通过 | 编译，未启动 Docker/sshd |
| native VNC feature Clippy `--all-targets -D warnings` | 通过 | 含两个钩子探针和实际窗口 example |
| native feature 库测试 | 129 passed / 0 failed / 1 ignored | 与普通工作区测试存在重叠，不累计成独立测试总数 |
| 显式单独运行的 F24 抗 UI 卡顿回归 | 1 passed | 仅注入 F24，不认证系统组合键 |

原实现把 hook 安装在调用线程上。[修改前记录](hook-before-fix.txt)显示，
调用线程不泵消息 1.6 秒后未收到回调。改为独立 Win32 消息线程后，
[最终回归](hook-regression.txt)收到回调，注入计数增加 2，吞键计数保持 0。
这证明了线程隔离的作用，不能据此宣称完整 GUI 全局抓取完成。

## 实际 Adit 窗口：仍未通过

目标为用户授权的 Windows VM，Session 1，独立合成 RFB peer 绑定
`127.0.0.1:15915`。窗口使用生产 widget、dispatcher 和 SessionManager，
配置、凭据和事件日志隔离，双向剪贴板关闭。观察器只记录合成窗口的输入。

| 轮次 | 证据 | 结果 |
| --- | --- | --- |
| case5 | [ready/hash](case5-ready.json)、[窗口事件](case5-native-events.jsonl)、[peer](case5-peer-events.jsonl) | 实际 token integrity RID 12288（High）；抓取开启、消息循环运行，但回调计数均为 0。Alt+Tab 触发本地切换，peer 只有 Alt down/up，没有 Tab |
| case7 | [ready/hash](case7-ready.json)、[窗口事件](case7-native-events.jsonl)、[peer](case7-peer-events.jsonl)、[限定前台点击](case7-owned-click.json) | 以 Limited 任务启动；前台 PID 与目标一致后点击真实开关。Adit 自身 SendInput 接受 F24 down/up 两个事件，回调计数仍为 0；消息循环继续运行。Alt+Tab 仍切换本地并解除抓取 |
| 独立源文件探针 | [standalone-hook.txt](standalone-hook.txt) | WinSta0/Default；6 次回调，其中 2 次为自发 F24 注入，其他输入没有 injected 标记 |
| 独立 UI 库探针 | [控制台](standalone-root-hook.txt)、[GUI 子系统](standalone-gui-hook.txt) | 两者均实际收到 4 次非注入回调；因此 GUI 子系统本身不能解释完整 Adit 的失败 |

计数依次为：进入 callback、armed、injected、VNC 候选系统键、已入队。
第一个计数位于过滤之前，因此“跳过 injected input”不能解释 case7 的零回调。
这些事实仍不足以确定 Windows 停止投递的具体原因。安装句柄、绿色开关和
线程心跳均不作为抓取通过判据。未完成完整 Alt+Tab/左右 Win/Win+字母/
Ctrl+Alt+Enter/持键失焦/切换标签矩阵。

case5 的被测二进制来自较早的独立线程版本；case7 增加了显式
`ADIT_NATIVE_HOOK_SELFTEST=1` 诊断，只在 native acceptance feature 中注入 F24。
归档记录被测 EXE hash，不把它们宣称为最终提交树已经通过原生验收。
最终本地 CI 对本提交的 Rust 逻辑运行，后续仅增加证据和文档。

未计入通过记录：控制 RDP 到期或异常后的失焦/无响应窗口、没有命令应答的
PE 对照尝试。一次控制 helper 在 ClearCodec 解码处出现切片越界，重新连接后
继续测试；这不作为 VNC 断线、OS 睡眠或自动恢复的通过证据。

## 实际 UltraVNC 文本消费

使用 [VM fixture](../../windows-vm-unicode.ps1) 和生产
`vnc_fixture_driver`，对便携 UltraVNC 1.8.3.0 进行 VncAuth 连接，
向独立 Windows WinForms TextBox 发送 `a中文😊é`，不调用剪贴板发送。
目标关闭 IME，因此这是协议到实际应用的 Unicode 验收，不是原生 IME 端到端验收。

官方 [1.8.3.0 下载页](https://uvnc.com/downloads/ultravnc/172-ultravnc-1-8-3-0.html)
所列 ZIP SHA-256 为
`11163a0b9b86321bf6403eb0cf84b81f7230ce32e6291377e1fe5beb987f7e4f`，
执行前已匹配。只运行便携应用，没有安装或修改既有 VNC 服务。

- [第一次](vendor-unicode.json)：控制会话已断开，目标不在前台，未发送输入。
- [第二次](vendor-unicode2.json)：Windows PowerShell 5.1 不支持
  `ProcessStartInfo.StandardInputEncoding`，未进行有效协议消费验收。
- [第三次](vendor-unicode3.json)：改为显式 UTF-8 StreamWriter 后，目标 PID
  等于前台 PID，driver 退出 0；实际文本为 `a中文é`，**整项失败**。

期望 scalar 为 `0061 4E2D 6587 1F60A 0065 0301`，实际为
`0061 4E2D 6587 F60A 0065 0301`。中文和组合符号在该样本中保留，emoji
被截断。官方服务端源码的
[keysymToUnicode 和 Unicode 输入路径](https://github.com/ultravnc/UltraVNC/blob/9f60ac352f66ade046d1aecd173aca1ca949ec45/winvnc/winvnc/vnckeymap.cpp#L648)
以 16-bit WORD 保存字符，与观察到的截断相符。这是源码佐证，未对下载二进制
做符号级归因；不能通过向所有服务端发送非法 surrogate keysyms 来掩盖问题。

服务端 TCP 实际只监听 loopback，厂商 UDP echo 实际监听 wildcard；
既有防火墙 Private/Public 关闭，本次没有更改它们，也不声称 UDP 被防火墙隔离。
此操作只用于已授权的测试 VM。各轮结束后该服务器、文本框、driver 和对应
TCP/UDP 端口均退出，最终还进行了独立环境复核。

## 电源能力与清理

[power-capabilities.txt](power-capabilities.txt)显示该 VMware VM 只支持 S1，
不支持 S3、S0 low-power idle 或 hibernate；
[wake-armed.txt](wake-armed.txt)列出网卡和 Remote Desktop Mouse Device。
[平台标识](vm-platform.json)用于匹配外部恢复对象，不是已取得恢复控制能力。
本轮没有再次进入睡眠，没有绕过已有 external-resume-proof 保护。
这些能力查询不构成实际睡眠/唤醒成功。

[初次独立清理](cleanup-initial.json)发现一个残留 PowerShell launcher，
保留 failed 结果。按 PID、进程名和自有目录参数核对并停止后，
[最终独立复核](cleanup-final.json)确认：WinRM Stopped/Manual、
原防火墙 1/0/0、自有进程/启动器/任务为空、共享与临时管理 endpoint/rule
不存在、5985 和测试 TCP/UDP 不监听。归档后还移除了本轮 VM 自有目录；
对应字段为 `directory_removed`，完整目录删除结果见同一最终报告。

归档不包含密码、真实桌面/剪贴板、EXE、PDB、dump、本地构建配置或 OAuth 信息。
文本统一以 LF 保存，sha256.json 对应仓库保存的文件字节。
后续待办与优先级仍由 #205、#210、#212 跟踪；本文件仅描述证据及适用范围。
