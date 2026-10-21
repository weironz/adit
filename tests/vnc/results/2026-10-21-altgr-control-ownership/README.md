# AltGr / 真实 Ctrl / 本机预持 Win：原生验收证据

第一版 `witness` 保存于 `rejected/`，状态是 **rejected / incomplete，不能作为最终 #210 验收通过**。四项 UI backlog 修饰键所有权样本通过；随后系统键回归中 Win+R、右 Win+E、Alt+Tab 的 12 条 wire 事件成功，Ctrl+Esc 没有进入 peer，并丢失前台和抓取。修复后的独立运行必须使用新 prefix 和新目录保留结果，不能覆盖这批失败。

## 第一版已证明的内容

测试使用真实 Windows 前台窗口、独立 LL hook 工作线程、原生 RDP scancode 输入、生产 dispatcher 和 SessionManager，peer 原始 KeyEvent 为最终 wire 证据。fixture 的 F8 只在显式测试开关开启时同步阻塞前台 UI 线程 1500 ms，不进入 peer；生产 hook 工作线程继续处理。

| 样本 | backlog 前缀（hex scan / extended / down） | wire 与阶段断言 |
|---|---|---|
| pure AltGr | `38/1/1` | `AG↓ @↓ @↑ AG↑`，没有 Ctrl；最终 down-set 为空 |
| 左 Ctrl held | `1d/0/1 → 5b/1/1 → 5b/1/0 → 38/1/1` | `CL↓ Win↓ Win↑ AG↓`；AG 释放后 CL 仍 held；真实 CL 释放后为空 |
| 右 Ctrl held | `1d/1/1 → 5b/1/1 → 5b/1/0 → 38/1/1` | `CR↓ Win↓ Win↑ AG↓`；AG 释放后 CR 仍 held，无额外 CL；真实 CR 释放后为空 |
| grab 前预持本机 Win | hook/grab 关闭时真实 Win↓，随后启用 grab；stall 内 `1d/0/1 → 38/1/1` | start→end 没有新增 peer KeyEvent；Ctrl/RightAlt 释放也不制造远端孤儿 release |

`CL=0xffe3`、`CR=0xffe4`、`Win=0xffeb`、`AG=0xfe03`。德语布局由原生 IME/HKL 快照核对。每项 start/end 都证明前台归属、相同 epoch、抓取开启；dispatcher 与前台线程相同，hook worker 为另一个线程。

四次真实 stall 的同 VM 时间区间与下游观察器可见 LL 边沿：

| 样本 | stall start/end，Unix ms | 可见真实边沿，Unix ms | 捕获键计数增量 |
|---|---|---|---:|
| pure | `1792525118080..1792525119581` | RightAlt `1792525119440` | 0 |
| left | `1792525155706..1792525157207` | 左 Ctrl `1792525156995`；RightAlt `1792525157000` | 2 |
| right | `1792525198581..1792525200082` | 右 Ctrl `1792525199868`；RightAlt `1792525199875` | 2 |
| preheld | `1792525307362..1792525308863` | 左 Ctrl `1792525308640`；RightAlt `1792525308646` | 0 |

生产 hook 已截断捕获的 Win，因此下游 observer 看不到 Win down/up。Win 的命令顺序、hook captured/swallowed 计数与 peer 原始顺序被保留，**不声称有逐条 Win LL 时间戳**。真实 Ctrl 和 RightAlt 的可见边沿均严格在对应同 VM stall 区间内，不用宿主机收件时间推算它们。

## 握手及时间记录的边界

`*-handshake.json` 保存实际 F8 start/end 观察和 burst 内容，`*-burst.commands.txt` 保存顺序，其中 `[await real ui_stall_start]` 是证据注释，不能直接作为输入协议命令执行。控制程序的路径是在 watcher 输出 observed 后，由 Node data callback 解析记录并同步写 burst；此因果说明来自运行控制程序，未在本目录另存其实现。

右 Ctrl 握手的 `received_unix_ms(start)=1792525199417`，而 burst command 时间为 `1792525199416`，相差 1 ms。两字段分别由 Python `time.time` 与宿主 Node `Date.now` 采样，不能据此断言精确 wall-clock 先后。审计保留差值，不引入容差把记录改写成 receipt≤burst。严格 backlog 判据使用前述同 VM 的 app/observer 时间。后续运行若增加宿主单调 elapsed，可另外证明 callback→write 顺序。

原始日志没有逐次 repair 分支命中证据。因此这里只声称原生行为符合目标；精确 generalized `real Ctrl → Key* → RightAlt` 分支另由确定性队列/wire 单测覆盖。此矩阵不扩大为全部普通 WM 文本的全局排序保证。

## 失败与归档范围

`witness-core-start` 到 `witness-core` 仅新增 Win+R、右 Win+E、Alt+Tab 的 12 条 peer KeyEvent，hook 捕获计数增加 10；Ctrl/Escape 的真实 down 在 observer 出现，但没有对应 peer wire。末尾 app 日志为 hook `armed=false`、`foreground=false`。原生操作观察为本机 Start 被触发；本目录未包含私有截图，仅用上述文本证据审计“不完整、不能通过”的结论。

`rejected/provenance.json` 保存 48 份精确白名单原始文件的字节数与 SHA-256。包括 11 个阶段的 app/peer/observer 三件套、4 次 handshake、11 份只含测试键输入的命令记录。原始文件按字节复制，没有归档截图、RDP 帧、EXE、认证信息、启动/bootstrap 命令或其他私有文件。

## 可复用离线审计

第一版命令在仓库根目录运行：

```powershell
python tests/vnc/results/2026-10-21-altgr-control-ownership/audit.py
```

成功退出只表示已归档证据满足“4 项 modifier 样本通过、core 失败”的判据；输出分类仍为 `rejected-incomplete`。

复用入口：`audit_matrix(directory, prefix="witness", expected_core="rejected")`。后续候选运行保留相同阶段/后缀文件命名，换 `witness2` prefix、独立目录和新的 provenance；manifest `status` 应为 `candidate`。候选审计命令：

```powershell
python tests/vnc/results/2026-10-21-altgr-control-ownership/audit.py <新目录> --prefix witness2 --expected-core passed
```

`passed` 模式在相同四项严格断言之外，要求完整 **20 条 core wire**：Win+R、右 Win+E、Alt+Tab、左 Ctrl+Esc、右 Ctrl+Esc 各四条；左右 Ctrl 分别使用 `0xffe3` / `0xffe4`，不能过滤新增事件。捕获计数须增加 14（8 条 Win 组合边沿、2 条 Tab、4 条 Escape），最后仍在前台且抓取开启。四个新握手还必须提供同一宿主 Node 时基的 `node_receipt_monotonic_ms` / `commands.monotonic_ms`，严格验证 trigger→receipt callback→burst write→end receipt 的单调顺序；不把它当作 VM 时钟。它不会因为换 prefix 就把已有 12 条 core 记录算作通过；最终验收仍需与新构建及本地 CI 的来源对应。
