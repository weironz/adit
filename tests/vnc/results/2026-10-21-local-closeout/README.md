# 本地收尾证据（2026-10-21）

本目录只归档已经执行的本地结果，没有新增测试、研究、网络或 VM 操作。七个原始日志按字节复制，失败记录保留；另收录最终 source overlay 和实际清理元数据。产品未因归档修改。

## 最终采用的 CI 结果

`unicode-error-ci-accepted.txt` 是最终成功日志，包含本地 root CI、native feature UI 和独立 graphics 运行。按命令/测试 binary 边界分别统计：

| 分组 | passed | failed | ignored | 备注 |
|---|---:|---:|---:|---|
| root workspace | 440 | 0 | 12 | 33 个 test result 摘要；含默认 UI 测试 |
| native feature UI | 155 | 0 | 5 | 与 root UI 大量重叠 |
| isolated graphics | 202 | 0 | 0 | vendored ironrdp-graphics |

这些组不可相加声称为互不重复的独立测试总数。ignored 仍为未执行；没有消耗 hosted CI 额度。

## 首次 CI 失败

名字为 `unicode-error-final-ci.txt` 的文件实际保存的是此前未通过的一轮，不能因文件名带 final 就当成成功。它在 `tests::vnc_releases_the_original_keysym_after_shift_or_focus_changes` 失败：截至停止点的 partial root 为 **375 passed / 1 failed / 11 ignored**（19 个摘要），后续 native feature/graphics 未运行。最终成功结果见 accepted 日志，未覆盖或删改这份失败。

## #223 公开合成 IPC 结果

`unicode-error-final-targeted.txt` 首先包含两个 wire 定向测试通过；这些与最终 root UI suite 重叠。随后是 **1 个 IPC 父测试，其中启动 closed / eof 两个隔离 worker 模式**，不是 3 个独立测试。

两模式日志均明确：`had_frame=true`、`held_after_error=true`、`input_count=0`、`specific_error_preserved=true`、`shutdown_event_consumed=true`。它证明已有首帧后 Error 时具体错误保留，管道保持打开期间生产全局键盘/排队鼠标等输入没有到达 peer，并分别消费显式 Closed 与实际 EOF。记录属于 synthetic IPC、SessionManager、生产 dispatcher 与 widget tree 结构验收，不是原生 Windows ClearCodec panic 重现，也不是 GPU 文字内容显示测试。

## 完整性与范围

`SHA256SUMS.txt` 覆盖七份原始日志、source overlay、实际清理和最后控制连接元数据，共十份文件；`provenance.json` 同时保存字节数、计数与四份相关当前产品源码的 SHA-256 元数据。没有复制产品源码，也没有宣称这些工作文件 hash 就是二进制内置指纹。主控报告产品已提交本地 `eb118d7`（17 files、尚未 push），该事实单列为 product_commit，未在归档时运行 Git。

归档前在进程内读完白名单日志/metadata，并检查常见凭据赋值、已知 token、Bearer、私钥和 URL 用户密码形式，候选数均为 0。日志内容为编译路径、测试名称和公开合成 IPC 元数据；该检查不构成对任意未知秘密格式的数学保证。没有归档真实凭据、截图、私有桌面帧或其他未列入白名单的文件。

`.gitattributes` 设置 `* -text -whitespace`，用于保持原始日志字节并避免原始输出空白成为仓库检查噪声；它属于仓库传输属性，不属于原始证据。归档时已逐字节及 SHA-256 复核白名单副本与来源一致。

## Linux 补充检查范围

`linux-closeout-clippy.txt` 保留最终 driver ACK 调整之前已成功的 Clippy，未覆盖它。**最终采用 `linux-closeout-final-clippy.txt`**：更新 driver ACK source overlay 后，Ubuntu / Rust 1.99.1 的 root workspace 与 native feature Clippy-only 再次完成，两条 Finished profile，主控确认退出码 0。来源为 `working-source-overlay.json`（私有源 `local-linux-ci-1792527130428/working-source-overlay.json`），其明确列出的 15 份工作源文件在归档时逐个 byte count/SHA 与当前文件一致。Windows 主机为 Rust 1.98.0，工具链不同。本轮没有运行 Linux tests，也没有完成完整 Windows/Linux/macOS 平台矩阵，不能把 Clippy 成功写成这些项目通过。

`linux-clippy-28c8546.txt` 保留首次 deprecated `Atomic::fetch_update` 失败；`linux-unicode-final-ci.txt` 保留后续 non-Windows hook probe 的 unused/dead-code 失败。它们是最终成功前的不同检查记录，均未覆盖或改成通过。

本轮按冻结范围只归档现有证据和复核既有清理结果，不重新建立 VM 夹具。

最后的 [cleanup/README.md](cleanup/README.md) 保留租约已清理资源但目录仍残留、随后精确移除目录的真实顺序，以及最后控制 driver PID 54084 的独立退出确认。没有归档混合了旧 probe deadline 的 stderr。
