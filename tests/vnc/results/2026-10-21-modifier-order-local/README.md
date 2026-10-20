# #210 修饰键窗口事件与 hook 队列的顺序边界

[上轮真实 VM](../2026-10-20-win-chord-vm/README.md) 已修复 Win+R，但保留
Shift/Alt-up 晚于后续 Win-down、Ctrl/Alt 晚于 captured Enter-up 的失败。

候选为每个新按下且走普通窗口路径的左右 Shift/Ctrl/Alt 建立队列边界。
它们仍传给本地 Windows/IME；只有对应窗口事件转发完毕后才确认该边界，
后面的 Win/Tab/已捕获组合成员才可下发。开启抓取前已持有的修饰键、不属于
本轮窗口的键和 injected 键不建立边界。代次变化或失焦清理丢弃旧边界。
若窗口事件缺失超过3秒，释放本客户端所属按键并关闭抓取，给出中文提示。
callback 只追加无文本的边界，网络发送、等待与超时处理均在 UI 路径。

最终冻结源码的检查全部退出0：

- `just ci-vnc`：root **425 passed / 0 failed / 11 ignored**；native feature
  **140/0/3**；standalone vendor graphics **202/0/0**，计数重叠不累计。
- 两 workspace 与 feature 全 target Clippy `-D warnings` 通过；SSH integration
  编译通过，没有运行需要 Docker 的集成测试或托管 Actions。
- 3项显式 Windows native 回归通过；队列定向回归覆盖 Shift/Alt/Ctrl 的窗口
  松开不能被后续 Win 越过、乱序确认不能跳过边界，以及无需真实等待的超时。
- 最终 acceptance GUI 构建通过；[来源](provenance.json) 保存5个源码的 LF hash
  和最终 EXE hash。只有在目标端重新计算 EXE hash、读取 app/observer/peer
  后才计实际前台通过，VM 结果另外归档。

这项边界不声称所有普通文本事件与全部 hook 事件已有全局时间排序；它解决
本轮确定的 pass-through 修饰键边界。前置 dead-key、跨厂商 Unicode、物理
键盘 hardware auto-repeat 不由这些本地回归代替。
