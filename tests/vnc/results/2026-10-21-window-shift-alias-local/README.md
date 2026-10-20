# #210 同时匹配 hook 和普通窗口的 E0-Shift 别名

[上一候选](../2026-10-21-window-shift-alias-vm-failure/README.md) 证明仅修改
低级 hook 不足。普通窗口收到的 NativeCode::Windows(E02A/E036) 现在也映射
到 ShiftLeft/ShiftRight，用于 VNC keysym、按下/释放追踪和边界确认。RDP 与
其他未知键不改。原生 acceptance 额外记录这两个扫描码的按下/释放元数据，
不记录输入文本，用于下一轮证明实际窗口身份。

最终冻结源码的全部本地检查退出0：

- `just ci-vnc`：root **427 passed / 0 failed / 11 ignored**，native feature
  **142/0/3**，standalone vendor graphics **202/0/0**；重叠计数不累计。
- 两 workspace 与 feature 全 target Clippy `-D warnings`、SSH integration
  编译通过；没有托管 Actions 或需要 Docker 的集成执行。
- 3项显式 Windows native 回归通过，新增窗口别名回归证明左右 Shift 的
  物理身份与 keysym 一致，其他 NativeCode 保持原样。
- acceptance GUI 构建通过；[来源](provenance.json) 记录6个冻结源码和最终
  EXE hash，VM 实际结果独立归档。

这是修饰键与其后抓取键的顺序修复，不扩大为所有普通文本/hook 事件的全局
时间排序、物理 hardware repeat 或全部国际布局认证。
