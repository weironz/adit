# TightVNC Unicode：原始失败结果与兼容策略证据

这里归档本轮已经取得的原始 VM 结果，不重新定义通过条件。所有 JSON 的 `outcome: failed` 保持原样；没有新建环境，也没有复跑 VM。测试目标为 TightVNC 2.8.90 应用模式及隔离的合成 Windows TextBox，经生产 SessionManager 的 `TEXT` 入口输入，不能据此声明中文 IME 原生验收通过。

| 模式 | 案例 | 原始结果 | 观测 |
| --- | --- | --- | --- |
| 默认完整 Unicode | ASCII、BMP 中文、组合符 | 3 项通过 | 接收标量与输入完全一致 |
| 默认完整 Unicode | 非 BMP 表情 | 1 项失败 | 输入 U+1F60A，实际 U+F60A，发生截断 |
| BMP 兼容模式首次验收 | ASCII、BMP 中文、组合符 | 3 项通过 | 接收标量与输入完全一致 |
| BMP 兼容模式首次验收 | 单独表情、`abc+表情+xyz` | 2 项失败 | 实际文本均为空，press、keypress、text_changes 均为 0，拒绝标识及 notice 存在；release 均为 24，严格计数条件未通过 |

两份顶层结果均为 **failed**。兼容模式的首次脚本把建连、输入和关闭的生命周期计数混在一个最终快照中，无法根据这份证据把 24 次 release 精确归属到输入阶段或生命周期边沿。不得忽略、扣除或容忍这 24 次 release 后宣布通过；“文本确实没有输入、拒绝提示存在”只是一部分证据，不能等同于实机最终验收通过。原 JSON 同时保留最终计数、真实焦点检查、清理验证及原有服务状态比较。

`unicode-bmp-emoji-stdout.txt` 和 `unicode-bmp-mixed-stdout.txt` 原样保留，包括当时 Rust UTF-8 输出经默认代码页读取形成的 notice 乱码。ASCII 拒绝标识 `text_rejected=unicode_compatibility` 可见，不能把原日志解释成已经验证了中文提示显示正确。

后续 fixture 已显式设置 stdout/stderr UTF-8，并加入有 token 校验和 flush 的两阶段 ACK：连接完成后取 `connected_before`，TEXT 后取 `connected_after`，最后单独记录关闭后的 `after`；连接、输入和关闭计数分别计算。拒绝输入需要 connected 输入阶段四项计数均零增量且文本为空。该脚本目前只完成 **PowerShell AST 与内存 helper 的双 ACK、stdout 收集、EOF 拒绝检查**，**未在 VM 复跑**。本次环境租约及共享已回收，当前停止于此边界。

归档文件只有四份命名原始日志、当前源码哈希清单、本说明及子目录 SHA 清单。JSON 字段经封闭白名单检查，文本字段只允许本测试的公开合成样本；stdout 仅允许 readiness、拒绝标识和 notice。未复制密码、凭据库、可执行文件、屏幕截图或真实桌面像素。原始临时目录与运行元数据保留在 JSON；其中 registry owner token 是一次性清理所有权标记，不是认证凭据。

`current-source-manifest.json` 记录当前 driver、PS fixture、VNC transport 和真实 RFB 回归测试四份源码的 SHA-256，代表 **ACK 修订之后的当前源码**，不冒充旧 VM 运行时二进制身份。旧运行实际 driver 的哈希仍由原始 JSON 的 `driver_sha256` 保存。`SHA256SUMS.txt` 仅覆盖本子目录，不修改上一级验收代理的清单。
