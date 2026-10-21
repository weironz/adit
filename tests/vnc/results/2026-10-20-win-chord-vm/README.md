# #210 Win 组合候选的实际 Windows VM 复测

被测提交为 `44f73b94b22f6918fab1e793f6b8915d319e6402`，不是随后归档提交。
目标 WIN11-VM Session 2，实际运行的 acceptance GUI SHA-256 为
`0fe9e877e46feb5cd0b8c1afb7ca89f662e4a459d7c463b4b5181f5f129ed2c2`。
[目标端 native-ready](native-ready.json) 与[候选来源](candidate-provenance.json)
相同；app=8084、peer=4384、observer=10124。完整本地 CI 见
[此前冻结源码的记录](../2026-10-20-win-chord-ime/README.md)。没有运行托管 Actions。

通过真实工具栏开启抓取，dispatcher=7776=前台窗口线程，生产 hook 的自身
标记 F24 回调2。后续输入为现有 RDP helper 发出的扫描码；独立 observer
记录窗口可见的 R/Shift/Alt/Ctrl 为 injected=false。生产 hook 吞掉的键不会
继续传给更早安装的 observer，不能要求 observer 重复记录全部 Win 组合。
本轮不认证物理键盘硬件或厂商服务端消费。

## 已证实的行为

| 项目 | 实际结果 |
| --- | --- |
| 原生 Win+R | peer 精确收到 Super_L-down、r-down、r-up、Super_L-up；修复上一候选只有 Super 的失败 |
| 普通输入路径 | 用户语言配置仍仅 zh-Hans-CN；抓取前非注入 R 到达前台窗口但 peer 无 key。没有把普通 R 改成直接 hook 字符输入 |
| Win→Shift→R | 第一组精确收到 Super、Shift、R 及逆序释放 |
| Shift→Win→R | 大写 R、Shift、Win 都最终释放；与下一组快速衔接时的完整顺序未通过，见下方缺陷 |
| Alt+Tab、左右 Win | 全部键到达 peer，hook 保持 armed；连续衔接时 Alt-up 的完整顺序未通过 |
| 已持有 Enter 后按 Ctrl/Alt | 主键盘、数字键盘 Enter 都各有一次 down/up，最终无持键；不把其他修饰键的相对顺序算通过 |
| Ctrl+Alt+Enter 逃生 | hook 关闭，所属 Ctrl/Alt 释放，没有发送新的 Enter |
| 持 Win 切主机页 / 最小化失焦 | 两次分别补发一次 Super-up，迟到的物理 up 不重复发送；最终持键为空 |

`*-peer.jsonl`、`*-app.jsonl`、`*-observer.jsonl` 是逐阶段累积快照，不能把它们
相加作为输入总数。最终 peer 为54条 key。`audit.py` 核对增量、所属释放、
二进制来源及清理，同时要求下面的失败记录保留，**审计通过不表示功能矩阵全部通过**。

## 新发现：跨队列的修饰键顺序仍错误

`shifts-repeat-peer.jsonl` 中，第二组 Shift→Win→R 的 Win-up 后，下一组
Win-down、r-down 已发送，上一组 Shift-up 才到达。独立 observer 已记录
Shift 的真实松开，不能仅以最终无持键判通过。

`system-basics-peer.jsonl` 同样保留：Alt-down、Tab-down、Tab-up 后，下一组
Win-down 比 Alt-up 更早发送。`enter-held-peer.jsonl` 中，Ctrl/Alt 的窗口路径
也可能落后于 hook 的 Enter-up。因此 Win 字符可用、释放平衡与严格顺序是
不同判据，#210 继续开放。

当前 `KeyboardInput` 在处理窗口事件前无条件 drain 整个 hook 队列；排队的
后续 hook 事件可能越过仍未处理的窗口修饰键。此处为源码与原始记录支持的
定位方向，下一步需为修饰键边界建立队列顺序约束，再用相同矩阵复测。

`shifts-repeat.commands.txt` 包含重复 R-down 请求；实际 peer 有额外 r-up/down，
没有把该控制请求等同于物理硬件 auto-repeat，也不将重复输入判据勾选。
前置 German dead-key 和跨厂商非 BMP Unicode 的剩余范围没有扩大。

## 远程控制和清理

首次 TEXT 在当前 PowerShell 控制台没有产生预期输入，快速 ASCII 也丢字符。
随后使用逐字符扫描码和显式间隔执行公开测试脚本；ACK 仅证明驱动处理命令，
实际执行由目标端 owner、native-ready、app/peer 记录及清理报告确认。首次直接
调用脚本被本机默认执行策略拒绝，改为子 PowerShell 的 `-ep bypass`，没有修改
机器执行策略。[引导](share-bootstrap.ps1)、[初始化](initialize.ps1)、
[启动](run-native.ps1) 与 [独立清理](audit-and-clean.ps1) 均保留源码。

[上一轮清理](previous-cleanup.json) 证明旧 GUID 的进程、共享和任务已清除。
[本轮清理](cleanup.json) 独立核对 app/peer/observer PID 均不存在、15915 无监听、
旧共享与任务不存在；随后精确清理新 GUID 的共享、任务和 payload 内进程。
WinRM 仍 Stopped，防火墙 Domain/Private/Public 仍1/0/0。诊断文件保留在独有
ProgramData GUID 目录，测试控制台与 RDP 驱动已退出。没有修改账号、密码、
语言配置、VPN、防火墙或用户 Adit 配置。私有 VM 截图不提交。
