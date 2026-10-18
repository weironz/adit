# Windows VNC 服务模式：真实基本兼容证据

[GitHub run 39146480475](https://github.com/weironz/adit/actions/runs/39146480475) 已完成，4/4 jobs success。源提交 897d129b76489501b9db526fe6f4cd2912a85803，分支 codex/vnc-windows-service。固定 UltraVNC 1.8.3.0 官方包 SHA256 为11163a0b9b86321bf6403eb0cf84b81f7230ce32e6291377e1fe5beb987f7e4f。

结论仅涵盖一次性 Windows Server 2025 GitHub-hosted runner 上，独立 GUID 服务 + 正常直接 SYSTEM worker 路径、VncAuth、lossless/native loopback、完整 RGB 及合成 Tk 输入基本兼容。静态校准通过后 static/scroll/drag 各3次，合计 **1次校准 +9次基本样本通过、4次错误密码明确拒绝、4个独立服务完成精确清理**。静态样本没有输入，不能以其 tk_consumption_passed 代表鼠标消费。

| 范围 | Job ID | Artifact ID | 成功连接 | 错误密码拒绝 | SCM root → worker PID | console/worker session | 各样本完整首屏 ms |
|---|---|---|---:|---:|---|---:|---|
| calibration | 117731236093 | 12143259388 | 1 | 1 | 2044 → 8100 | 2 | 1030 |
| static | 117732733435 | 12143712273 | 3 | 1 | 212 → 8212 | 2 | 1003, 1915, 906 |
| scroll | 117732733361 | 12143707325 | 3 | 1 | 8528 → 1440 | 2 | 1064, 1015, 1004 |
| drag | 117732733424 | 12143703508 | 3 | 1 | 5792 → 8044 | 2 | 1010, 937, 1973 |

## 独立复核

audit.json 将下载产物重新解析后核对 SCM Running PID、session0 根进程、直接 worker PPID、较晚创建时间、SYSTEM SID、同一私有 EXE 路径/哈希、active-console session 与 GUID INI 命令，以及15911监听归属和精确程序 UDP Block。每个正例须有原始 probe 的完整 RGB/canary、至少3000ms稳态、末态Connected、0断线及无error；滚动/拖动须有同一 reset generation 的真实 Tk 消费计数。错误密码须 exit1、明确认证失败且没有完整帧。没有 portable fallback 或放宽身份条件。

所有样本 vendor EXE SHA256 为 9ebd1ec90ba1274d4a183f5bf6588374700052da907fa764a2ae7072e8ae19fe；每个job从同一run源提交独立进行locked构建，probe二进制SHA256有差异，逐job记于audit.json；不宣称不同job的probe字节完全一致。正常服务路径参考 [固定官方 UltraVNCService.cpp](https://github.com/ultravnc/UltraVNC/blob/0cd8b41aeb34e52e170b33e24411b3f805e303be/winvnc/winvnc/UltraVNCService.cpp)，仅用于解释配置/进程语义，不宣称官方二进制由该源码提交构建。

## 清理证据边界

原始 service-stop.stdout.txt、service-delete.stdout.txt 保留精确服务控制结果。根/worker退出、无活TCP/UDP、SCM/注册键不存在、最后删除自有规则及两套PolicyStore不存在，均由固定源 wrapper 的运行时断言保证；result.json 无cleanup/capture/abnormal错误。PID0 TIME_WAIT仅为关闭后的内核连接。没有另存清理后的完整系统原生快照。worker和active-console session数值保留；Tk session相同由wrapper当场断言，未另存原生Tk身份快照。

## 下载与仓库哈希

manifest.json 分别记录每个允许文件的**下载原始bytes/SHA256**与**CRLF→LF后仓库存储bytes/SHA256**，两种哈希不能直接混比。audit.json另记完整artifact ZIP独立下载SHA256，已逐个与GitHub API digest比对一致。只将允许文本中的CRLF变为LF，其余字节不改，空stdout/stderr也保留。归档包括result/identity/listeners/rule/target/load/probe/SC stop-delete/runnerlog；排除fixture.ini、认证日志、vendor日志和安装配置。

## 未覆盖

不证明UAC、锁屏/登录、物理输入、Adit原生UI最终呈现、键盘/IME、剪贴板、DPI、睡眠、弱网或服务长期稳定性；不是每档/每工作负载n10，也不输出p50/p95/p99。本记录不改变其他失败实验的结论。
