# #210 已抓取 Win 组合成员与 IME 的输入归属

上一轮实际 VM 已证明前台 hook 投递、Alt+Tab、左右 Win、逃生与失焦清理，
但 Win+R 的 R 被本地拼音预编辑消费，peer 只有 Super pair。
[失败记录保留](../2026-10-20-foreground-system-keys-vm/README.md)。

本次候选只为本轮真正吞下的 Win-down 建立归属；在其组合中捕获成员，
在事件发生时保存前台 HKL 和修饰键快照，在 UI 路径做字符转换。
首次 down 选定的 keysym 沿用到 repeat/up，即使 Win 先松开或布局变化。
普通无 Win 输入继续原窗口/IME 路径，不冒领预先由本地持有的 Win/字符。
窗口中来的 logo/Super 新 press 留本地，防止抓取开启后残余字符泄漏。

Ctrl/Alt 保留窗口 modifier 通道以维持现有 Ctrl+Alt+Enter；Win 内的 Shift
可归属组合并叠加到字符快照。新的 Escape-down 保持本地；若 Enter 已由远端
持有后才按 Ctrl/Alt，其 key-up 仍释放原 keysym，覆盖主/数字键盘 Enter。
RDP 的原逃生判定没有修改，callback 不调用 SendInput，也不伪造 injected。

最终冻结源码的本地检查：

- `just ci-vnc` 退出0：root **423 passed / 0 failed / 11 ignored**；native feature
  **138/0/3**；vendored graphics **202/0/0**。计数存在重叠，不累计。
- 两 workspace 与 feature 全 target Clippy `-D warnings`、SSH integration
  编译均通过；没有运行 Docker integration 或托管 Actions。
- 9 项定向回归通过，包含真实 RFB 字节顺序、Win先释放、repeat、preheld
  本地组合不发线、窗口 logo fallback、两种 Enter 的完整释放序列。
- 3 项显式 Windows native 回归通过：独立消息泵、安装失败恢复、Raw Input
  lease 的精确恢复和后续注册保护。此组只产生标记 F24，不拦截真实键。
- `vnc_native_acceptance` 与最小 GUI example 构建通过；EXE hash 与压缩包内
  候选来源一致，见 candidate-provenance.json / provenance.json。

这些本地回归不替代实际前台 Win+R/Win+Shift/逃生矩阵，VM 的新候选结果
需单独读取原始 app/observer/peer 记录后才计通过。

明确保留的边界：死键、无映射、多 scalar 或未知 Caps seed 会说明字符未发送，
不会猜测 US 字符；先前已发送的 Win 仍按原顺序释放，不能撤回整个组合。
前置 German dead-key 状态可能影响 ToUnicodeEx 的单 scalar 结果，这一额外
矩阵尚未证明正确。跨厂商 Unicode 消费、物理键盘硬件和全部布局均不由本记录认证。

API：[Microsoft ToUnicodeEx](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-tounicodeex)
说明 flags bit2 在 Windows 10 1607 起不修改键盘状态；它不保证忽略先前的
dead-key 状态，因此本轮没有为该场景扩大验收声明。
