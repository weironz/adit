# witness2：四项修饰键 backlog 与五组系统键原生结果

本轮独立审计通过：四次真实 1500 ms 前台 UI stall、纯 AltGr、左右 Ctrl held 所有权、本机预持 Win 隔离，以及 **20 条精确系统键 wire** 均满足判据。独立清理及完整语言恢复证据已追加。目录保持 `candidate` 分类来保留验收运行身份：构建和实测时来源是基线 `63092a6` 加未提交补丁，随后冻结源代码提交为 `3f9dadba1b3fa1b8bd8008bb9a7224472c2ff0f8`；不把后续提交号伪装成实测时已经存在的程序指纹。

先前 `../rejected/` 原始文件全部保留，Ctrl+Esc 失败没有被覆盖。

## 来源

- 最终 native fixture SHA-256：`625032eff0414b9c7b373efc803db61412b5aeaffa9503a94092ab562c48e64c`；`witness2-native-ready.json` 与私有 EXE 的独立文件 hash 相符，app PID `11308`，交互 session `2`。
- 首版 binary 的独立文件 hash：`876464e832fc34099facfe8b92c5285d5d48b58bc3524fbcb0466ec5daf19a6a`；仅记录 hash，未复制 EXE。
- `witness2-native-build.txt` 保存本地重新编译 native fixture 的完成日志。
- `witness2-ci-vnc.txt` 保存本地 CI 日志，所有保存的 test result 摘要均为 `ok / 0 failed`。ignored 项仍保持 ignored；该日志不变成额外实机覆盖证据。
- `source/` 按明确白名单冻结 10 个 Rust 文件：7 个 UI 键盘/fixture 文件及 `egfx.rs`、`session.rs`、`clearcodec_dump.rs`。每份快照记录字节数与 SHA-256；主控确认这些源文件在最终构建和实测期间保持冻结。native-ready 自身未输出源文件 hash，因此不把源码快照额外包装成程序内置提交指纹。
- `witness2-languages.json` 保存原测试语言配置，四 stall 各自仍以 app 原生 HKL 状态确认德语布局。

`provenance.json` 收录 73 份白名单原始证据/源码快照（含后加的清理/语言/最终 control JSON、3 份独立核验与清理脚本来源及独立 CI 分组计数）；原始内容按字节复制。没有归档截图、RDP 帧、真实凭据、EXE 或启动/认证 bootstrap 脚本。根 `.gitattributes` 的 `* -text` 是仓库传输属性，防止 Git 换行转换破坏原始字节，不属于原始证据清单。

保存的本地 CI 分组如下。root 内含 UI 默认测试，feature 会重跑 UI，所以不能把三行相加当成互不重复的独立测试数。

| 日志分组 | passed | failed | ignored | 说明 |
|---|---:|---:|---:|---|
| root workspace | 439 | 0 | 11 | 33 份 test result 摘要，包含 UI 默认 154 项 |
| feature UI | 154 | 0 | 4 | 第二次 UI test binary，与 root UI 有重叠 |
| isolated graphics | 202 | 0 | 0 | vendored ironrdp-graphics 独立运行 |

`witness2-ci-counts.json` 是父级按真实命令分段所得的独立汇总（本地退出码 0，没有使用 hosted CI），审计再与保存日志的分组统计交叉核对。

## 实测顺序与断言

本次 **core 先于四 stall**。peer 是累计日志，pure AltGr 前已有 20 条 core 事件；审计先核对所有阶段原始日志为 append-only，再按阶段增量和该次 stall 时间选取事件，不假定整份 peer 的第一条是 AltGr。

core-start→core 的精确 20 条 wire 是 Win+R、右 Win+E、Alt+Tab、左 Ctrl+Esc、右 Ctrl+Esc 各四条；左右 Ctrl 分别为 `0xffe3` / `0xffe4`。生产 hook 捕获计数增加 **14**（8 条 Win、2 条 Tab、4 条 Esc），末尾前台/grab/armed 仍有效，down-set 为空。

| 样本 | stall start/end，Unix ms | 可见真实 LL 边沿，Unix ms | 结果 |
|---|---|---|---|
| pure AltGr | `1792526035924..1792526037425` | RightAlt `1792526037155` | `AG↓ @↓ @↑ AG↑`，无 Ctrl，最终为空 |
| 左 Ctrl held | `1792526064705..1792526066206` | 左 Ctrl `1792526065957`；RightAlt `1792526065963` | `CL↓ Win↓ Win↑ AG↓`；AG-up 后 CL 仍 held；真 CL-up 后为空 |
| 右 Ctrl held | `1792526105705..1792526107206` | 右 Ctrl `1792526106978`；RightAlt `1792526106984` | `CR↓ Win↓ Win↑ AG↓`；AG-up 后 CR 仍 held；真 CR-up 后为空 |
| 预持本机 Win | `1792526184237..1792526185738` | 左 Ctrl `1792526185541`；RightAlt `1792526185547` | preheld-start→end 零新增 peer KeyEvent |

没有单独下载 `witness2-preheld-off-{app,peer,observer}` 文件，也没有补造这些快照。`witness2-preheld-off.commands.txt` 被保留；preheld-start 的累计 app/observer 已证明 Win-down 时 hook 未 armed、grab=false，随后真实启用 grab，Win 持续按住直至本项结束。

四个新 handshake 均有同一宿主 Node 时基的 `node_receipt_monotonic_ms` 和命令 `monotonic_ms`，满足 trigger→start receipt callback→burst write→end receipt；VM app/observer 的真实 Ctrl/RightAlt 边沿另在对应同 VM stall 内。没有混用这两套时钟。生产 hook 截断了捕获的 Win，observer 看不到其 down/up，故只保留命令顺序、hook 计数和 peer 顺序，未声称逐条 Win LL 时间戳。

该矩阵证明当前前台修饰键所有权及捕获系统键的相对顺序；不扩大为普通 WM 文本的全局排序，也不单凭 wire 证明每个 native 样本实际走过 repair 分支。

## 复核

仓库根目录执行：

```powershell
python tests/vnc/results/2026-10-21-altgr-control-ownership/audit.py tests/vnc/results/2026-10-21-altgr-control-ownership/candidate --prefix witness2 --expected-core passed
```

审计只读归档文件，输出仍保持 `classification=candidate`。

## 独立清理与语言恢复

`cleanup/` 保存实际 VM 的独立恢复核验、原始语言 baseline/restored、两阶段 cleanup report、native stopped 结果，以及三份说明来源的只读核验/回收/清理脚本副本。审计核对完整语言对象（LanguageTag、InputMethodTips、Spellchecking、Handwriting）与原 baseline 相等，只有 `zh-Hans-CN`；不以仅比较 LanguageTag 代替完整还原。

独立 pre-cleanup 时自有 payload 进程和 15915 fixture 监听已为空，只余本次自有任务。随后两阶段报告显示：准确自有目录 `C:\ProgramData\AditVncKeys-f6a76343ddc648689e1c9ff5efe2b1e5` 已删除、自有 SMB share 不存在，任务/进程/监听均为空；防火墙在各阶段完全相同，WinRM 保持 Stopped。原始 owner 名称/绝对目录核验、reparse point 拒绝、被调用 cleanup 源 hash 核验可在 `cleanup/source/witness-cleanup-final.ps1` 查看；没有把未归档的 owner 原始内容宣称为独立字节比对结果。

`cleanup/witness-control-final.json` 另保存最终 native control driver PID 3004 的退出码 0、真实 stderr `helper_exit=exit code: 0`、helper SHA-256 与 capture=false。它与此前 #223 ClearCodec 采集连接的 helper code 1 是不同连接，不改变那份失败/未解释记录。

这些是已保存的现场报告的独立审计，本脚本没有重新连接 VM 或重跑清理。新提交号通过主控报告追加到 provenance，构建时基线加补丁事实仍保留。[source-commit-verification.json](../source-commit-verification.json) 是父级独立执行 Git blob 核验的记录：十份冻结源码与 `3f9dadb` 对应文件在仅 CRLF→LF 规范化后字节一致，原始 snapshot hash 与 Git blob hash 均保留，未声称二者原始换行字节相同。
