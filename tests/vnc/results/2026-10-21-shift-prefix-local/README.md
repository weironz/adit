# #210 右 Shift 的 VNC 物理身份归一化

[实际 VM 失败](../2026-10-21-modifier-order-vm-prefix/README.md) 中，源请求为
非扩展右 Shift，但 Windows 低级 hook 给出 extended 标记，和 iced 的
ShiftRight 身份不一致，顺序边界等待超时。VNC 路径现在统一左右 Shift 的
非扩展身份，包含外部窗口观察、preheld seed 匹配、组合成员和窗口边界；
右 Ctrl/Alt、PrintScreen 等其他键保留原扩展标识。RDP 发送路径保持原状。

最终冻结源码全部本地检查退出0：

- `just ci-vnc`：root **426 passed / 0 failed / 11 ignored**；native feature
  **141/0/3**；standalone vendor graphics **202/0/0**。计数重叠不累计。
- 两 workspace 与 feature 全 target Clippy `-D warnings`、SSH integration
  编译通过；没有托管 Actions 或 Docker integration 执行。
- 3项显式 Windows native 回归通过；新增定向回归验证 OS prefix 不会冒领
  preheld Shift，成员 down/up 使用同一身份，右 Ctrl/Alt 保留 extended。
- 最终 acceptance GUI 构建通过。源码与 EXE hash 见 [来源](provenance.json)。
  实际 case4 前台结果必须另外读取目标端 EXE hash 和原始三方记录后验收。

原右 Shift 失败记录不会被替换为通过；死键前置状态、厂商 Unicode 消费、
物理硬件 repeat 仍保持此前范围。
