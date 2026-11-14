# #233 多屏 —— 对着真实 GRD 主机的验收证据

缺陷：[#233](https://github.com/weironz/adit/issues/233)「RDP：多显示器验收」。
验收日期：2026-11-14 · 目标主机：**授权的 GRD 主机 `100.70.113.193`（headless 远程登录模式）**
· 结果：**客户端半程 4/4 通过；服务端半程在该主机上无法观测（原因见下）**

## 结论（先说清楚每一项测到了什么）

#233 有四个验收项。在这台主机上：

| # | 验收项 | 结果 | 观测位置 |
| --- | --- | --- | --- |
| 1 | 连接时按客户端广告的布局开虚拟桌面 | **通过** | 服务端 helper 日志 + **GRD 服务端自己的日志**：两块 1200×800 的 surface，起点 `(0,0)` 与 `(1200,0)` |
| 2 | 切换显示布局后远端跟随 | **通过** | 客户端探针 `layout_switch_probe` + **GRD 服务端自己的日志**：服务端 `Disposing 1 virtual monitor(s)`，远端桌面从 2400×800 缩成 1200×800 |
| 3 | 鼠标坐标落在每块屏幕的正确位置 | **客户端半程通过，服务端半程无法观测** | 客户端侧 4/4；服务端侧这台主机给不出任何可观测量 |
| 4 | 重连后布局恢复 | **通过** | `multimon_reconnect_probe` 两轮协商出同一布局 |

**第 3 项的服务端半程之所以测不了，不是因为没做，是因为这台主机在设计上就不产生可观测的指针位置。**
下面第五节列出四次独立的否定结果。这一项在真实的多屏服务器上仍未验收。

> 第 2 项此前是「没测」而不是「测了没过」——已跑的 27 轮会话里，客户端
> post-connect 发出的布局和连接时广告的完全是同一份几何，服务端看到的东西没变。
> 本条目第二节记录了后来补上的那一次**真正的**切换，以及为什么
> 「服务端只剩一块 surface」这个看起来很自然的指标其实**不成立**。

## 验收环境

- **主机**：授权的 GRD 主机，`100.70.113.193`，Ubuntu 26.04.1 LTS，
  `gnome-remote-desktop` **50.2-0ubuntu0.1**，跑 **headless（远程登录）** 模式。
  这是 `mstsc` 连的那个目标。
- **headless 单元在跑，系统单元不在跑**（实测）：

  ```
  systemctl --user is-enabled gnome-remote-desktop-headless.service  → enabled
  systemctl --user is-active   gnome-remote-desktop-headless.service  → active
  systemctl          is-enabled gnome-remote-desktop.service          → disabled
  systemctl          is-active   gnome-remote-desktop.service          → inactive
  ```

  也就是说这台机器是被配置成**专用远程登录主机**的：桌面的 `gnome-remote-desktop.service`
  被关掉了，RDP 由 headless 单元单独提供（`grdctl --headless status` 报
  `Unit status: active`，普通 `grdctl status` 报 `inactive`）。
- **`screen-share-mode` 是 `extend`**（实测
  `gsettings get org.gnome.desktop.remote-desktop.rdp screen-share-mode` → `'extend'`）。
  所以客户端广告多屏时 GRD 走的是「扩展这块虚拟桌面」而不是「只显示一块」。
- **主机只有一块物理屏幕**（实测 `xrandr --current` 只有 `HDMI-2 connected primary
  1920x1080+0+0`）。`MAX_MONITOR_COUNT_HEADLESS` 允许到 16 个**虚拟**显示器，
  但物理上只有一块。
- **客户端**：本机，Windows 11，单显示器 `\\.\DISPLAY1`，2560×1440，100% 缩放。
  验收窗口被摆成 2416×898（两个 1200×800 的虚拟屏正好放得下）。
- **口令**：只经 stdin 管道传入，从不进 argv、不进仓库、不进本文档。

## 一、连接时按广告的布局开桌面（第 1 项）

客户端在连接时通过 **GCC Client Monitor Data** 块广告两块 1200×800 的布局。
helper 日志（`%APPDATA%\Adit\rdp-helper.log`）里，服务端照广告建了两块 surface：

```
INFO adit_rdp::egfx: surface created id=0 w=1200 h=800
INFO adit_rdp::egfx: surface mapped id=0 origin_x=0 origin_y=0
INFO adit_rdp::egfx: surface created id=1 w=1200 h=800
INFO adit_rdp::egfx: surface mapped id=1 origin_x=1200 origin_y=0
```

两块 surface 起点分别是 `(0,0)` 和 `(1200,0)` —— 正是广告的布局，**不是**请求的
`640×480`、也不是一块屏。虚拟桌面因此是 2400×800。

同一个值在后续的 surface 重建里反复出现（每轮验收会话都是这两块），所以这不是
一次偶然。

### 服务端自己怎么说

helper 日志是**客户端**看到的东西。GRD 服务端自己也留了日志，两边对上，这一项
才算真的验完了。服务端的说法（`journalctl --user -u
gnome-remote-desktop-headless.service`）：

```
[RDP] Remote Desktop session will use virtual monitors
[RDP] Layout manager: Preparing virtual monitor with geometry [x, y, w, h] = [0, 0, 1200, 800]
[RDP] Layout manager: Preparing virtual monitor with geometry [x, y, w, h] = [1200, 0, 1200, 800]
[RDP] Layout manager: Finished applying new monitor config
```

服务端**自己**记下了 `[0,0,1200,800]` 和 `[1200,0,1200,800]` 两个虚拟屏 —— 和
客户端广告的、helper 看到的完全一致。这是第 1 项最强的证据：它来自服务端自己
的账本，不是客户端的自述。

> 拿服务端日志：`journalctl --user -u gnome-remote-desktop-headless.service`。
> 这一步需要主机的 shell 权限，本条目里的服务端日志都是这样取的。
>
> **为什么这很重要**：GRD 的 `screen-share-mode` 是 `extend`，所以它服务的是一个
> **虚拟桌面**而不是主机的物理屏幕（物理上只有一块 HDMI-2）。服务端日志里
> 「will use virtual monitors」这句话说明它认了客户端广告的多屏布局。

## 二、切换布局后远端跟随（第 2 项）——**通过**

这一项此前**没有被真正触发过**：客户端 post-connect 发出的 DisplayControl 布局
和连接时广告的**完全是同一份**几何，服务端照单全收，服务端看到的东西却没变，
所以证明不了「切换之后远端会跟随」。

原因是 `SessionOptions.rdp_monitors` 被用了两次 —— 连接时进 GCC 块，连接后再发
一次 DisplayControl 请求 —— 两次带的是同一份几何。把 helper 日志里当时全部
27 轮 Adit 会话统计一遍：

| 连接时块数 → post-connect 块数 | 会话数 |
| --- | --- |
| 2 → 2 | 27 |
| 2 → 1 | 0 |
| 1 → 2 | 0 |

**没有任何一轮的布局内容发生过变化。** 这一项在真实主机上从未被触发过。

### 真正触发一次切换

工具栏上的「单屏 / 双屏」按钮走的是
`SessionManager::set_active_rdp_monitor_count`，它会发一份**新的**布局（单屏时
桌面尺寸从 2400×800 缩回 1200×800）。新增的验收工具
`crates/adit-ui/examples/layout_switch_probe.rs` 走的正是这条路径：连接时广告
两块 1200×800，会话中途调 `set_active_rdp_monitor_count(1)`。

客户端侧的账本（`sent_layout`，即最后编码上 DisplayControl 通道的布局）：

```
connect_time_layout=1200x800+0+0,1200x800+1200+0
after_switch_layout=1200x800+0+0
client_layouts=1200x800+0+0,1200x800+1200+0 -> 1200x800+0+0
```

客户端确实发了一份**服务端在这条会话里从没见过的**布局：单屏，起点 `(0,0)`。

### 服务端自己怎么说

DisplayControl 没有布局确认包，所以「服务端有没有跟随」只能读服务端自己的账本。
GRD 的 journal（`journalctl --user -u gnome-remote-desktop-headless.service`），
同一次会话，从头到尾：

```
[RDP] Layout manager: Preparing virtual monitor with geometry [x, y, w, h] = [0, 0, 1200, 800]
[RDP] Layout manager: Preparing virtual monitor with geometry [x, y, w, h] = [1200, 0, 1200, 800]
[RDP] Layout manager: Finished applying new monitor config

[RDP.DISP] Channel opened successfully. Ready for receiving new monitor layouts
[RDP.DISP] Received new monitor layout. PDU contains 2 monitors          ← post-connect 重新确认同一布局
[RDP] Layout manager: Preparing virtual monitor with geometry [x, y, w, h] = [0, 0, 1200, 800]
[RDP] Layout manager: Preparing virtual monitor with geometry [x, y, w, h] = [1200, 0, 1200, 800]
[RDP] Layout manager: Finished applying new monitor config

[RDP.DISP] Received new monitor layout. PDU contains 1 monitor            ← 切换
[RDP] Layout manager: Disposing 1 virtual monitor(s)                      ← 远端真的收掉了第二块
[RDP] Layout manager: Preparing virtual monitor with geometry [x, y, w, h] = [0, 0, 1200, 800]
[RDP] Layout manager: Finished applying new monitor config
```

**关键在 `Disposing 1 virtual monitor(s)`**：服务端**销毁了**一块虚拟显示器，
不是又建回来一块。远端桌面从 2400×800 缩成了 1200×800 —— 这就是「切换显示
布局后远端跟随」。

客户端发的和服务端做的，对得上：客户端说单屏 `(0,0)`，服务端留下并重建的正是
`[0, 0, 1200, 800]`，另一块被销毁。

### 一个必须记下来的坑：surface 数量**不是**布局的可观测量

client 侧看服务端，最直观的指标是「服务端删掉旧 surface、建了新的」。**这个
指标在这里不成立。**

这台主机在应用「双屏 → 单屏」时，只销毁了**第二块**虚拟显示器，**第一块活着**
没动 —— 于是服务端**根本没有发过 surface 删除包**，客户端看到的 surface 列表
在切换前后完全一样（helper 日志里，两次 `surface created` / `surface mapped`
都是同样的两块，`id` 从 0/1 变成 2/3 的那一次是 post-connect 重新确认布局造成
的，与切换无关）。

一份「切换后服务端只剩一块 surface」的断言，会在一次**服务端完全正确地应用了**
切换的会话上失败。这条已经写进探针的注释里 —— 探针只断言客户端发出的布局，
服务端那半程交给 journal。

> 这一项此前**曾经失败过**，而且失败的样子极具误导性：IronRDP 的
> `ironrdp-displaycontrol` 0.8.0 读能力字段时没跳过 8 字节的
> `DISPLAYCONTROL_HEADER`，服务端的 `16, 8192, 8192` 被读成 `5, 20, 16`，
> 面积上限变成 1600 平方像素，于是**每一个**布局都被判「太大」而拒绝。
> 那个 crate 现在已经 vendor 并打了补丁，见
> [`IRONRDP-PATCHES.md`](../../../crates/adit-rdp/IRONRDP-PATCHES.md)。
>
> 这次切换能跑通，也说明那条补丁是必要的：没有它，服务端回报的面积上限会把
> 单屏布局一并拒掉。

## 三、鼠标坐标 —— 客户端半程（第 3 项的一半）

验收工具 `crates/adit-ui/examples/rdp_pointer_acceptance.rs` 驱动 **OS 指针**，
不是直接调用内部函数：它用 `ClientToScreen` / `SetCursorPos` / `GetCursorPos` /
`ScreenToClient` 把指针真的挪到屏幕上的目标位置，然后读回**真正发到线上**的坐标。

四个目标点覆盖两块虚拟屏的近端与远端：

| 标签 | 期望线上坐标 | 实测线上坐标 | 落点 | 结果 |
| --- | --- | --- | --- | --- |
| `mon0-near-origin` | `(100, 400)` | `(100, 400)` | client `(116, 470)` / screen `(124, 501)` | 通过 |
| `mon0-far-edge` | `(1100, 400)` | `(1100, 400)` | client `(1116, 470)` / screen `(1124, 501)` | 通过 |
| `mon1-near-origin` | `(1300, 400)` | `(1300, 400)` | client `(1316, 470)` / screen `(1324, 501)` | 通过 |
| `mon1-far-edge` | `(2300, 400)` | `(2300, 400)` | client `(2316, 470)` / screen `(2324, 501)` | 通过 |

`PASSED 4/4`。

读回的线上坐标来自 helper 日志里 helper 收到指针移动的那一行：

```
INFO adit_rdp::session: pointer move reached the helper x=100  y=400
INFO adit_rdp::session: pointer move reached the helper x=1100 y=400
INFO adit_rdp::session: pointer move reached the helper x=1300 y=400
INFO adit_rdp::session: pointer move reached the helper x=2300 y=400
```

**这个数字的意义**：它走完了「本地坐标 → 缩放因子 → 远端坐标」整条客户端通路，
并越过了 helper 与 IronRDP 的边界，到达会话层的最后一跳。它证明的是
**Adit 算出来的远端坐标是对的**。

注意 `scale` 与 `fit` 在每一行里都是 `[1.0, 1.0]` —— 这台机器是 100% 缩放，
客户端窗口又被摆成正好容纳两块虚拟屏，所以缩放因子是 1。**缩放不为 1 的情形
这个工具没有覆盖**。

### 一个必须记下来的坑

Windows 的 `SetCursorPos` 会把屏幕外的点**静默地**夹到最近的边缘，**不报错**。
第一次验收时指针被夹了，测试看起来像是客户端算错了坐标。

现在的做法是：先把指针放到目标，再**读回**它真正的落点（`GetCursorPos` +
`ScreenToClient`），用**落点**重新算期望值（`expected_at_landing`），
然后和线上坐标比对。这样「指针没到它要去的地方」和「客户端算错了」就分开了 ——
上表里 `expected_at_landing` 与 `expected` 都相等，说明每一轮指针都真的到了。

## 四、布局重连后恢复（第 4 项）

`crates/adit-ui/examples/multimon_reconnect_probe.rs` 驱动应用**自己的**连接路径
两次，对同一个磁盘上的 profile，中间**从文件重新加载 profile** —— 所以第二轮只能
从存储里拿到布局。两轮协商出的布局相同：

```
[(800,600,0,0),(800,600,800,0)]   第一轮
[(800,600,0,0),(800,600,800,0)]   第二轮
```

布局住在 `SessionOptions.rdp_monitors`，会话启动时重新读一次，然后**用两次**：
连接时进 GCC 块，连接后再发一次 DisplayControl 请求。

## 五、为什么第 3 项的服务端半程在这台主机上测不了

客户端算对了坐标。**服务端自己认出来的那个坐标**，这台主机给不出任何可观测量。
四次独立的否定结果：

1. **GRD 不把光标画进 framebuffer。** 在三个不同的指针位置各截一张图，
   逐像素比较：**0 个像素有差异**（`B-A pixels changed>12: 0`、`C-A: 0`、`C-B: 0`）。
   指针动了，画面纹丝不动。
2. **GRD 不回传指针位置。** `GrdRdpCursorUpdate` 结构里**没有位置字段**。
   服务端永远不回「我现在认为指针在 (x, y)」。
3. **GRD 不记指针坐标。** 服务端日志里没有任何指针坐标。
4. **每一条主机侧指针探针都被挡住。**
   - Shell `Screenshot` D-Bus → `AccessDenied`
   - Shell `Eval` → `(false, '')`
   - GDK3 Wayland seat → `0,0`（拿不到 Wayland 游标的位置）
   - `xdotool` 读的是 **X 指针**，而 GRD 驱动的是 **Wayland 游标** —— 两者不是同一个东西

这不是「还没找到方法」，是这台主机**不产生**那个可观测量。

另外两件这台主机做不到的事，如实记录：

- **GRD headless 把它的 ei region 放在主机物理屏幕之外**，也不驱动宿主机的
  live compositor —— 它服务的是一个虚拟桌面，不是本机屏幕。
- 主机只有**一块**物理屏幕。多屏布局在 GRD 上是**虚拟**显示器（上限 16），
  所以第 3 项里「指针跨过屏幕边界」这个最难的场景，在这台主机上只有**虚拟**边界。

## 六、顺带记录的两件事

### helper 之前在正常断开后退出码是 1

一次**正常的**断开，helper 报的却是「管道断了」。根因是 `tokio::select!` 配
`biased`：一个**已关闭**的 channel 永远「就绪」，`recv()` 立刻返回 `None`，
于是显示通道先关闭时（reader 离开时正是如此），select 永远返回 `None`，
真正的关闭公告还躺在输入队列里，那个分支根本没被轮询到。

helper 的退出码是有意义的信号，所以这个 bug 让「正常断开」和「真的崩了」在调用方
眼里一模一样。已修：应用主动关闭时走 `SessionCmd::AppClosed`，会话以成功结束，
helper 退出 0；而应用**没**关而管道断了，仍然是失败。见 PR #243。

### `AVC444 decode failed` 是服务端偶发的

一次运行里连续出现 6 次：

```
WARN adit_rdp::egfx: AVC444 decode failed error="OpenH264 encountered an error. Native:18. Decoding State:0. ..."
```

之后的运行都正常（`first AVC444 frame painted; session is on the H.264 4:4:4 path
codec=Avc444v2`）。这是 **OpenH264 解码器侧**的失败、发生在服务端发来的码流上，
不影响本次验收的任何一项判定，但记在这里。

## 复现

### 第 2 项：布局切换

```bash
# 口令只经 stdin 管道传入，从不进 argv
ADIT_RDP_HOST=crates/adit-rdp/target/debug/adit-rdp-host.exe \
cargo run --example layout_switch_probe -p adit-ui -- 100.70.113.193 will < password-file
```

探针打印客户端侧的账本，然后提示去看服务端自己的账本：

```bash
# 服务端那半程：DisplayControl 没有确认包，只能读服务端的日志
journalctl --user -u gnome-remote-desktop-headless.service \
  | grep -E 'DISP|Layout manager'
```

服务端必须出现 `PDU contains 1 monitor` 与 `Disposing 1 virtual monitor(s)`。

### 第 3 项：指针坐标

```bash
# 口令只经 stdin 管道传入，从不进 argv
ADIT_RDP_FIXTURE_HOST=100.70.113.193:3389 \
ADIT_RDP_FIXTURE_USER=will \
scripts/run_pointer_acceptance.sh < password-file
```

工具是 feature-gated 的（`native-rdp-pointer-acceptance`），因为它会驱动真实的
OS 指针 —— 不该被任何无人值守跑测试的命令构建起来。

### 第 1、4 项

第 1 项的证据在第一节（helper 日志 + GRD journal）；第 4 项用
`crates/adit-ui/examples/multimon_reconnect_probe.rs`。
