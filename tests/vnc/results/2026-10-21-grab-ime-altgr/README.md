# #210 抓取模式中文 IME 与 AltGr 的真实 Windows 验收

最终被测源码 `76df0b6c93f7a5587fbdd9e29f04b2f82d9fbf56`，目标 EXE SHA256
`55f7161690b387b659d60afd8b704c653ea000c99f704bfffb3b2f81455119f2`。
同一台 Windows 11 VM、实际 Adit 工具栏、生产 dispatcher / SessionManager / hook，
输入来自 RDP 扫描码，独立 WH_KEYBOARD_LL 观察器保留非 injected 事件。
合成 peer 的 92 条最终输入序列全部保留；它不证明厂商应用的 Unicode 文本消费。

## 结果

| 实测范围 | 结果与证据 |
| --- | --- |
| 真实微软拼音，抓取关闭 | 有预编辑与“中文”候选，提交两字；预编辑阶段 peer 没有字符。`before-off-*`。 |
| 抓取开启后 Win+R → 中文 | Win+R 完整送达，然后真实预编辑、选词和两字提交；不是直接构造 IME 事件。`before-on-*`。 |
| 失焦 → 回焦 → 再抓取 | 失焦撤销抓取，回焦保留自己的中文上下文，再抓取后再次提交“中文”。`before-focus-away` / `before-refocus` / `before-rearm-*`。 |
| 最终修复版中文 IME | 从德语通过实际 Win+Space 切回拼音，确认 native context open、中文转换状态、抓取开启，再验预编辑与提交。`final-pinyin-*`。 |
| 德语 Win+物理 Y/Z | 正确送出 z/y；当前窗口 HKL 为 `0x04070407`。`final-dead-recovery`。 |
| 德语 AltGr | `@`、`€`、Win+AltGr+Q 与后续 Win+R 通过；等待超过原三秒超时仍 armed。`final-altgr` / `final-real-ctrl-altgr`。 |
| 真正左右 Ctrl | 左/右 Ctrl+A → Win+R 的顺序完整，真实 Ctrl 没有被辅助 Ctrl 规则过滤。`final-real-ctrl-altgr`。 |
| 重音组合 | 普通 acute+e 产生 é；完成未决重音后也能继续 Win+R。`final-accent` / `final-dead-recovery`。 |
| 最终源码基础组合回归 | Alt+Tab、右 Win、左 Shift 在 Win 之前、右 Shift 在 Win 之后、持 Enter 后再按 Ctrl/Alt、本地 Ctrl+Alt+Enter 逃生完整通过。`final-core-after-ime`。更完整的旧 118 条矩阵另见[此前记录](../2026-10-21-foreground-system-keys-final/README.md)。 |

每个中文提交是 `0x01004e2d`、`0x01006587` 各一对按下/释放。
fixture 新增的状态采样通过 `window::run` 在拥有 HWND 的原生线程读取 IMM；
只访问自身窗口，只记录 open/conversion、输入资格、epoch 和事件长度，不记录文本。
采样依据 [ImmGetOpenStatus](https://learn.microsoft.com/en-us/windows/win32/api/imm/nf-imm-immgetopenstatus)
与 [ImmGetConversionStatus](https://learn.microsoft.com/en-us/windows/win32/api/imm/nf-imm-immgetconversionstatus)。
读取转换状态不代替真实预编辑和远端输入证据。

## 发现并修复的 AltGr 问题

前一个被测版本 `312cc33` 实际能发送 AltGr 字符，但随后三秒退出抓取。
独立观察器记录 Windows 的辅助左 Ctrl：`vk=0xa2`、**完整 scan=0x21d**、
`injected=false`。winit 0.30.13 的 Windows `is_current_fake` 会过滤 AltGr 附带的
Ctrl，因此不存在可供 Adit 确认的普通窗口 Ctrl 事件。旧 hook 把扫描码截为
`0x1d` 后建立了永远无法确认的 modifier fence。

修复仅在 VNC hook 路径让这一明确标记的辅助 Ctrl 继续走 Windows 处理，
不把它加入按键归属和窗口 fence。真实 `0x1d` Ctrl、右 Ctrl、RDP 和 injected
过滤保持原有路径。新增队列回归验证辅助边沿不会阻塞后续键，真实 Ctrl 仍受保护。
前一个版本的 40 条输入、退出状态和日志保留在 `before-*`，没有删掉失败样本。

## 已确认的限制与不作的推断

- **未完成的本地 dead-key + Win 字符组合仍是明确不支持的情况。**
  实测 acute → Win+R 时 `ToUnicodeEx(flags=4)` 返回多个 scalar；客户端显示中文
  提示，跳过 R，Win 按下/释放仍完整，抓取仍 armed。之后 e 正常完成 é，随后
  Win+R 恢复。`final-pending-dead`、`final-stderr.txt` 和截图保留此限制；
  不把 flags=4 说成会忽略已有 dead-key 状态，也不猜测成美式字符。
- 中文试验早期失败来自未确认 Adit 自身中文模式的准备过程。本轮有自己的上下文
  状态、真实预编辑和四条提交 wire 证据；不据此反推先前每次失败的唯一原因。
- 这不是物理键盘硬件重复认证，也不是所有布局/厂商支持认证。UltraVNC 非 BMP
  截断、TightVNC/Linux 应用实际文本消费继续由 #205 跟踪。
- 前一轮工具栏定位曾点击合成 peer 的 Ctrl+Alt+Del：六条事件保留并被审计精确
  匹配，没有静默裁掉。第一份 `language-added` 报告用了不正确的 PowerShell
  集合序列化，四个 null 保留为 `language-added-invalid-serialization.json`，不计
  成有效配置证据；最终原生 HKL 和系统语言列表独立证明实际状态。
- 控制驱动有一次 Node 批量调用超时，但结果文件已经落盘；独立读回确认该次
  再抓取中文提交实际完成。RDP 控制重连不冒充产品 VNC 自动重连验收。

## 本地检查与回收

最终源码的 `just ci-vnc` 退出 0：主工作区 **428 passed / 0 failed / 11 ignored**，
原生 feature **143/0/3**，独立 vendor graphics **202/0/0**。计数重叠，不相加。
两 workspace 与 feature 全 target Clippy `-D warnings` 通过，SSH integration
只编译；没有执行需要 Docker 的集成，也没有重新运行 Linux/macOS 或付费 Actions。
vendor graphics 自身测试保留既有两条 warning；它们不是 Clippy gate 失败。
三项显式 Windows 原生回归另行顺序执行，**3/3** 通过，见 `native-tests.log`。

`language-restored.json` 与独立子进程 `independent-before-cleanup.json` 证明原始
zh-Hans-CN、InputMethodTips、拼写与手写选项完整恢复；15915 监听和所有自有
GUI/peer/observer 已退出。随后 `cleanup-final.json` 证明本轮 GUID 目录、加密临时
共享、两项租约任务均移除。WinRM 仍 Stopped，Domain/Private/Public 防火墙仍
1/0/0；账号、密码和网络路由没有改动。控制台和本地 RDP 驱动退出另在控制报告记录。

`provenance.json` 固定源码 Git blob 和目标二进制来源；`SHA256SUMS` 对文本按 LF
校验，对 PNG 按原字节校验，支持 Windows checkout。复核：

```powershell
python tests/vnc/results/2026-10-21-grab-ime-altgr/audit.py
```

本次只提交推送到 PR #222，未合并或发布。#210 继续保留上述未决 dead-key 能力边界
和跨厂商消费的后续跟踪，不能把核心组合通过写成所有输入情况均已解决。
