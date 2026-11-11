# UltraVNC 实机分阶段验收：兼容模式 5/5 通过（含非 BMP 整段拒绝）

2026-11-12 在授权 VM `WIN11-VM`（Windows 11 10.0.26300）的**控制台会话**里，
用生产 `SessionManager` 的 `TEXT` 入口对官方便携 UltraVNC 1.8.3.0 输入，目标
是同一桌面上的真实 WinForms TextBox。夹具为
[`tests/vnc/windows-vm-unicode-staged.ps1`](../../../windows-vm-unicode-staged.ps1)，
本目录是它一次连续运行的原始输出。

| 身份 | 值 |
| --- | --- |
| 服务端 | 官方 `UltraVNC_1830.zip` SHA-256 `11163a0b9b86321bf6403eb0cf84b81f7230ce32e6291377e1fe5beb987f7e4f` |
| `x64/winvnc.exe` | SHA-256 `9ebd1ec90ba1274d4a183f5bf6588374700052da907fa764a2ae7072e8ae19fe`，版本 1.8.3.0，用户态 `-run`，未安装服务 |
| 生产 driver | `vnc_fixture_driver.exe` SHA-256 `a41fc8b15a31b9c02f354c0448702fa102516abef881d710d2ef07629efcf4f4` |
| 会话 / 账户 | 控制台会话 1 / `WIN11-VM\will`，`elevated=False`，PowerShell 5.1 |
| 绑定 | 仅 `127.0.0.1:15911`（服务端另有自己的 UDP 15911 echo，清理后消失） |

## 兼容模式（`--bmp-only`）：5/5 通过

分阶段计数把「建连 / 输入 / 关闭」分开，`input_delta` 只覆盖 `TEXT` 提交这一
窗口，因此拒绝是否真的「一个键都不发」不再被生命周期边沿污染。

| 用例 | 期望 | `input_delta` press/release/keypress/text_changes | 关闭阶段 | 结果 |
| --- | --- | --- | --- | --- |
| ascii | `abc` | 3 / 3 / 3 / 3 | 0/0/0/0 | passed |
| bmp-cjk | `中文` | 2 / 2 / 2 / 2 | 0/0/0/0 | passed |
| **nonbmp-emoji** | 空（整段拒绝） | **0 / 0 / 0 / 0** | 0/0/0/0 | **passed** |
| combining | `é`（`e`+U+0301） | 2 / 2 / 2 / 2 | 0/0/0/0 | passed |
| **mixed-nonbmp** | 空（整段拒绝） | **0 / 0 / 0 / 0** | 0/0/0/0 | **passed** |

两个拒绝用例的 driver stdout 同时保留 `text_rejected=unicode_compatibility`
与中文提示（见各自 `driver.stdout.txt`），TextBox 文本为空，且没有任何部分前缀。

### 上一轮 24 次 release 的归属

每个用例的 `connection_delta` 都是 `press=1, release=1, keypress=0`：服务端在
客户端建连时向桌面注入了一对**与文本无关**的按键（不产生字符、不改文本）。
输入阶段是精确的（`press == 期望标量数`），关闭阶段恒为 0。也就是说旧轮次
「全连接生命周期混有 24 次 release」来自连接期注入与整轮累计，不是被拒绝的
文本泄漏，也不是关闭时残留；分阶段后输入阶段可以严格断言为零。

## 默认完整 Unicode 模式：2 通过、1 项厂商截断失败、1 项就绪抖动

| 用例 | 观测 | 结果 |
| --- | --- | --- |
| ascii | 未进入输入阶段：`TextBox readiness failed`（窗口 10 s 内未出现） | failed（夹具环境抖动，非产品结论） |
| bmp-cjk | `中文` 精确 | passed |
| nonbmp-emoji | 收到 `U+F60A`（服务端把 U+1F60A 截为 16 位） | failed，**保留为厂商限制** |
| combining | `e` + `U+0301` 精确 | passed |

因此默认模式这次**不宣称整项通过**：`U+F60A` 与此前 TightVNC/UltraVNC 记录一致，
是服务端 16 位限制；ascii 的就绪失败是夹具在冷 VM 上的窗口等待太短，已在夹具里
改成 30 s 且仅对就绪步骤重试一次（重试不发送任何输入），未在本目录的这次运行中
生效。

## 边界

- 只证明 `TEXT` 路径与**这一款服务端**的应用消费；不是 IME 原生输入、不是物理
  键盘硬件认证，也不代表其他厂商/版本。
- 不是长期稳定性、弱网或服务模式验收；服务端为一次性用户态进程，运行后已回收
  （`original_state_unchanged=true`、`cleanup_verified=true`、端口无残留）。
- `actual.json` / `result.json` 含 PID、窗口句柄与该 VM 的合成测试文本；
  **不含密码、凭据、截图或桌面像素**。
