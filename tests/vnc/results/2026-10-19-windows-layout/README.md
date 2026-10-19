# Windows VM 恢复、布局切换与输入验收（2026-10-19）

本目录记录恢复后的 Windows VM、一个确定的客户端修复，以及未通过的系统键实验。
工作项继续由 [#210](https://github.com/weironz/adit/issues/210) 和
[#212](https://github.com/weironz/adit/issues/212) 跟踪；本记录不表示它们全部完成。

## 最终候选与有效验收

Windows 11 VM `WIN11-VM`，build `10.0.26300.0`，用户桌面 session 1。
最终原生窗口 EXE SHA-256：
`2dff3d9b74f6e8777c358fd8d67a522bfc061c14b9e747c3b6b24848c5357121`。
窗口复用实际 Adit 组件、输入分发和 SessionManager，使用独立存储、关闭双向剪贴板，
连接 VM 内 `127.0.0.1:15915` 的合成 RFB None peer。

本次修复：关闭系统键抓取时，Win+Space / Win+字母原先可能只把 Super-down
送给远端，Windows 却在本地消费组合的其余部分，误触发远端开始菜单。
现在本地 Win 组合不会进入远端，已经实际发出的远端按键仍能正常释放。
真实 TCP/RFB UI 回归同时检查左右 Super、组合字母、无归属释放和持 Logo 时的所属释放。

输入验收使用 RDP 扫描码；拼音使用扫描码拼写，emoji 使用 Windows 原生面板选取。
没有把 RDP `TEXT`、剪贴板粘贴或直接构造 IME 事件当作原生键盘验收。
`TEXT` 仅用于执行隔离环境的控制脚本。

| 已观察的行为 | 服务端实际效果 | 原始记录 |
| --- | --- | --- |
| 德语 → 英语 → 德语，实际 Win+Space 切换 | 物理 Y/Z 对应 z/y → y/z → z/y；无 Super 泄漏 | `final-before-layout.jsonl`、`final-german-us.jsonl`、`final-us-german.jsonl` |
| 右 Shift、右 Ctrl、德语 AltGr | 大写 Y、Ctrl+A、`@`、`€`，修饰键按下/释放成对 | `final-us-german.jsonl` |
| 德语重音组合 | é，keysym `0x00e9`，成对送达 | `final-us-german.jsonl` |
| 切到微软拼音后输入 zhongwen | 真实预编辑与“中文”候选；预编辑阶段没有提前发出字符 | `final-pinyin-preedit.png`、`final-pinyin-preedit.jsonl` |
| 空格选词提交“中文” | `0x01004e2d`、`0x01006587` 各一次按下/释放 | `final-pinyin-commit.jsonl` |
| Windows emoji 面板选择 😊 | 非 BMP 字符 `0x0101f60a`，没有丢失代理对/截成第一个代码单元 | `emoji-picker-late.png`、`final-emoji.jsonl` |

上述 36 条有效按键记录由 `tests/vnc/analyze_windows_layout.py` 独立读取服务端
JSONL 并逐条比对。`final-before-layout` 是异步读取的**请求名**，实际读回已含
德语基线的四条事件；分析器明确校验这四条，再比较后续英语样本，未把请求名当成
精确时间边界。截图是原始 RDP framebuffer PNG，没有编辑像素。

这是客户端原生输入到 RFB 的证据，不是物理键盘硬件认证，也不证明所有
TightVNC / UltraVNC / Linux 服务端应用能消费全部 Unicode 字符。

## 系统键抓取仍未验收

基线 `212713d` 的两个窗口分别记录在 `case1-*`、`case2-*`。
普通德语输入通过，抓取显示开启时 Alt+Tab 仍触发本地切窗，没有取得服务端 Tab。
带外部布局查询的旧观察器、直接 WM 布局请求及旧私有 dump 均不能替代受支持的
布局切换验收；本轮使用正式语言列表与 Win+Space，最终候选没有复现输入失去响应。
这不等于已经证明先前挂起的根因。

`case3` 增加分发线程观察，确认分发线程与窗口线程相同。
`case4` 尝试独立消息循环线程；`case5` 增加回调计数；`case6` 在相同实验路径下
补显式模块句柄。`case5/6` 都出现 armed=true，但回调计数始终为零，随后失焦撤销。
这些实验没有通过，独立线程与模块句柄的运行时改动已撤回，未作为产品修复提交。
原始实验二进制哈希在各自 `native-ready` 中；实验源码快照未完整保留，不能据此
宣称它们是可重建、已验收的产品版本。

独立 Python WH_KEYBOARD_LL 观察器在同一最终窗口看到 Alt 和 Tab，
`injected=false`，目标线程与窗口身份相符（`observer-first*`）。
因此已经具备继续自动调试的有效输入源，不能再简单归因于“需要用户物理键盘”。
观察器不拦截按键，不在回调里查询外部线程 HKL，只记录系统键，180 秒自动卸载。

第二轮对照没有形成有效验收：记录为空，读回的焦点证明早于该轮观察器启动。
`observer-second*` 保留这个事实，分析器明确将其列为 incomplete，未计通过。
测试控制用 RDP helper 也曾以 101 退出（`controller-helper-exit.txt`）；退出码不能
证明 panic 根因。驱动现有命令 ACK、MOVE 和 helper 退出报告；ACK 仅证明驱动
处理了命令，远端效果必须另查。控制脚本已避开远端报告覆盖 CLIP offer 的竞争。

## 虚拟机恢复与清理

- `old-cleanup-independent.json`：旧 WinRM endpoint 注册项、规则、任务、进程和
  15915/15916 监听均不存在；WinRM 恢复为原始 Stopped / Manual。
- 原待机脚本实际返回耗时 7,506,030 ms，不能算请求的 30 秒自动唤醒通过。
  本轮未再次让 VM 休眠。
- `language-baseline.json` / `language-verification.json`：临时添加的 en-US、de-DE
  已移除，完整语言列表、InputMethodTips、拼写及手写属性与原始 zh-Hans-CN 快照一致。
- 两个观察器均报告卸载 hook，最终合成窗口、peer 和自有 payload 进程均已退出。
- `cleanup-reported.json` 后又执行**不同进程**的管理员只读复查，
  `cleanup-independent.json` 确认共享不存在、自有任务/进程/launcher/规则/监听为空，
  语言已恢复，Domain/Private/Public 防火墙仍为 1/0/0，WinRM 仍为 Stopped / Manual。
  WinRM 停止是原始配置，不能把 5985 不监听重新当作 VM 离线。
- 控制 RDP、临时 SMB 客户端均已正常退出。诊断文件保留，未发布私有 dump、系统 DLL
  或任何测试主机凭据，也没有发布版本。

## 来源、检查与复核

`provenance.json` 记录 Rust 1.98.0、EXE 哈希、源码文件哈希和构建命令。
`native-runtime.patch` 应用于基线 `212713df13f3a97eceeb1b8037c343dc81c601a2`
可还原最终原生窗口对应的 UI 运行时源码；它不包含后续文档或单元测试改动。
`observer-source.py` 保留实际部署的观察器源码。

本地完整 `just ci`：414 passed、0 failed、11 ignored，另含独立 RDP workspace
Clippy 与 SSH integration compile；不把 ignored 测试或独立 RDP workspace 的测试
写成已执行。原生窗口 feature 的 Clippy 另行通过。
`just-ci.log` 仅做必要的 UTF-8 编码转换，原文件哈希另存 provenance。

JSON / JSONL 是 UTF-8 文本传输的原始内容；PowerShell 读取可能去掉 BOM。
`sha256.json` 校验的是**归档文件字节**，不冒充 VM 原文件的字节哈希。
目录 `.gitattributes` 保留记录的换行。CI 同时校验清单、哈希、有效输入序列、清理
状态以及未验收边界；该 Linux 审计任务不代表重新执行了 Windows 原生按键测试。

```powershell
python tests/vnc/analyze_windows_layout.py tests/vnc/results/2026-10-19-windows-layout
```

相关 Win32 契约：[LowLevelKeyboardProc](https://learn.microsoft.com/en-us/windows/win32/winmsg/lowlevelkeyboardproc)、
[SetWindowsHookExW](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowshookexw)。
这些文档约束了实验设计，没有据此直接认定回调未进入或旧挂起的根因。
