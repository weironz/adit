# #89：Windows RDP 前台输入与失焦释放

## 环境与方法

- 2026-11-25（下表时间为 UTC）。Windows Server 2022；同机真实 RDP 服务 `127.0.0.2:3389`。Adit v0.1.78 发布版运行在本地控制台（会话 1），测试账户登录到独立的 RDP 会话 2。验收未使用从本分支编译的二进制。
- 管理员可用 `Start-Observer.ps1` 编译 `KeyObserver.cs`、创建临时账户并注册登录任务。通过 Adit GUI 登录该账户，将密码仅填入连接凭据界面；脚本把密码以当前用户的 DPAPI 密文保存在仓库外。测试结束后注销会话并移除任务、账户、测试目录及密文。
- `KeyObserver` 在 RDP 会话内用低级键盘钩子记录白名单内的按键边沿，用 `GetAsyncKeyState` 记录修饰键状态变化；`remote-observer.txt` 首行的 `session=2 remote=1` 标识取样地点。控制台另外运行同一观察器，`local-observer-excerpt.txt` 保存对应时段的原始行。它不记录文本、窗口标题或任意按键。
- 让已连接的 RDP 标签页位于前台，每次运行 `Invoke-FocusLoss.ps1 -Key Ctrl`（也分别运行 `Shift`、`Alt`）。脚本用 `keybd_event` 注入本地按下，在按住时最小化 Adit，随后才注入本地抬起并请求恢复焦点。比较 `focus-driver.txt` 与两端观察器的 UTC 时间。远端 `async mask=00` 表示所有受监测修饰键都已抬起；观察器只在状态变化时记一行。

## 结果

| 项目 | 状态 | 会话内证据 |
| --- | --- | --- |
| 按住左 Ctrl 后最小化 | 已验证（合成输入） | 13:10:55.075 远端抬起，状态归零；先于 13:10:56.455 本地抬起 |
| 按住左 Shift 后最小化 | 已验证（合成输入） | 13:17:19.797 远端抬起，状态归零；先于 13:17:21.159 本地抬起。13:11 的首次尝试未记录到完整边沿，不计入通过 |
| 按住左 Alt 后最小化 | 已验证（合成输入） | 13:12:02.544 远端抬起，状态归零；先于 13:12:03.947 本地抬起 |
| 上述三次恢复焦点 | 已验证（合成输入，观察时段内） | 本地日志显示前台回到 Adit，远端在对应恢复时段没有再报告非零状态 |
| 按住 Win 后失焦；点击其他本地窗口或用本地 Alt+Tab 切走时的释放 | 未验证 | 无完整的远端按下、释放与恢复焦点记录 |
| 前台全局钩子转发 Win、Alt+Tab、Ctrl+Esc、Alt+Esc | 未验证 | 没有可用于验证钩子转发的非注入输入；13:17:02 的 Alt+Tab 是本地注入对照，不计为远端成功 |

## 局限

两种客户端输入方式（自动化桌面工具和脚本的 `keybd_event`）在本地都被标记为 `LLKHF_INJECTED`。Adit 的低级钩子会跳过这类输入；远端经 RDP 收到的按键可以显示 `injected=False`，但这个标志不能证明客户端输入来自实体键盘，也不能证明经过了 Adit 的全局钩子。最小化三项的结论依据远端先于本地抬键而归零，不外推到 Win 键、右侧修饰键或其他失焦方式。本次没有确认需要改动的产品缺陷，#89 的剩余范围没有全部覆盖。

## 补测：用外层 RDP 提供非注入事件

计划是在会话 A 内运行 Adit，把连接到会话 B 的 RDP 标签页切到全屏前台。外层使用仓库已有的 `crates/adit-rdp-proto/examples/rdp_probe.rs` 向 A 发送 `KEY <扫描码> <扩展位> <按下位>`，不用 `SendInput` 或 `keybd_event`。用 `Start-Observer.ps1` 分别创建测试账户 `adit89a`、`adit89b` 及各自的登录观察器。用 `Invoke-LoopbackProbe.ps1`（指定探针、辅助进程、会话和命令文件）从控制台连接 A 的 `127.0.0.2:3389` 或 B 的 `127.0.0.3:3389`；脚本仅通过标准输入交付凭据。用于单会话对照的命令是 `nested-control-keys.txt`。

- **输入来源对照通过。** 两个账户分别登录时，`nested-observer-A.txt`（会话 3）及 `nested-observer-B.txt`（会话 4）均记录到由 RDP 服务器送到会话里的 Win、Alt 和 Tab，标记为 `injected=False`。两边的前台记录还捕获了 Win 后的 `SearchApp` 和 Alt+Tab 期间的 `explorer`，因此这些仅是本地 shell 会响应的对照，不是 Adit 的转发或拦截证据。
- **双会话被 Windows 登录限制阻断。** Administrator 控制台及 A 活动时，B 的 RDP 握手和画面传输成功，但画面是 Windows 登录提示 “There are too many users signed in”，要求先断开 Administrator 或 A（`nested-b-sign-in-limit.png`），B 的观察器当时尚未启动。A 断开后，B 使用相同地址和凭据成功登录。保持 B 活动后重连已经登录、仅处于断开状态的 A，仍出现相同提示，要求断开 Administrator 或 B（`nested-a-sign-in-limit.png`）。`nested-windows-state.txt` 保存 Windows 会话事件。此 VM 未安装 RD Session Host；没有断开 Administrator、选择踢掉另一会话或更改系统角色。两个环回地址由同一个 Windows 实例服务，不能让 A 和 B 同时活动。
- **未验证项不变。** A 中启动了发布版 Adit，但未达到“全屏 Adit 前台且已连接 B”的前提。未验证任一组合键在 A 被钩子拦截并到达 B，也未验证嵌套路径中按住 Ctrl、Shift、Alt、Win 后通过本地 Alt+Tab、会话内辅助窗口或最小化失焦，以及恢复后的状态。继续需要第二台可用的 Windows RDP 主机，或可在不中断控制台的条件下保持两个测试会话活动的 Windows 环境。
