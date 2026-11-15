# #244 多屏指针坐标 —— 服务端半程：xrdp 多屏会话里的服务端自测

缺陷：[#244](https://github.com/weironz/adit/issues/244)「RDP 多屏：指针坐标的服务端半程验收」（从 [#233](https://github.com/weironz/adit/issues/233) 拆出）。
验收日期：2026-11-16 · 服务端：**xrdp 0.9.17 + xorgxrdp 0.2.17**（Ubuntu 22.04.5，本机回环 `127.0.0.1:3389`）
· 结果：**线上坐标在三种多屏布局下与服务端自己的指针坐标逐点一致（27/27）；不一致的点（等大双屏 5 个 + 原点对照 1 个）全部是 xorgxrdp 自身的已知行为，源码可查（见第三节）**

## 结论

#244 缺的是「服务端自己认为指针在哪」这一可观测量。GRD 和 Windows 此前都只从画面或指针 PDU 里找，找不到。
这次换了观测位置：**直接在服务端会话内部读它的指针**。xrdp 的会话就是一个 X server（xorgxrdp），
RDP 输入落到的就是这个 X server 的指针，`xdotool getmouselocation` 读到的是服务端本身的状态，不是客户端的推算。
多屏也是真多屏：xorgxrdp 按客户端 GCC Client Monitor Data 块建 RandR 显示器，`xrandr --listmonitors` 能直接看到。

| 布局（客户端广告） | 服务端 RandR 实测 | 线上 = 服务端 | 备注 |
| --- | --- | --- | --- |
| 1280×800 + 1280×800 | `rdp0 1280x800+0+0`、`rdp1 1280x800+1280+0`，2560×800 | **9/9**（内部 + 接缝 1279/1280/1281 + 两屏近端/远端） | 另有 5 个边缘点，见第三节 |
| 1280×800 + 1920×1080（不等大） | `rdp0 1280x800+0+0`、`rdp1 1920x1080+1280+0`，3200×1080 | **10/10**（含接缝、mon1 低于 mon0 底边的区域、mon1 内角） | |
| 1024×768 ×3 | `rdp0/1/2` 起点 0 / 1024 / 2048，3072×768 | **8/8**（两条接缝的两侧） | |

**这条验法证的是：Adit 发出去的线上坐标，服务端按同一个坐标系、同一套显示器布局接收。** 客户端半程（本地坐标 → 线上坐标）
已由 `rdp_pointer_acceptance` 在 #233 验过，两段拼起来就是完整链路。

## 验收环境

- 服务端：`xrdp 0.9.17-2ubuntu3.1`、`xorgxrdp 1:0.2.17-1build1`，`security_layer=negotiate`，本地测试账号 `aditrdp`。
  口令随机生成，存在仓库外，只经 stdin 传给 `rdp_probe`，不进 argv、环境变量、日志或本目录。
- 客户端：`rdp_probe`（`crates/adit-rdp-proto/examples/rdp_probe.rs`）驱动真实的 `adit-rdp-host` helper，
  走 helper IPC 发 `InputEvent::MouseMove`——和 GUI 用的是同一条路径。
- helper 基于 `b60659a` 构建，有两处差异（第四节）：
  1. 本 PR 的 vendor 补丁（接受 finalization 阶段的 Monitor Layout PDU）——**随 PR 提交**；
  2. 仅本地：`enable_tls: true`。xrdp 0.9.17 不支持 CredSSP，而发布版 helper 只广告 HYBRID，
     服务端只能选 `STANDARD_RDP_SECURITY`，连接在安全协商就失败。**这一处没有提交**，见「局限」。
- 服务端指针 oracle：`xrdp-oracle.sh pointer`（`sudo -u aditrdp DISPLAY=:10 xdotool getmouselocation`）。
  拓扑 oracle：`xrdp-oracle.sh xrandr`。
- 驱动：`drive.py`。每个目标发 `MOVE x y` + `ACK`，然后轮询 oracle，直到服务端位置等于线上坐标或 1.5 s 超时。
  **期望值就是发出去的线上坐标，没有按服务端行为做任何修正。**

复现（每轮前 `sudo pkill -u aditrdp` 结束上一个会话，确保服务端按本轮布局新建）：

```bash
python3 drive.py --probe target/release/examples/rdp_probe --helper <adit-rdp-host> \
    --host 127.0.0.1:3389 --user aditrdp --monitors 1280x800,1280x800 \
    --oracle "$PWD/xrdp-oracle.sh pointer" --layout-oracle "$PWD/xrdp-oracle.sh xrandr" \
    --out equal-2x1280x800.jsonl --settle-timeout 1.5 seam=1280,400 ... < 口令文件
```

## 一、等大双屏 1280×800 + 1280×800（`equal-2x1280x800.*`）

| 标签 | 线上 | 服务端 | |
| --- | --- | --- | --- |
| fresh-origin | (0,0) | (400,300) | 不一致 → 第三节 A |
| mon0-origin | (0,0) | (400,300) | 不一致 → 第三节 A |
| mon0-near | (100,400) | (100,400) | ✔ |
| mon0-far | (1100,400) | (1100,400) | ✔ |
| seam-left | (1279,400) | (1279,400) | ✔ |
| seam | (1280,400) | (1280,400) | ✔ |
| seam-right | (1281,400) | (1281,400) | ✔ |
| mon1-near | (1300,400) | (1300,400) | ✔ |
| mon1-far | (2300,400) | (2300,400) | ✔ |
| mon1-top | (2000,0) | (2000,0) | ✔ |
| mon1-inner-corner | (2558,798) | (2558,798) | ✔ |
| edge-right | (2559,400) | (2558,400) | 不一致 → 第三节 B |
| edge-bottom | (1000,799) | (1000,798) | 不一致 → 第三节 B |
| edge-corner | (2559,799) | (2558,798) | 不一致 → 第三节 B |

## 二、不等大双屏、三屏（`unequal-1280x800-1920x1080.*`、`triple-3x1024x768.*`）

全部一致，逐点见 jsonl。值得单独指出的两点：

- 不等大布局下 `mon1-below-mon0 (1500,900)` 一致：这个点在 mon0 底边（y=799）以下、只属于 mon1，
  说明服务端用的是每块屏各自的高度，而不是把桌面当成统一高度的矩形。
- 三屏的两条接缝 1023/1024、2047/2048 两侧都一致。

## 三、不一致的点：全部是 xorgxrdp 自身的行为

### A. 新会话里的第一次移到原点（`fresh-origin-control.*`）

新建会话时 X 的指针在屏幕中央 (400,300)，第一次发 (0,0)，服务端不动；随后发 (1,1) 立即到位，再发 (0,0) 也到位。

- **客户端确实发了**：helper 日志（`fresh-origin-control.helper.txt`）三次移动都有记录，
  `pointer move reached the helper x=0 y=0` / `x=1 y=1` / `x=0 y=0`。Adit 侧的「第一次移到原点也要发出去」在 #239 已修过。
- **是服务端丢的**：xorgxrdp `xrdpmouse/rdpMouse.c` 的 `PtrAddEvent` 只在
  `old_cursor_x/y != cursor_x/y` 时才把运动事件交给 X；`old_cursor_x/y` 初始为 0，而 X 自己把指针放在屏幕中央。
  于是新会话里的第一次 (0,0) 被它自己当成「没动」吞掉。复用已有会话时同一操作一致（`reused-session-origin.jsonl`：指针原在 (1280,798)，第一次发 (0,0) 即到位）。

### B. 右边缘、下边缘差 1 像素

`x = 桌面宽 − 1` 落到 `宽 − 2`，`y = 桌面高 − 1` 落到 `高 − 2`；`宽 − 2`、`高 − 2` 本身一致（`mon1-inner-corner`）。
xorgxrdp `rdpMouse.c` `rdpInputMouse` 的 `WM_MOUSEMOVE` 分支：

```c
/* without the minus 2, strange things happen when dragging
   past the width or height */
pointer->cursor_x = l_bound_by(param1, 0, dev->width - 2);
pointer->cursor_y = l_bound_by(param2, 0, dev->height - 2);
```

服务端主动把坐标钳到 `[0, 宽−2] × [0, 高−2]`。线上坐标是对的（2559 是 2560 宽桌面的最后一列）。

## 四、顺带修掉的两个缺陷

1. **Adit 连不上多屏 xrdp**（随本 PR 提交）。xrdp 收到客户端的 Monitor Data 块后，会在 connection finalization 阶段、
   Font Map 之前发一个 Monitor Layout PDU（MS-RDPBCGR 2.2.12.1）。vendored `ironrdp-connector` 在这里只认
   Synchronize / Control / ServerSetErrorInfo / Font Map，直接 `unexpected server message` 把整个连接判失败。
   Windows 和 GRD 都不发这个 PDU，所以此前没暴露。补丁见 `crates/adit-rdp/IRONRDP-PATCHES.md` 第 6 条，
   单测 `crates/adit-rdp/tests/finalization_monitor_layout.rs`。
2. **`rdp_probe --monitors` 对不等大布局算错起点**（随本 PR 提交，仅影响探针）。原来是 `left = index × width`，
   1280×800,1920×1080 会把第二块屏放在 +1920，中间留出一段 640 像素的空洞，服务端照单全收。
   改为前面各屏宽度累加。GUI 用的是系统真实的显示器几何，不经过这段代码。

## 局限

- **服务端是 xrdp，不是 Windows 或 GRD。** 证明的是 Adit 的线上坐标语义与一个按 MS-RDPBCGR 实现多屏的服务端一致；
  Windows 服务端的同类验收另行进行（会话内 `GetCursorPos`）。
- **测试用 helper 打开了 TLS**（未提交）。发布版 helper 只走 CredSSP，连不上默认配置的 xrdp 0.9.17。
  这是独立的兼容性缺口，不影响本条目的坐标结论：TLS 只改变安全层，输入 PDU 的编码路径相同。
- 只测了 100% 缩放、所有显示器 top=0 的水平排列。负坐标、纵向排列、非 100% 缩放没有覆盖。

## 文件

| 文件 | 内容 |
| --- | --- |
| `drive.py` | 驱动：发 `MOVE`，轮询服务端 oracle，逐点写 jsonl |
| `xrdp-oracle.sh` | 服务端 oracle：xrdp 会话内的 `xdotool` / `xrandr` |
| `*.txt` | 每轮的完整探针输出（含广告布局、服务端拓扑、逐点结果） |
| `*.jsonl` | 每轮逐点结果：`wire`、`server_before`、`server_after`、`match`、`settle_ms` |
| `fresh-origin-control.helper.txt` | helper 日志中的指针移动记录（第三节 A） |
| `reused-session-origin.jsonl` | 复用已有会话时第一次移到原点的对照（第三节 A） |
