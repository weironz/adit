# #210 未决重音键与前台系统键：无状态布局转换

本轮解决 **标准德语布局的未决 acute → Win+R / Win+E**：真实 Adit 前台窗口的
系统组合键完整送到合成 RFB peer，随后普通 e 仍合成为 é。没有清空或重放
本地组合状态。**本轮 AltGr 暴露额外 Ctrl 留存，不能记为干净回归通过，#210 保持开放。**

## 已证实的产品行为

| 场景 | 原始证据及结果 |
| --- | --- |
| acute → 左 Win+R → e | `stateless-pending-r*`：Win/r 四条事件；随后 é 一对。 |
| acute → 左 Win+E → e | `stateless-pending-e*`：Win/e 四条事件，没有误发 é；随后 é 一对。 |
| acute → 右 Win+R → e | `stateless-german-extra*`：右 Super/r 与后续 é 完整。 |
| 德语布局字符 | 同组物理 Y/Z 送 z/y；Caps+Win+1 送 !，按行 CAPLOK 规则生效。 |
| 英语与微软拼音底层布局 | `stateless-english-and-switch*`、`stateless-chinese-grab*`，实际窗口 HKL 与 Win+R wire 对照。 |
| 抓取开启后的微软拼音 | `stateless-chinese-preedit.png` 有真实 `zhongwen` 预编辑和“中文”候选；预编辑无远端字符，空格提交两字各一对。 |
| 核心组合与本地逃生 | `stateless-core*`：Alt+Tab、右 Win、左右 Shift 两种顺序、持 Enter 后再按 Ctrl/Alt、Ctrl+Alt+Enter。 |
| 失焦及重新启用 | `stateless-before-focus*` / `focus-away*` / `refocus*` / `final-rearm*` / `final*`：失焦解除，回焦不自动抓取，显式重新启用后 Win+R 通过，最后逃生。 |

输入来自授权 Windows VM 的 RDP 扫描码；独立 OS observer 保留 noninjected
标记。生产 hook、dispatcher、SessionManager 和实际工具栏参与测试。
所有 **130 条远端键事件**逐阶段原样保留。不是物理键盘硬件重复认证，也不是
厂商服务端应用文本消费。后者另见 [Linux 三厂商 12/12](../2026-10-21-linux-unicode-text/README.md)。

## 原因和实现边界

`ToUnicodeEx(flags=4)` 不修改组合状态，但仍读取已有 dead-key 状态。
在 acute 未完成时，R 得到 acute+r 两个字符；E 则可能只返回 é，不能用
“只接受一个 scalar”可靠避开误译。[Microsoft ToUnicodeEx 文档](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-tounicodeex)。

`keyboard_vnc_layout.rs` 从 Windows 系统布局 DLL 的 `KbdLayerDescriptor`
复制 WDK 表，再按 VK、Shift、每行 Caps 和物理 AltGr 列查询。
结构依据 [Microsoft 官方德语布局样例](https://github.com/microsoft/Windows-driver-samples/blob/main/input/layout/all_kbds/kbdgr/kbdgr.c)。
只接受精确 HKL `04070407`、`04090409`、`08040804`，并核对其注册表 Layout File
分别为 KBDGR/KBDUS/KBDUS；没有从自定义 HKL 猜 KLID。
当前只启用64位后端。其他布局保留原翻译路径，**没有因此取得未决重音隔离保证**。

布局 DLL 仅从绝对 System32 路径加载。指针须在同 DLL 的已提交可读区域内，
表数、行数、列数和 stride 均有限；复制后卸载 DLL，缓存中没有原生指针。
NLS、ligature、未知行属性、dead-key 单元及 surrogate 明确拒绝。
命名键保留原路径。准备工作位于 hook worker、安装 hook 之前；UI 和 timed
callback 不读取注册表/加载 DLL，不通过清理用户 dead-key 状态凑出结果。

VM 中独立原生测试还在本地 acute 已未决时**冷准备布局**，通过实际生产
`vnc_chord::translate` 查询 r/e/€，最后验证原队列的 e 仍得到 é。
`stateless-integrated-dead-test.json` / stdout：退出0，实际1 passed、0 ignored。
被测 native test EXE SHA256 `8664988cc57c77e44ad1c8bc0d1dc52321d8642d9ca743e50ca9cc3035b26084`；
GUI EXE `1e6b48cf97d4f8bc0a7fabf30ec9944528d609568ce24e4e5f299e4a3245e9c2`。

## 明确保留的异常和被排除方案

- **AltGr 未通过本轮干净回归。** `stateless-altgr*` 的 @/€ 字符正确，但第一次
  AltGr 之前多发了左 Ctrl；它到随后真实 Ctrl+A 的 Ctrl 释放才恢复，真实 Ctrl
  down 还有重复记录。Win+AltGr 后也留下 Ctrl，后续本地逃生才清理。全部130条
  保留，审计明确输出 `incomplete_stray_ctrl`。这与此前独立轮次的干净通过不同；
  未确定根因，不把它归因于布局表，也不把“最终全部释放”当成中间正常。
- `rejected/dead-isolation2*`：独立线程/消息队列仍继承未决 acute，实际测试失败。
  `rejected/dead-rejected-worker.rs.txt` 是已撤回原型，没有接入产品。
- `rejected/dead-desktop-probe*`：独立未显示桌面也读到 acute+r / é；
  原线程仍可完成 é，临时桌面和线程均回收。没有 SwitchDesktop 或 AttachThreadInput。
- 测试准备曾在 native GUI 退出后尚无有效前台控制台时发送命令，ACK 不代替执行；
  另一次直接执行被 PowerShell execution policy 拒绝。对应 commands 保留，
  只有最终有独立结果文件的 `stateless-recover` 计为原生测试通过。
- 本地 CI 第一轮因正在运行的 `rdp_probe.exe` 无法覆盖而停在 LNK1104；关闭控制
  驱动后重跑。下一轮两个旧 wire 测试未调用新的缓存准备步骤，得到
  `layout-not-prepared`，完整失败日志保留于 `ci-cache-preparation-failure.txt`。
  只在 `cfg(test)` 增加显式准备，生产 UI 翻译路径仍不能懒加载 DLL。

## 来源、检查与回收

`provenance.json`、`source/` 保留被测生产文件和二进制哈希，源码 SHA 使用 LF
规范化，同时保留原始字节哈希。GUI 构建时 workspace 经过 rustfmt；其余44个
文件只有无关格式变化，已在最终本地 CI 前撤回。GUI/native 测试之后只新增
`cfg(test)` 的旧 wire 测试准备，不改变发布路径行为。最终 CI 单独保留日志。

最终产品提交 [`ff84b344d532e4a5ecc3cad9a8124ea35ea96d1e`](https://github.com/weironz/adit/commit/ff84b344d532e4a5ecc3cad9a8124ea35ea96d1e)，
`final-source/` 及逐文件哈希与该 Git blob 对照。审计额外证明 GUI 后的 hook 差异
只有 `cfg(test)` 准备 helper。Rust/Cargo 1.98.0，完整 `just ci-vnc` 退出0：
root **430 passed / 0 failed / 11 ignored**、feature **145/0/4**、graphics **202/0/0**，
计数重叠。两 workspace 与 feature 全 targets Clippy `-D warnings` 通过；
SSH integration 仅编译。四个 ignored native 测试并未全部在此轮执行，只有本轮
新的隔离 gate 在 VM 显式执行；此前 native3项通过保留为此前证据。

`audit.py` 独立复算完整 wire、实际 HKL/IME、epoch/前台/抓取、异常 Ctrl、
原生测试执行、被排除方案、二进制来源、哈希和精确资源清理；不操作桌面或网络。

虚拟机完整语言列表已恢复到原始 zh-Hans-CN，包括 InputMethodTips、Spellchecking、
Handwriting。独立复查无自有进程或15915监听；精确 GUID 共享、任务、目录随后
全部移除。WinRM、防火墙保持基线；没有账号/密码或网络配置改动。
`stateless-independent-before-cleanup.json` 和 `stateless-cleanup-reports.json`
是回收依据。现有 TightVNC 服务未停止或修改；本轮没有运行新增 TightVNC 文本夹具。

没有消耗 GitHub Actions，没有合并或发布。#210 的首次 AltGr Ctrl 留存和更广布局
保证继续开放；#205 厂商兼容边界、#212 外部唤醒、#223 真实 ClearCodec dump 回放
不由本次合成 peer 证据替代。
