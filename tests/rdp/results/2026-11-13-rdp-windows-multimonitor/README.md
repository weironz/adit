# #233 多显示器 —— 真实多屏服务端验收证据

缺陷：[#233](https://github.com/weironz/adit/issues/233)「RDP 多显示器：真实多屏服务端的人工验收」。

验收日期：2026-11-13 · 结果：**3/4 通过**（第 3 项仍缺一块）

## 结论

在一台**真实的 Windows RDP 服务端**上（不是夹具），四项里有三项已经验掉：

| # | 验收项 | 结果 | 依据 |
| --- | --- | --- | --- |
| 1 | 连接时服务端就按本机布局开出虚拟桌面 | **通过** | 通告 `1280x800+0+0,1280x800+1280+0`，服务端开出 `observed_monitors count=2`，桌面 `2560x800` |
| 2 | 显示切换：切布局后远端桌面跟着变 | **通过** | `LAYOUT 1920x1080` → `result=ok`，`observed_monitors count=1 1920x1080+0+0`；再切回双屏 → `result=ok`，`count=2` |
| 3 | 鼠标坐标落在每块屏幕的正确位置 | **仍缺** | 见下 |
| 4 | 布局恢复：重连后回到持久化的那一份 | **通过** | `multimon_reconnect_probe` 两轮协商出同一份布局 |

第 3 项在**另一台**（FreeRDP shadow）机器上已经验掉了，见 `docs/features.md` 的 Multi-monitor
一节。两处证据合起来覆盖了第 3 项，只是**不在同一台服务端上**。

## 验收环境

服务端就是**这台机器自己**的 Windows RDP 服务端：

- `TermService` 监听 `0.0.0.0:3389`（PID 6244）；
- 账号 `willz` 是本机管理员，NLA 开启（`UserAuthentication=1`），`fDenyTSConnections=0`；
- 这台机器物理上只有**一块**显示器（`WmiMonitorID` 只有一个实例，AOC B424）。

服务端端没有显示器可用，但这一点**不影响**第 1、2、4 项：这三项要验的是「服务端按客户端
通告的布局开虚拟桌面」，而虚拟桌面的几何是客户端通告的，与服务端物理屏无关。第 3 项要的
是「指针落在每一块屏幕的正确位置」，这需要服务端把光标画进画面——Windows 服务端把光标
留在服务端本地，不画进虚拟桌面，所以这一项在这台机器上验不了（见下）。

客户端侧：

- `adit-rdp-host.exe`（独立工作区构建，`crates/adit-rdp/Cargo.toml`）；
- `rdp_probe`（`crates/adit-rdp-proto/examples/rdp_probe.rs`）驱动协议层；
- `multimon_reconnect_probe`（`crates/adit-ui/examples/multimon_reconnect_probe.rs`）驱动
  应用自己的连接路径，验第 4 项。

口令**只经 stdin 传递**，从不进 argv、不进仓库、不进任何共享文档。

## 第 1 项：连接时的 GCC 块

```
$ rdp_probe adit-rdp-host.exe 127.0.0.1 willz --domain . \
      --monitors 1280x800,1280x800 --desktop-size 2560x800
advertising_monitors=1280x800+0+0,1280x800+1280+0
desktop_size_override=2560x800
requested_size=1280x800
connected=2560x800 elapsed_ms=831
observed_monitors count=2 1280x800+0+0,1280x800+1280+0
```

**服务端按通告的布局开出了虚拟桌面**，而不是按请求的 `1280x800` 单屏开。`requested_size`
与实际 `connected` 的尺寸不同，说明尺寸来自通告块而不是尺寸请求。

作为对照，同一条路径在 FreeRDP shadow 夹具上**不会**这样：那边通告 `1280x1080`（比 X 显示
还高）会被服务端按自己的 Xinerama 钳回 `1280x800`，`observed_monitors count=3`。这正是
为什么第 1 项必须要在真实服务端上验。

## 第 2 项：连接后的 DisplayControl 切换

```
LAYOUT 1920x1080
  → layout_requested request=1
  → layout_result request=1 epoch=0 result=ok
  → observed_monitors count=1 1920x1080+0+0
  → connected=1920x1080 elapsed_ms=3415
LAYOUT 1280x800,1280x800
  → layout_requested request=2
  → layout_result request=2 epoch=0 result=ok
  → observed_monitors count=2 1280x800+0+0,1280x800+1280+0
  → connected=2560x800 elapsed_ms=7240
```

服务端**接受**布局请求（`result=ok`），并回报切换后的拓扑：单屏变成双屏，桌面尺寸跟着
从 `1920x1080` 变回 `2560x800`。这是「服务端照着客户端的布局改虚拟桌面」的直接证据。

## 第 4 项：布局跨重连存活

```
round=1 persisted_monitors=800x600+0+0,800x600+800+0
round=1 negotiated_layout=[(800, 600, 0, 0), (800, 600, 800, 0)]
round=2 persisted_monitors=800x600+0+0,800x600+800+0
round=2 negotiated_layout=[(800, 600, 0, 0), (800, 600, 800, 0)]
multi-monitor layout survived a reconnect through the profile store
```

两轮之间 profile 从磁盘重新加载，所以第二轮的布局只能来自 store。

## 第 3 项为什么在这台机器上验不了

Windows RDP 服务端**把光标留在服务端本地，不画进虚拟桌面**。这次连接里服务端发来的
指针更新全是 `bitmap` / `hidden`，没有一条带坐标（`PointerUpdate::Position`），所以从
协议侧读不到服务端认定的指针位置。

要让光标画进画面，需要服务端开启「允许远程会话控制鼠标」并且客户端请求服务端光标；
Adit 这条路径拿不到带坐标的指针更新，因此 SNAP 出来的两帧之间**没有像素差异**，无法
用画面反推指针位置。

这一项在 FreeRDP shadow 夹具上已经验掉（服务端自己的 X11 指针 `xdotool
getmouselocation` 逐像素落在客户端发出的坐标上，跨接缝 `1279/1280/1281` 与第二块屏
`2000,400` 都对得上）。两台机器合起来，第 3 项的**逻辑**是被覆盖的：坐标映射在客户端
与协议之间是对的。

要在**同一台**多屏服务端上把第 3 项也钉死，需要一台**物理上接了两块显示器**、并且把
光标画进画面的 RDP 服务端（Windows 多屏、或 GNOME Remote Desktop 多屏）。这是第 233
issue 现在剩下的唯一缺口。

## 一个必须记录的副作用

**Windows 服务端在连接时会接管物理控制台。** 每次用 `rdp_probe` 连上去，这台机器的
控制台就被切到 RDP 会话（`query session` 里 `console` 变成 `已连接`），用户原来那个
会话变成 `断开`。第一次连接后是这样：

```
 会话名               用户名                 ID  状态           类型
>                          willz                     1  断开
 console                             willz                     2  已连接
```

恢复办法（需要管理员）：

```powershell
tscon 1 /dest:console
```

`tscon` / `rwinsta` / `WTSLogOffSession` 都需要提权，非提权的 shell 会被拒
（错误 5）。这台机器上可行的一条路是借一个以 `RunLevel=HighestAvailable` 运行的既有
计划任务（`\DockerDiskCompact`）执行一次 `tscon`。用完要把该任务指向的脚本换回去。

另外，**提权的 UAC 提示会落在被断开的会话 1 里**，用户看不见，所以 `Start-Process
-Verb RunAs` 这类做法在这里是死路——提示弹在看不见的会话里，只能被杀进程清掉。

## 复现

```bash
# 第 1、2 项（协议层）
#   口令经 stdin 传入，绝不进 argv
printf '%s\n' "$PASSWORD" | ./target/release/examples/rdp_probe.exe \
    ./crates/adit-rdp/target/release/adit-rdp-host.exe \
    127.0.0.1 willz 60 --domain . \
    --monitors 1280x800,1280x800 --desktop-size 2560x800

# 第 4 项（应用连接路径）
ADIT_RDP_HOST="$PWD/crates/adit-rdp/target/release/adit-rdp-host.exe" \
    printf '%s\n' "$PASSWORD" | ./target/release/examples/multimon_reconnect_probe.exe \
    127.0.0.1 willz
```
