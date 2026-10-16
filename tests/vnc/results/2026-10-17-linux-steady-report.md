# Linux VNC 实测结果（2026-10-16 至 2026-10-17）

客户端为 Windows 11 上的 Adit 会话层 debug 探针，固定源码 `d76f7440515b65d1123ea42de338ca2eaa87b09d`。二进制 SHA-256、Rust、容器镜像标识和测量方法见同目录 `2026-10-16-linux-steady-environment.json`，原始 360 条观测保存在 `2026-10-16-linux-steady.json`。

本轮为三种真实 Linux 服务端、两种画质、三种负载、两种网络条件的 36 个组合，每组 10 次新连接；360/360 获得完整像素覆盖并完成随后 5 秒负载窗口，失败 0 次。所有样本的未完成刷新请求最大为 1。首次像素覆盖最多等待 15 秒，完整更新事件可能只含尺寸元数据，因此不能用它代替首屏覆盖。

服务端分别为 Ubuntu 24.04 x11vnc 0.9.16-10、TigerVNC 1.13.1+dfsg-2build2、官方 TurboVNC 3.3.1（build 20260820）。统一 800×600×24 合成画布、标准 VncAuth，无真实桌面或私密剪贴板内容。网络整形仅作用于容器 eth0 出向：增加 180 ms 延迟、5 Mbit/s、0.1% 随机丢包。该配置不等于实测双向 RTT 恒为 180 ms。

| 网络 | 服务端 | 画质 | 负载 | n | 首屏覆盖中位数 ms（最小–最大） | 完整更新字节中位数 MiB |
|---|---|---|---|---:|---:|---:|
| 回环 | x11vnc | lossless | static | 10 | 512（416–1974） | 0.566 |
| 回环 | x11vnc | lossless | scroll | 10 | 1498（430–1846） | 5.889 |
| 回环 | x11vnc | lossless | drag | 10 | 958（634–1236） | 9.849 |
| 回环 | x11vnc | wan | static | 10 | 1137（539–1936） | 0.398 |
| 回环 | x11vnc | wan | scroll | 10 | 780（707–923） | 3.173 |
| 回环 | x11vnc | wan | drag | 10 | 734（596–964） | 4.709 |
| 回环 | TigerVNC | lossless | static | 10 | 126（66–299） | 0.552 |
| 回环 | TigerVNC | lossless | scroll | 10 | 166（112–247） | 9.336 |
| 回环 | TigerVNC | lossless | drag | 10 | 203（129–270） | 14.205 |
| 回环 | TigerVNC | wan | static | 10 | 548（407–959） | 1.630 |
| 回环 | TigerVNC | wan | scroll | 10 | 456（402–806） | 3.894 |
| 回环 | TigerVNC | wan | drag | 10 | 404（257–509） | 4.607 |
| 回环 | TurboVNC | lossless | static | 10 | 202（167–282） | 0.554 |
| 回环 | TurboVNC | lossless | scroll | 10 | 218（166–320） | 9.671 |
| 回环 | TurboVNC | lossless | drag | 10 | 231（182–272） | 16.048 |
| 回环 | TurboVNC | wan | static | 10 | 502（428–720） | 0.394 |
| 回环 | TurboVNC | wan | scroll | 10 | 530（353–1003） | 4.134 |
| 回环 | TurboVNC | wan | drag | 10 | 933（379–1034） | 2.730 |
| 受控弱网 | x11vnc | lossless | static | 10 | 3224（3124–4931） | 0.566 |
| 受控弱网 | x11vnc | lossless | scroll | 10 | 3169（2972–3494） | 2.251 |
| 受控弱网 | x11vnc | lossless | drag | 10 | 3228（3092–3511） | 2.205 |
| 受控弱网 | x11vnc | wan | static | 10 | 2910（2694–3263） | 0.399 |
| 受控弱网 | x11vnc | wan | scroll | 10 | 2910（2744–3642） | 1.590 |
| 受控弱网 | x11vnc | wan | drag | 10 | 2966（2791–3382） | 1.577 |
| 受控弱网 | TigerVNC | lossless | static | 10 | 2876（2861–2944） | 0.552 |
| 受控弱网 | TigerVNC | lossless | scroll | 10 | 2879（2855–7117） | 2.757 |
| 受控弱网 | TigerVNC | lossless | drag | 10 | 2874（2844–3982） | 2.163 |
| 受控弱网 | TigerVNC | wan | static | 10 | 2625（2591–2721） | 0.483 |
| 受控弱网 | TigerVNC | wan | scroll | 10 | 2620（2588–2651） | 1.976 |
| 受控弱网 | TigerVNC | wan | drag | 10 | 2634（2591–7007） | 1.573 |
| 受控弱网 | TurboVNC | lossless | static | 10 | 2786（2747–4182） | 0.554 |
| 受控弱网 | TurboVNC | lossless | scroll | 10 | 2761（2753–2811） | 2.767 |
| 受控弱网 | TurboVNC | lossless | drag | 10 | 2772（2756–3195） | 2.692 |
| 受控弱网 | TurboVNC | wan | static | 10 | 2526（2490–3399） | 0.394 |
| 受控弱网 | TurboVNC | wan | scroll | 10 | 2523（2495–3070） | 1.970 |
| 受控弱网 | TurboVNC | wan | drag | 10 | 2560（2488–3334） | 2.341 |

这些数据证明了本组固定条件下的可连接性、完整覆盖及请求上限。字节计数覆盖握手后的已完成画面更新，包含初始画面及固定 5 秒负载窗口，不包含 TCP/TLS 开销；它不是每秒帧数。十个样本不足以提供可靠 P95。该探针不测 GPU 呈现、鼠标到画面的完整延迟或操作主观体验。运行期间存在本机构建和独立 RDP 测试负载，结果不应作为跨机器性能排名。

WAN 档没有对所有组合带来同样收益。例如 TigerVNC 的回环静态场景，WAN 档首屏和字节量均高于无损档；服务端后续更新、JPEG 解码和细化行为应同时考虑。保留默认无损与显式 WAN 选择，避免承诺自动最优。CPU 补充测量单独记录，不把原始 360 次观测中未采集的 CPU 数值补造出来。

旧的固定总计 5 秒试跑保存在 `linux-fixed-window` 两个 JSON 文件及 [说明](README.md)，包括未覆盖完整首屏的失败样本。本轮采用修正后的独立首屏截止时间和完整稳态窗口，不覆盖、删除或重新解释旧失败。

证书直连另见 `2026-10-16-vencrypt.json`：TigerVNC/TurboVNC 的有效私有 CA 与正确主机名成功，错误主机名和未知根均拒绝。Windows TightVNC/UltraVNC、服务模式、锁屏、UAC、原生输入法和 DPI 验收独立在 GitHub #205/#207/#210 跟踪；本表不证明它们完成。
