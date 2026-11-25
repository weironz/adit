# #88：Adit Telnet 对真实 telnetd 的 `CR NUL` 验收（2026-11-25）

**结论**：三种真实 telnetd（netkit、GNU inetutils、BusyBox）上，Adit 生产 Telnet 客户端
（`adit_ssh::spawn_telnet`）发出的裸回车都在线上是 `CR NUL`，三个服务端都把它还原成单个
`CR` 交给 pty，**没有任何服务端把 NUL 漏进会话**；`0xFF` 加倍后服务端还原为单个 `0xFF`；
真实 `/bin/login` 登录并执行命令在三个服务端都成功。

这**不是**真实网络设备（Cisco IOS、IPMI SOL、串口服务器）的验收，#88 不因此关闭。

## 测试对象

| 名称 | 实现 | 版本 | 启动方式 |
|---|---|---|---|
| netkit | Ubuntu `telnetd`（netkit in.telnetd） | 0.17-44build1 | 登录：openbsd-inetd 0.20160825-5 + tcpd，端口 23；观测：`systemd-socket-activate --inetd -a` 端口 2301，`-L observer.py` |
| inetutils | GNU inetutils telnetd | 2.2（Ubuntu `inetutils-telnetd` 2:2.2-2ubuntu0.2，因与 netkit 包冲突，`dpkg-deb -x` 解包运行） | `systemd-socket-activate --inetd -a`；观测 2302（`-E observer.py`），登录 2312 |
| BusyBox | BusyBox telnetd | 1.35.0 官方 musl 静态二进制（sha256 `6e123e7f…bb311348`，Ubuntu 的 busybox 包不含 telnetd） | `busybox telnetd -F`；观测 2303（`-l observer.py`），登录 2313（`-l /bin/login`） |

环境：Ubuntu 22.04.5，全部在本机 127.0.0.1。Docker Hub 当时返回 429，因此没有用容器。

## 方法

- 客户端：`crates/adit-ssh/examples/telnet_probe.rs`，直接调用生产的 `spawn_telnet`，按 stdin
  脚本发送按键（hex 编码，保证 CR/NUL/0xFF 原样），把每个 `Output` 事件按 hex 记录。
  终端尺寸 100×30（NAWS 中可见 `0064 001e`）。
- 线上抓取：`proxy.py` 单连接 TCP 转发，逐块记录双向字节 → `wire-*.jsonl`。
- 服务端观测（oracle）：`observer.py` 被 telnetd 当作登录程序启动，**在服务端 pty 内**读字节并回报 hex：
  - `raw-*`：`tty.setraw` 后读取，看到的就是 telnetd 写进 pty 的原始字节；
  - `cooked`：恢复规范模式，读一行，验证回车能完成一行输入。
- 登录：测试账号 `telnettest`，密码随机生成，存于仓库外，经 stdin 传给探针（`SEND -`），
  线上记录中替换为 `<redacted-password>`；`drive.py` 结束前断言所有证据文件都不含密码。
  登录后执行 `printf 'ok-%s\n' $((6*7))`，以输出 `ok-42` 判定成功。

复现：`cargo build -p adit-ssh --example telnet_probe`，按上表启动服务端后
`python3 drive.py target/debug/examples/telnet_probe <密码文件>`。

## 结果（`summary.json`）

| 用例 | Adit 按键 | 线上（c2s） | netkit pty | inetutils pty | BusyBox pty |
|---|---|---|---|---|---|
| raw-cr | `61 0d 62` | `61 0d 00 62` | `61 0d 62` ✅ | `61 0d 62` ✅ | `61 0d 62` ✅ |
| raw-iac | `78 ff 79` | `78 ff ff 79` | `78 ff 79` ✅ | `78 ff 79` ✅ | `78 ff 79` ✅ |
| cooked | `hello 0d` | `hello 0d 00` | `hello 0a` ✅ | `hello 0a` ✅ | `hello 0a` ✅ |
| raw-crlf（仅记录） | `63 0d 0a 64` | `63 0d 0a 64` | `63 0d 64` | `63 0d 64` | `63 0d 64` |
| 登录 + 命令 | `telnettest 0d`… | `telnettest 0d 00` | `ok-42` ✅ | `ok-42` ✅ | `ok-42` ✅ |

每个观测会话 9/9 个 WAIT 命中、0 超时；每个登录会话 4/4 命中、0 超时。

## 观察与限制

- `raw-crlf`：Adit 对已有的 `CR LF` 不追加 NUL（符合 RFC 854），三个服务端都把 NVT 的
  `CR LF` 当作一次换行交给 pty（只剩 `CR`）。这是服务端定义的行为，只记录不判定；它意味着
  粘贴含 `CRLF` 的文本时远端只看到 `CR`，与传统 telnet 客户端一致。
- 服务端→客户端方向，三个服务端都会把裸 `CR` 发成 `CR NUL`（如 `0d 00 0d 0a`）。Adit 把 NUL
  原样交给终端，VT 解析器忽略 C0 NUL，画面无影响；本次未改动。
- 均为非 BINARY 模式（三个服务端都没有协商 BINARY）。
- 未覆盖：Cisco IOS / IPMI SOL / 串口服务器等真实设备、GUI 内交互输入（本次只用了生产客户端
  的同一 `spawn_telnet` 路径，没有经过 iced 按键映射）。

## 文件

- `drive.py` / `proxy.py` / `observer.py`：驱动、线上抓取、服务端 pty 观测。
- `probe-*.txt`：探针逐事件输出；`wire-*.jsonl`：双向线上字节；`summary.json`：汇总。
