# #210 实际 Windows VM 系统键首轮记录

测试目标为用户授权的 WIN11-VM，Session 1。被测 GUI 是 `e2ecd45` 的
合成原生 acceptance 构建，目标端实际计算的 SHA-256 与本机相同：
`6bb645965aebd6667ccc4ef31939e51aba35ee16d5b32b3c5703fc2d2df8fa35`。
[目标端 native-ready](native-ready.json) 记录 app=4740、peer=6696、observer=12344。
源码/本地 CI 来源见[前台 Raw Input 记录](../2026-10-20-foreground-raw-input/README.md)。

通过现有 RDP helper 发送扫描码，不是修改 LLKHF_INJECTED，也不标成物理
键盘硬件。独立 hook 的[原始观察](observer.jsonl)记录了前台线程 4208 的
Alt、R、Ctrl、Shift 等事件为 injected=false；被生产 hook 吞掉的 Win/Tab
不会继续传到更早安装的 observer，不能要求它重复看见这些已吞事件。
生产 hook 保留 injected 过滤，抓取计数与 peer 的系统键序列相互核对。

## 已取得的实际行为

| 情况 | 实际结果 | 原始证据 |
| --- | --- | --- |
| 点击真实工具栏开启抓取 | grab=true，dispatcher=4208=前台窗口线程；自身标记 F24 回调2 | app.jsonl 的 hook_dispatcher / grab_state / hook_diagnostics |
| Alt+Tab | Alt_L、Tab 各成对下发，期间 hook 保持 armed，未切到本地其他窗口 | matrix-peer.jsonl 前4个 key，matrix-app.jsonl |
| 左 Win、右 Win | Super_L、Super_R 各一对，hook 保持 armed | matrix-peer.jsonl 第5–8个 key |
| Ctrl+Alt+Enter 本地逃生 | hook关闭，远端持有 Ctrl_L/Alt_L 补发释放，没有向 peer 发送 Enter | escape-app.jsonl / escape-peer.jsonl |
| 持 Win 切到主机页 | Super_L 补发一次释放，hook关闭；迟到的物理keyup没有重复发到 peer | lifecycle-peer.jsonl / lifecycle-app.jsonl |
| 持 Win 最小化失焦 | Super_L 补发一次释放，hook关闭；随后所有远端持键平衡 | lifecycle-peer.jsonl / lifecycle-app.jsonl / peer.jsonl |

[最终 app](app.jsonl)、[最终 peer](peer.jsonl)、[最终 observer](observer.jsonl)
为同一轮的原始记录；没有把 F24 injected 计数当作真实系统键数量。
本轮不认证 Limited token、物理键盘硬件、厂商桌面消费或全部国际布局。

## 保留的失败：Win+R 的字母进入本地 IME

首次 Win+R 中，独立 observer 看见非注入 R down/up，但 peer 只有 Super_L
的一对，没有 R。私有画面观察到本地拼音预编辑 `r`。这符合当前实现的边界：
hook 吞掉 Win，普通 R 仍经过本地 IME，不能从完整 hook 投递推断 Win+字母
也已经可用。这个失败继续推动下一项实现，不算已通过。

`shift-toggle-attempt.commands.txt` 是尝试 Shift 切换后的对照；没有独立确认
输入法转换模式，peer 仍缺 R，所以也不把它称作英语布局验收成功。
所有 case 的输入命令均归档，不含密码。`SNAP` 指向的 VM 画面仅用于私有
诊断，没有提交截图。

## 隔离与清理

[setup-vm.ps1](setup-vm.ps1) 是实际执行的公开引导源码，创建独有 GUID 目录、
加密且仅授权测试身份的临时 SMB 共享，以及60分钟的自有清理任务。没有修改
账号、语言配置、WinRM 或防火墙。原始状态见 [owner.json](owner.json)：
WinRM Stopped，防火墙 Domain/Private/Public 为 1/0/0，保持原样。

大 EXE 写入后改由目标端直接计算 hash，避免回读64MB；native-ready.json
已独立验证实际运行文件。其他 fixture 的本地 hash 见 small-upload.json；
它不冒充目标端重新计算结果。

[run-native.ps1](run-native.ps1) 仅为测试子进程指定独立 TEMP/TMP，使用
已有 windows-vm-native.ps1 与 native_rfb_fixture.py。只在 loopback 15915
连接合成 None peer，禁用剪贴板同步，没有访问用户 Adit 配置。
[stopped.json](stopped.json) 记录该轮脚本停止自有 GUI/peer/observer；最终
独立资源核对和共享/任务清理会另附，不能只凭这个布尔值宣称全 VM 已清理。

[cleanup-fixed.ps1](cleanup-fixed.ps1) 在精确 GUID 名称外还规范化重复路径分隔符，
核对 owner/共享目录，只停止此 payload 内的进程并清理此 GUID 的共享/任务。
既有提升 PowerShell 控制台不是本轮创建，不会按 bootstrap_pid 杀掉它。
原始 RDP 诊断驱动当时仅增加限额阶段日志，传输判据只用 ACK；实际行为以上述
三个独立记录为准。后续已改用带严格 0/1 参数检查的正式 rdp_probe。
