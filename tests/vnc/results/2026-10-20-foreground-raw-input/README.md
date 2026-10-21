# #210 前台键盘回调：Raw Input 对照与候选修复

本轮在 `codex/vnc-system-key-capture`、基线 `57edc64` 上完成。候选修复
位于草稿 PR #222，未发布版本。只使用本地 CI，没有启动 GitHub Actions。

## 结论与范围

在本机环境，winit 注册 raw keyboard 后，前台窗口能收到 F24 的普通
窗口事件，但独立消息线程上的 `WH_KEYBOARD_LL` 没有收到对应回调。
同一 EXE 只移除本进程 keyboard usage page 1 / usage 6 注册，鼠标保留，
普通窗口事件与钩子回调就都收到 2 次。这是受控实验所支持的本机失败路径，
不宣称所有 Windows 版本和 Raw Input 应用都有这一行为。

修复在开启抓取前保存并暂停 raw keyboard 注册，退出抓取或安装失败时
恢复。恢复保留其他组件的后续注册，拒绝把已销毁/已更换所属线程的 HWND
重新注册；恢复失败保留快照供重试。鼠标与其他设备类不变，正常窗口键盘与
IME 仍走 WindowEvent。旧 hook worker 只退出自己的 pump，不能清除新一代的
全局抓取 gate；相同目标重复 arm/disarm 保持幂等。

## 修复前的受控对照

| 记录 | 前台/焦点 | 本周期自身标记 F24 回调 | 结果 |
| --- | --- | --- | --- |
| [最小 iced，默认 raw](baseline-raw-default.json) | 持续前台，窗口 F24=2 | 0 | failed |
| [同 EXE，仅移除 keyboard raw](baseline-raw-disabled.json) | 持续前台，窗口 F24=2 | 2 | passed |
| [纯 Win32、相同 hook](baseline-win32-alt.json) | 持续前台和焦点 | 2 | passed |
| [winit-only 默认 DeviceEvents](baseline-winit-default.json) | 持续前台和焦点，窗口 F24=2 | 0 | failed |
| [winit-only DeviceEvents::Never](baseline-winit-never.json) | 持续前台和焦点，窗口 F24=2 | 2 | passed |

[baseline-provenance.json](baseline-provenance.json) 与
[当时的最小窗口源码](baseline-gui-source.rs.txt) 固定对照 EXE 和源码来源。
原始 Win32 EXE SHA-256 为
`59b10b2a4c1b7cbe88a0ec34e6fab5f3d19a7cedccb159d9a290f9b82f6616aa`；
winit-only EXE 为
`3e6aeccb5e7d013a02cff0dea1f1fd1df0dcdde6e08c207a94c2cc3deca6eb03`。
它们均构建于生产 lease 修复之前；当前重建已经包含 lease。

winit 的一次性 focus 请求会产生 Alt 注入，故接受判据使用进程特有的
F24 标记增量，不使用累计 injected 总数。Win32 控制显式 `--focus-alt`
采用同一请求方式；未把这一 Alt 当作 F24 回调。

## 修复后的完整窗口自动验收

[最终完整原生窗口报告](full-native-passed.json) 的 outcome=passed：

- 实际 `AditApp` / `SessionManager` 和生产 dispatcher，通过
  `Message::ToggleVncGrab` 连续开关 3 次，每次前台、焦点、抓取持续有效。
- 每周期接受 2 个带自身标记的 F24 事件，实际库 hook 收到 2 次；注入事件
  继续留在本地，swallowed_delta=0，没有为了测试移除 injected 过滤。
- 每次开启后 keyboard raw 注册不存在，鼠标和其他注册完全相同；每次关闭
  后完整设备列表（包括 flags、目标 HWND）与开启前相同。
- 最终 grab=false、hook=false、raw 完整恢复。独立合成 peer 只监听
  `127.0.0.1:15915`，没有读取用户配置或剪贴板、没有捕获桌面。
- [运行来源与清理](full-native-invocation.json) 记录 EXE/fixture hash，
  自有 GUI 与 peer 均退出；[peer 记录](full-native-peer.jsonl) 保留原始协议事件。

前几次运行因前台/焦点中途丢失，分别保留为
[首次完整窗口 incomplete](first-full-incomplete.json)、
[首次最小窗口 incomplete](first-minimal-incomplete.json)、
[第二次最小窗口 incomplete](second-minimal-incomplete.json)。部分周期的局部
成功不改变整体 incomplete；没有放宽连续前台要求来取得最终通过。

这一验收是程序化生产 dispatcher + 注入 F24 的回调投递证明，**不是**
真实 Win/Alt+Tab 的拦截、工具栏鼠标点击、所有布局/厂商文本消费的完成证明。
#210 的完整系统键矩阵继续独立验收，不因本记录自动关闭。

## 本地检查

- [最终 `just ci-vnc`](local-ci-passed.txt) 退出 0：root 414 passed / 0 failed /
  11 ignored；native feature 库 129/0/3（与 root 重叠）；vendor graphics 202/0/0。
  包含两工作区与 feature 全 target Clippy `-D warnings`、SSH integration 编译。
  未运行 Docker integration，也不替代 Linux/macOS/ARM 的检查。
- [显式 Windows native 回归](native-regressions.txt) 3 passed / 0 failed：
  UI 线程卡顿后回调、安装失败恢复、raw 注册幂等/后续注册保护/其他设备不变。
- [最终候选构建](build-final.txt) 退出 0。
- RDP 手动驱动的 KEY 标志此前把非法文本静默解释为 false。现在只接受
  `0/1`，其他值明确报错；[两个 parser 回归](driver-parser-tests.txt) 通过，
  最终完整 CI 已覆盖这个修正。早期误用布尔词的 VM 序列不计为按键验收。
- [首次本地 CI](local-ci-first-failed.txt) 的新增自动验收 guarded match Clippy
  失败保留；已修正后完整重跑。vendor graphics 的两个既有 progressive warning
  保留，不把测试输出称作无警告 lint。

源码与二进制来源见 provenance.json；归档字节见 sha256.json，审计见 audit.json。
归档不包含真实密码、真实桌面、EXE/PDB 或 dump。VM 验收结果如后续补充会
单独记录，不从本机回调结果推断。

## API 参考

- [Microsoft RegisterRawInputDevices](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-registerrawinputdevices)：设备类注册在进程内只有一个目标；移除注册要求 NULL HWND。
- [Microsoft GetRegisteredRawInputDevices](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getregisteredrawinputdevices)：读取本进程当前设备注册。
- [Microsoft LowLevelKeyboardProc](https://learn.microsoft.com/en-us/windows/win32/winmsg/lowlevelkeyboardproc)：回调在线程消息上下文执行，超时可能被静默移除。
