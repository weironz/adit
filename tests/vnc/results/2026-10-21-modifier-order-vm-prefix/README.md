# #210 修饰键顺序实测通过，右 Shift 前缀差异保留失败

被测产品提交 `981c1680a6295efcf49c89410b8ddba357e2203f`，目标实际 EXE hash
`466546a9febdaa0c13921e068a3f8ddd4585416f791ae2a956e851e8251de3ce`。
WIN11-VM Session 2，app=9932、peer=11728、observer=8072。
dispatcher=1208=前台窗口线程，生产 hook worker=11772；真实工具栏点击开启抓取。
输入来自现有 RDP helper 扫描码，独立 observer 保留 injected=false 原始标志。

上轮失败的顺序已得到实际修复：

- `shifts-repeat-peer.jsonl` 中，第二组 Shift-up 在下一组 Win-down 之前。
- `system-basics-peer.jsonl` 中，Alt+Tab 完整释放后才下发下一组 Win。
- `enter-held-peer.jsonl`、`numpad-enter-held-peer.jsonl` 中，Ctrl/Alt-down 在
  captured Enter-up 之前；按键及逆序释放的8条 wire sequence 完整一致。
- Win+R、Win→Shift→R、左右 Win 保持正常；每个阶段最终持键为空。

仍然失败的新增右 Shift 情况：`right-modifiers.commands.txt` 明确请求
`KEY 36 0 1/0`，独立 observer 却观察到 Windows 的 vk=161、scan=54、
flags=1/129，说明目标端右 Shift 带 extended 标记。窗口路径使用普通
ShiftRight，队列边界两边的身份不一致，右 Shift→Win→R 被阻塞，3秒后
生产安全路径关闭抓取。peer 只有随后右 Ctrl/Alt 的4条键，没有此 Shift 组合。
这不是通过项；最终 hook_diagnostics.armed=false 和全部原始输入保留。

下一候选需统一 VNC Shift 身份，同时保留右 Ctrl/Alt 的扩展标识；RDP 原始
扫描码路径不改。目标端程序已通过 `native-stop.request` 停止，最终共享、
任务、所有 case PID 和15915监听清理会在下一候选完成后独立归档。

本轮目标端哈希与[完整本地CI来源](../2026-10-21-modifier-order-local/README.md)
相同，没有运行托管 Actions。逐阶段 JSONL 为累积快照，换行统一 LF；私有
截图、EXE、压缩包和凭据不提交。此处不认证物理硬件 auto-repeat、前置死键
或跨厂商 Unicode。后续最终证据审计同时验证本失败仍被保留。
