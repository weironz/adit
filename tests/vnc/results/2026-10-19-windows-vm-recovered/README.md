# 恢复后的 Windows VM 自主验收记录（2026-10-19）

本目录保留真实跨主机 TCP 丢包、原生拼音和部分键盘输入的成功与失败记录。
任务和剩余验收要求仍以 [#205](https://github.com/weironz/adit/issues/205)、
[#210](https://github.com/weironz/adit/issues/210)、[#212](https://github.com/weironz/adit/issues/212) 为准。

## 来源与范围

- VM：Windows 11 专业版，10.0.26300，vmxnet3；实际用户桌面 session 1。
- 原生客户端来自 Windows runner 39230932522，EXE SHA-256
  `896b2731bb63562f797d5e822e2319101f0e823b175c214810c2918fd323bed5`。
  与 main `7f088a1` 的 Rust 运行时输入相同；本轮键盘观察使用这个原始二进制。
- Python 3.12.10 便携包来自 python.org，签名有效，发布者 Python Software Foundation；
  ZIP SHA-256 见 `provenance.json`。不替换 VM 原有 Python，不保存测试主机密码。
- 客户端经 NetBird Windows 路由网关连接 VM 的合成 VncAuth/ZRLE 服务。
  服务端不公布 Fence；固定挑战和公开合成凭据仅用于隔离夹具，不证明认证安全性。
- NTLM 消息加密、指定用户的临时 WinRM endpoint 和来源限定规则用于测试部署。
  未修改 TrustedHosts、Basic 或 AllowUnencrypted。

## 网络结果

| 记录 | 客户端与真实故障 | 结果与清理 |
| --- | --- | --- |
| `network-actual` | 修复前；静态 35 秒后真正启用 Windows 防火墙并阻断测试 TCP | 等待 105 秒仍未进入断开；控制器终止。还包含一次清理参数类型错误，随后独立恢复。失败保留。 |
| `network-fixed` | 设置每 socket 的 TCP_MAXRT=30 秒、TCP_KEEPCNT=3 | 同标签新帧已恢复，但清理检查失败；原始 `result.json` 过早写成 passed，**本轮整体不计通过**。 |
| `network-final` | 同一运行时修复；35.082 秒静态画面，真实 Windows WFP 丢包，无 Fence | 完整通过，所有远程操作无失败，原始 Domain/Private/Public 状态逐项恢复并校验。 |

最终合格轮客户端时间轴：完整红帧 ready **2383 ms**；断开并撤销旧帧
**61959 ms**；同一标签、一个 session、VncAuth/ZRLE/只读保留且完整蓝帧
**72478 ms**；完整绿帧 **76599 ms**。这是单次观察，不是可靠分位数。
外层 `remote.json` 包含部署和 RPC 耗时，不能混作客户端时间。
`pktmon counters` 返回了此端口的 TCPIP 丢弃计数，原始返回保存在远程记录中。
VM 端确实发生 OS 丢包，不是只停止应用回应。但 NetBird 的 Windows userspace
路由网关会终结并重新建立 TCP，因此这不是客户端直连 TCP keepalive 的最终证明；
也不能仅凭前后两轮时间差确定客户端哪个 socket 计时器负责了恢复。
此处仅记“经网关的丢包与同标签恢复”；直连范围单独补验。
依据：[NetBird 网络模式说明](https://github.com/netbirdio/netbird/blob/main/AGENTS.md)。

静态阶段仍返回空 framebuffer updates，不能把它扩大为完全无 RFB 回复的
静默桌面。代码另增加真实 loopback TCP、无 Fence、合法挂起 incremental
请求 35 秒后仍能收到新画面的回归，覆盖“不能用 30 秒读超时误断静态桌面”。

TCP 探针启动前没有记录 EXE 哈希，随后 Clippy 重链接了该文件。因此
`provenance.json` 明确区分源码来源与后来磁盘上的 EXE 哈希，不把后者作为
该轮实测二进制哈希。控制器现已在启动前记录探针哈希，后续运行可补足此项。
本轮运行时实现与 `aaec5ce` 相同；[该提交完整 PR CI](https://github.com/weironz/adit/actions/runs/39244674989)
已 completed/success，11 项通过、ARM 安装器按 PR 策略跳过。

## 原生输入的已观察范围

- `ime-preedit.png`：在真正的微软拼音窗口用扫描码输入 `zhongwen`，出现“中文”候选。
- `ime-commit.json`：选词后服务端完整收到 `0x01004e2d`、`0x01006587`，各一次按下/释放。
  这不是剪贴板粘贴、协议 TEXT 或直接给 UI 填入组合事件的替代证明。
- `english.json`：实际窗口路径的 `a`、`A`、Ctrl+A，以及组合键释放记录。
- `left-win.json`：生产抓取路径送出左 Windows 键按下/释放。
- `modifier-observations.json`：观察到左右 Ctrl、左 Shift、左右 Alt；不把缺失的
  右 Shift、右 Windows、Alt+Tab、德语 AltGr 或 emoji 记录写成通过。
- `os-input-observer.json`：选词空格等实际 Windows hook 事件的 injected=false。
  输入来自 RDP 扫描码，不能扩大为一台物理国际键盘的硬件认证。

图片是原始 RDP framebuffer 的无损矩形裁剪，仅保留合成窗口，排除背景用户
控制台。原始文件哈希、尺寸、裁剪坐标在 `native/image-crops.json`；没有改写像素。

## 保留的失败和未完成清理

直接向合成 HWND 投递德语 `WM_INPUTLANGCHANGEREQUEST` 后，HKL 读取值已经变化，
但窗口输入失去响应。这条切换方法没有取得有效 AltGr/emoji 结果。
已先捕获两个私有 minidump，再结束自有窗口。原始 dump 和系统 DLL 不入库。

[原始源码符号恢复 runner](https://github.com/weironz/adit/actions/runs/39244617362) 已成功。
重建 EXE 与被测 EXE 的 `.text` 字节哈希完全一致，可用其 PDB 映射代码地址。
上下文 RIP 在 `NtUserMessageCall`，RDX=0x50、R9=0x04070407；栈中候选地址
对应 winit 0.30.13 的 `public_window_callback_inner` / `DefWindowProcW`。
这是阻塞现场证据，尚未证明 Adit 根因，也不是正式展开的完整调用栈。
详见 `native/layout-hang-context.json`。

VM 报告支持 S1，且接受了 resume=true 的 30 秒 waitable timer；随后发出待机
请求，目标失去网络响应，没有按该计时器恢复可达。尚未取得恢复后的系统电源
事件，**不计睡眠恢复通过**。现有 RustDesk 控制工具持续报告窗口最小化/人工
输入状态冲突，宿主机既有 SSH 密钥未获登录，未进行宿主机重启或 VM 强制重置。

待机脚本已经改为默认只做能力观察；实际待机必须提供针对该 VM UUID、五分钟
内验证过的外部恢复路径记录。计时器 API 成功不再被当作可安全无人值守恢复的证据。

网络测试的临时阻断和防火墙配置已在待机前恢复；整个 VM 会话的 WinRM endpoint、
来源限定规则及测试任务仍需最终清理。`cleanup-status-snapshot.json` 是当时的
等待状态，**不是清理成功记录**。已启动有时限、凭据仅在内存的恢复清理进程；
VM 恢复后先取回白名单证据，再调度独立 SYSTEM 清理，结果仍需复核。

## 独立检查

```powershell
python tests/vnc/results/2026-10-19-windows-vm-recovered/audit.py
```

审计逐字节验证 `manifest.json`，明确排除清理失败的中间轮，并核对中文完整提交、
已观察修饰键、合格网络轮的所有操作、同标签安全策略及未完成状态。不会关闭原有
混合验收项，不把 VM 不可达改写成整个 VNC 工作无法推进。

Microsoft 的 [TCP socket 选项说明](https://learn.microsoft.com/en-us/windows/win32/winsock/ipproto-tcp-socket-options)
是 TCP_MAXRT 与 TCP_KEEPCNT 的接口依据；实际参数作用范围另由真实 Windows
socket 回读和未修改的对照 socket 回归验证。
