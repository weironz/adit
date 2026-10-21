# #210 保留失败：只统一 hook 的 Shift 身份仍不足

被测产品 `f31770e`，目标 WIN11-VM Session 2，实际 EXE SHA-256
`f88caca4837dec922e548600d8b19679f04ab425d5154b041f5169fa8466048d`。
app=11632、peer=2144、observer=9676，dispatcher=11208=前台线程，hook worker=11080。

`core-matrix-peer.jsonl` 的46条 key 与上一候选已修复的左侧完整矩阵一致：
Win+R、两种左 Shift 顺序、Alt+Tab/左右Win、主/数字键盘已持有 Enter 再按
Ctrl/Alt，全部维持正确顺序和最终释放。

`right-modifiers.commands.txt` 增加右 Shift 在 Win 前/后两种顺序；实际 peer
增量仍只有右 Ctrl/Alt 的4条 key，缺少两组各6条的 Shift 组合，等待边界后
hook 安全关闭。独立 observer 的右 Shift 仍为 scan=54、flags=1/129、
injected=false。不能把低级 hook 侧归一化视为整个窗口输入链路已修复。

本地锁定依赖 winit 0.30.13 的 Windows keyboard mapper 为 `0036` 生成
ShiftRight，而 `E036` 落入 NativeKeyCode::Windows。窗口侧别名是下一项定位
方向；下一候选会增加仅针对 E0-Shift 元数据的诊断，以实际窗口事件确认。
本记录没有伪造窗口键事件或修改系统标志来充当验证。

`native-stop.request` 已停止此 case。新候选继续使用同一个独有 GUID 的共享
与清理租约，最终所有 case 的 PID、监听、共享和任务将一起独立核对。
原始 JSONL 的换行统一 LF，不提交截图、EXE 或凭据。
