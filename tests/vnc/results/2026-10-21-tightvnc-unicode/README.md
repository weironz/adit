# TightVNC 2.8.90：Windows 原生 TextBox Unicode 实测

本轮第二次运行的结果是 **3 项通过、1 项兼容性失败，整体 `outcome=failed`**。ASCII、BMP 中文、组合字符均由真实 Windows TextBox 消费并逐 Unicode scalar 匹配；非 BMP emoji 期望 `U+1F60A`，实际输入为 `U+F60A`。原始失败未改写成通过。

测试使用生产 `vnc_fixture_driver` 的 `TEXT` 路径连接独立的 TightVNC application-mode 实例。服务端仅监听 `127.0.0.1:15911`；每个样本启动独立的原生 TextBox 目标。检查前台 PID、原生 GUI focus HWND、目标实例、实际输入计数和 TextBox.Text 的 scalar 数组。没有使用剪贴板替代键盘输入，也没有往 TextBox 直接填入期望值。此记录不等于 Windows IME 原生候选/组合全过程验收。

| 样本 | 期望 scalar | 实际 scalar | 第二轮结果 |
|---|---|---|---|
| ASCII | `U+0061 U+0062 U+0063` | 相同 | passed |
| BMP 中文 | `U+4E2D U+6587` | 相同 | passed |
| 非 BMP emoji | `U+1F60A` | `U+F60A` | **failed** |
| combining | `U+0065 U+0301` | 相同，未做 NFC 归一化 | passed |

emoji 样本的 driver 退出码、前台目标、原生 focus 和实际输入检查全部通过，只有 `exact_scalars=false`。观察结果与高位截断为 16 位一致；这份档案证明特定版本的实际兼容性表现，不单凭该结果推断所有 TightVNC 版本的内部原因。没有尝试把 UTF-16 surrogate keysyms 或剪贴板当成原键盘路径通过的替代证据。

## 环境与来源

- 实机记录：Microsoft Windows 11 专业版，`10.0.26300`，build `26300`，64 位。
- 已安装、签名有效的 TightVNC `2, 8, 90, 0`，复制到本次自有 payload 后运行。
- 服务端 SHA-256：`68be8f45403afbd5bc2312b39e40689fac9a0f00986f1e5773ba8845b2b050e8`。
- 生产 driver SHA-256：`0024168f5fc9575068c740f3b57ff6a87a45c217f0b50952af853c4efec3fd7b`。
- 测试只用公开合成认证材料；没有归档真实连接凭据。
- 原始 UTC 时间在 JSON 内；本目录日期按本地验收日期命名。

`provenance.json` 为 41 份白名单原始文件/源码副本记录字节数与 SHA-256。原始结果按字节复制，保留原文件名。`source/` 是归档时冻结的两个 fixture 脚本副本；这些副本的 hash 证明本目录保留的源码内容，**原始 VM 结果没有记录脚本执行时的 hash，不能额外宣称 VM 执行文件已通过同一 hash 关联**。二进制 hash 则来自真实运行结果。

## 两次运行及回收

`first/tight-unicode1-first/` 保留第一轮四个样本的 result、原生 actual、driver stdout/stderr；同样是 3 pass、1 emoji fail。第一轮在最终汇总阶段遇到 Windows PowerShell 5.1 GenericList→array 的基础设施异常，没有留下主 `result.json`，因此不能把四份 case 结果当成完整首轮验收。

`recovery/tight-recovery1-failed-utf8.json` 保留第一次独立回收失败：UTF-8-no-BOM 的中文 OS caption 被隐式 ANSI 读取，JSON 解析失败，回收没有执行进程动作，且没有声称移除注册表。修复显式 UTF-8 读取后，`tight-recovery2.json` 记录独立回收 `passed`、退出码 0；10 份精确自有 PID/start-ticks/path 身份均已退出，`terminated=false`，没有未知 payload 进程。自有 HKCU Server 键已移除，fixture TCP/UDP 均为空，原环境与第一轮 preflight 一致。

`second/` 保留修复基础设施后的完整第二轮。主结果明确：

- `cleanup_verified=true`、`original_state_unchanged=true`、`owned_registry_removed=true`，`cleanup_errors=[]`。
- `remaining_fixture_tcp=0`、`remaining_fixture_udp=0`。
- `before == after == preflight.before`，原 TightVNC PID `3420`、`11124` 及启动身份、服务状态、`15910` 原监听、HKLM 键元数据/SDDL 和防火墙状态一致。

第二轮 ownership journal 的 `registry_owned=true` 是运行中取得所有权的记录，保留到最后一次原子 journal 写入；回收结论应读取主结果的 `owned_registry_removed`，不能把 journal 当作清理后的现场快照。

上述“清理核验”均指已保存的实机结果及其独立比对。本目录审计只读这些文件，不重新连接 VM 或重新执行回收。

## 离线复核

在仓库根目录执行：

```powershell
python tests/vnc/results/2026-10-21-tightvnc-unicode/audit.py
```

脚本校验原始文件/源码副本的 SHA-256、实际 TextBox 文本与 scalar、原生前台/focus、独立目标、所有 case 汇总一致性、第一轮失败与独立恢复、第二轮原服务基线与 fixture 清理。审计成功退出只表示 **档案内容满足这些判据且保留已知失败**，不表示四个功能样本全通过；若 emoji 被改写为 passed、实际变成别的 scalar、丢失失败文件或清理结论不匹配，审计会失败。

未归档私有 RDP 帧、截图、命令脚本、bootstrap 文件或其他未授权文件。下一步仍需把此版本的非 BMP 限制保留为明确兼容性边界；不能据此关闭完整 Windows Unicode/IME 兼容性验收。
