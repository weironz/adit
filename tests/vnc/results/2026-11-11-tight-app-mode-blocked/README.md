# TightVNC 应用模式在本机无法监听：环境阻塞，不是兼容性结论

2026-11-11 尝试在开发工作站（Windows 11 10.0.26300，会话 1 为 RDP）用
`tests/vnc/windows-vm-tight-unicode.ps1` 的分阶段 ACK 版本复跑
[#205](https://github.com/weironz/adit/issues/205) 的兼容模式实机验收。
**服务端二进制与签名全部核对通过，但两个厂商的用户态服务器都没有建立监听，
因此没有任何一个 TEXT 用例被执行。** 本目录是这次阻塞的原始记录，不是通过，
也不能替代 VM 上的验收。

| 检查 | 结果 |
| --- | --- |
| TightVNC MSI 下载与解包 | `msiexec /a` 解包，未安装、未注册服务 |
| `tvnserver.exe` SHA-256 | `68be8f45403afbd5bc2312b39e40689fac9a0f00986f1e5773ba8845b2b050e8`，与夹具固定值一致 |
| 文件版本 / Authenticode | `2, 8, 90, 0` / `Valid`（OOO GlavSoft） |
| 生产 driver | `vnc_fixture_driver.exe` SHA-256 `76225fcf520f830585dc7d3a42687b6e94764cbb7a7a398081fb810d16d7d19f` |
| 夹具结果 | `outcome: failed`，`error: TightVNC app listener readiness failed`，`cases: []` |
| 清理 | `owned_registry_removed: true`、`original_state_unchanged: true`、`cleanup_errors: []` |

20 秒内 `127.0.0.1:15911` 没有 TCP 监听。`tvnserver.exe` 根进程与其子进程
都存活、无对话框，但从不绑定端口；夹具按设计在就绪检查处失败并在 finally 中
回收自有进程与 `HKCU\Software\TightVNC\Server`（本轮新建，删除后父键也一并
移除，复查无残留）。

为区分"厂商特例"与"环境不支持"，同样的方式和官方 1.8.3.0 归档
（SHA-256 `11163a0b9b86321bf6403eb0cf84b81f7230ce32e6291377e1fe5beb987f7e4f`，
`x64/winvnc.exe` 文件版本 1.8.3.0）跑了一次 UltraVNC 用户态服务器：
进程存活、零监听。其 `DebugLevel=9` 日志
（`ultravnc-1830.log.txt`）停在 `DesktopUsersToken::getDesktopUsersToken`，
没有进入监听阶段。两个厂商以相同方式失败，指向宿主环境而不是某一个实现。

## 环境事实，不扩大为结论

- 当前账户 **不是管理员**（`net session` 拒绝），两个服务器都以用户态启动，
  都未安装服务、未改防火墙。
- 会话 0 是 services，本线程在 **RDP 会话 1** 运行，物理 console 会话 11 无
  登录用户。两个服务器都是在**非控制台、非提升的 RDP 会话**里初始化桌面
  hook 与监听。
- 因此本轮**没有**取得任何 Windows 服务端应用消费样本；24 次幽灵 release
  的分阶段归属仍然未做，兼容模式的实机计数仍为未验收。
- 未发布版本，未在 GitHub Actions 上运行任何任务，未修改产品运行时行为。

复跑条件：一次提升权限的**控制台**会话（或 #205 原本的隔离 VM），把
`tvnserver.exe`、`vnc_fixture_driver.exe` 放进
`AditVncKeys-[a-f0-9]{32}\payload`，在 PowerShell 5.1 下执行
`windows-vm-tight-unicode.ps1 -Directory … -ResultName … [-BmpCompatibility]`。
宿主的 `PSModulePath` 混入了 PowerShell 7 的模块目录，会让
`Get-AuthenticodeSignature` 无法加载，运行时需把
`PSModulePath` 收窄到 `WindowsPowerShell` 的两个目录。

`tight-unicode-default.result.json` 是夹具原样输出（CRLF 已归一为 LF），
含一次性 registry 所有权标记、路径与哈希，**不含密码、凭据、截图或桌面像素**。
