# #244 多屏指针坐标 —— 服务端半程：Windows RDP 会话里的服务端自测

缺陷：[#244](https://github.com/weironz/adit/issues/244)「RDP 多屏：指针坐标的服务端半程验收」（从 [#233](https://github.com/weironz/adit/issues/233) 拆出）。
验收日期：2026-11-16 · 服务端：**Windows Server 2022 Standard 自带的 TermService**（本机回环 `127.0.0.2:3389`，NLA 开启）
· 结果：**四种布局、38 个点，线上坐标与服务端会话内 `GetCursorPos` / `GetPhysicalCursorPos` 逐点一致（38/38），无一例外**

## 结论

#244 缺的是「服务端自己认为指针在哪」这一可观测量。此前 Windows 那一轮（`2026-11-13-rdp-windows-multimonitor`）
只从画面和指针 PDU 里找，Windows 不回传指针位置，所以找不到。这次换了观测位置：**在 RDP 用户自己的会话里跑一个
小记录器，直接调 `GetCursorPos`**。RDP 输入注入的就是这个会话的指针，读到的是服务端本身的状态，不是客户端的推算。

多屏不需要物理双头：Windows 按客户端 GCC Client Monitor Data 块给会话建**虚拟显示器**。会话内
`EnumDisplayMonitors` 直接列出 `\.\DISPLAY1`、`\.\DISPLAY2`，矩形与客户端广告的完全相同。

| 布局（客户端广告） | 服务端会话内实测（`EnumDisplayMonitors` / `SM_*VIRTUALSCREEN`） | DPI | 线上 = 服务端 |
| --- | --- | --- | --- |
| 1280×800 + 1280×800 | `[0,0,1280,800]` 主、`[1280,0,2560,800]`，虚拟屏 `0,0,2560,800` | 96 / 96 | **11/11** |
| 1280×800 + 1920×1080（不等大） | `[0,0,1280,800]` 主、`[1280,0,3200,1080]`，虚拟屏 `0,0,3200,1080` | 96 / 96 | **10/10** |
| 1280×800 + 1280×800，连接后 `LAYOUT …@150` | 同第一行 | **144 / 144** | **9/9** |
| 1280×800 + 1920×1080，连接后 `LAYOUT 1280x800@100,1920x1080@150` | 同第二行 | **96 / 144**（混合 DPI） | **8/8** |

**这条验法证的是：Adit 发出去的线上坐标，Windows 服务端按同一个坐标系、同一套显示器布局接收，
包括接缝两侧、两屏的四角、不等大屏的下沿，以及服务端缩放 150% 与混合 DPI。**
客户端半程（本地坐标 → 线上坐标）已由 `rdp_pointer_acceptance` 在 #233 验过，两段拼起来就是完整链路
（但 GUI 一体化那一跑本机没能做，见「局限」）。

和 xrdp 那一轮（`2026-11-16-rdp-xrdp-multimonitor-pointer`）不同，Windows 这边**没有任何边缘不一致**：
原点 `(0,0)`、`(0,799)`、`(2000,0)`、右下角都一字不差。

## 验收环境

- 服务端：Windows Server 2022 Standard，`TermService` 运行中，`fDenyTSConnections=0`，端口 3389，
  `UserAuthentication=1`（NLA/CredSSP）。本地测试账号 `aditptr`（`Remote Desktop Users` 组）。
  口令随机生成，存在仓库外的 ACL 文件里，只经 stdin 传给 `drive.py` → `rdp_probe`，不进 argv、环境变量、日志或本目录
  （提交前对本目录做过一次口令全文 grep，无命中）。
- 连接走 `127.0.0.2` 而不是 `127.0.0.1`/机器名，控制台会话（Administrator，session 1）全程保持 Active，没有被接管。
- 客户端：`rdp_probe`（`crates/adit-rdp-proto/examples/rdp_probe.rs`）驱动真实的 `adit-rdp-host` helper，
  走 helper IPC 发 `InputEvent::MouseMove`——和 GUI 用的是同一条路径。
- helper 基于 `b99ae13`（PR #247）构建，**加上本 PR 恢复的 `ironrdp-pdu` vendor 补丁**（第四节）。
  没有这一行，helper 连本机 Windows 在约 350 ms 处必然断开，根本走不到发指针。
- 工具链：rustc 1.98.0（`rust-toolchain.toml` 固定版本），MSVC 14.44。
- 服务端指针 oracle：
  - `CursorLogger.cs` —— 在 `aditptr` 的会话里运行（登录触发的计划任务 `AditPointerOracle`，以该用户、交互式启动）。
    每 10 ms 采样一次，写原子替换的 `latest.json`，有变化时追加一行到 `samples.jsonl`。每条记录：
    - `cursor`：进程设为 Per-Monitor-Aware-V2 后的 `GetCursorPos`（**比对用的就是这个字段**）；
    - `physical`：`GetPhysicalCursorPos`；
    - `unaware`：线程临时切到 DPI-unaware 后的 `GetCursorPos`（老程序看到的逻辑坐标，只作对照）；
    - `virtual`：`SM_XVIRTUALSCREEN` / `SM_YVIRTUALSCREEN` / `SM_CXVIRTUALSCREEN` / `SM_CYVIRTUALSCREEN`；
    - `monitors`：`EnumDisplayMonitors` + `GetMonitorInfo` 的矩形、主屏标记，和 `GetDpiForMonitor` 的 DPI；
    - `session` / `user`：记录器所在会话号，用来证明读的是 RDP 会话（session 4–9），不是控制台（session 1）。
  - `oracle.py pointer` 读 `latest.json`，超过 1 s 未更新就拒绝（防止读到已断开会话里的旧值），打印 `X Y`；
    `oracle.py topology` 打印会话号、虚拟屏和显示器列表。
- 驱动：`drive.py`（PR #247 那个通用驱动，本 PR 给它加了 `--relayout`）。每个目标发 `MOVE x y` + `ACK`，
  然后轮询 oracle，直到服务端位置等于线上坐标或 3 s 超时。**期望值就是发出去的线上坐标，没有做任何修正。**
- `run.sh`：每轮先 `logoff` 掉 `aditptr` 遗留的会话，确保 Windows 按本轮布局新建会话，再调用 `drive.py`。

复现（Git Bash；记录器的编译与计划任务见 `CursorLogger.cs` 文件头）：

```bash
"$BASH" run.sh equal-2x1280x800 1280x800,1280x800 \
    mon0-origin=0,0 mon0-near=100,400 ... far-corner=2559,799 < 口令文件
"$BASH" run.sh scaled150-2x1280x800 1280x800,1280x800 \
    --relayout 1280x800@150,1280x800@150 mon0-origin=0,0 ... < 口令文件
```

## 一、等大双屏 1280×800 + 1280×800（`equal-2x1280x800.*`，session 4）

服务端拓扑：虚拟屏 `[0,0,2560,800]`，`DISPLAY1 [0,0,1280,800]` 主屏，`DISPLAY2 [1280,0,2560,800]`，DPI 均 96。

| 标签 | 线上 | 服务端 `GetCursorPos` | |
| --- | --- | --- | --- |
| mon0-origin | (0,0) | (0,0) | ✔ |
| mon0-near | (100,400) | (100,400) | ✔ |
| mon0-far | (1100,400) | (1100,400) | ✔ |
| seam-left | (1279,400) | (1279,400) | ✔ |
| seam | (1280,400) | (1280,400) | ✔ |
| seam-right | (1281,400) | (1281,400) | ✔ |
| mon1-near | (1300,400) | (1300,400) | ✔ |
| mon1-far | (2300,400) | (2300,400) | ✔ |
| mon1-top | (2000,0) | (2000,0) | ✔ |
| mon0-bottom-left | (0,799) | (0,799) | ✔ |
| far-corner | (2559,799) | (2559,799) | ✔ |

`physical`、`unaware` 在 96 DPI 下与 `cursor` 完全相同（见 `equal-2x1280x800.logger.jsonl`）。
每点从 `ACK` 到 oracle 读到新位置 35–48 ms。

## 二、不等大双屏 1280×800 + 1920×1080（`unequal-1280x800-1920x1080.*`，session 5）

服务端拓扑：虚拟屏 `[0,0,3200,1080]`，`DISPLAY1 [0,0,1280,800]` 主屏，`DISPLAY2 [1280,0,3200,1080]`。
左屏的 `left` 是累加宽度（PR #247 修过的 `rdp_probe --monitors`），服务端看到的 `DISPLAY2` 起点正是 1280。

| 标签 | 线上 | 服务端 `GetCursorPos` | |
| --- | --- | --- | --- |
| mon0-origin | (0,0) | (0,0) | ✔ |
| mon0-mid | (640,400) | (640,400) | ✔ |
| mon0-bottom-right | (1279,799) | (1279,799) | ✔ |
| seam-left | (1279,400) | (1279,400) | ✔ |
| seam | (1280,400) | (1280,400) | ✔ |
| seam-right | (1281,400) | (1281,400) | ✔ |
| mon1-top-left | (1280,0) | (1280,0) | ✔ |
| mon1-center | (2240,540) | (2240,540) | ✔ |
| mon1-bottom-left | (1280,1079) | (1280,1079) | ✔（低于左屏底边的区域） |
| far-corner | (3199,1079) | (3199,1079) | ✔ |

## 三、服务端缩放 ≠ 100%

### A. 两屏都 150%（`scaled150-2x1280x800.*`，session 8）

连接后发 `LAYOUT 1280x800@150,1280x800@150`（Display Control 通道，`desktopScaleFactor=150`）。
服务端 `layout_result=ok`，会话内两屏 DPI 从 96 变为 **144**，矩形不变。

| 标签 | 线上 | `GetCursorPos`（PMv2） | `GetPhysicalCursorPos` | DPI-unaware `GetCursorPos`（对照） | |
| --- | --- | --- | --- | --- | --- |
| mon0-origin | (0,0) | (0,0) | (0,0) | (0,0) | ✔ |
| mon0-near | (100,400) | (100,400) | (100,400) | (67,267) | ✔ |
| mon0-far | (1100,400) | (1100,400) | (1100,400) | (733,267) | ✔ |
| seam-left | (1279,400) | (1279,400) | (1279,400) | (853,267) | ✔ |
| seam | (1280,400) | (1280,400) | (1280,400) | (853,267) | ✔ |
| seam-right | (1281,400) | (1281,400) | (1281,400) | (854,267) | ✔ |
| mon1-near | (1300,400) | (1300,400) | (1300,400) | (866,267) | ✔ |
| mon1-far | (2300,400) | (2300,400) | (2300,400) | (1533,267) | ✔ |
| far-corner | (2559,799) | (2559,799) | (2559,799) | (1706,533) | ✔ |

### B. 混合 DPI：左屏 100%、右屏 150%（`mixed-dpi-1280x800-1920x1080.*`，session 9）

连接后发 `LAYOUT 1280x800@100,1920x1080@150`。服务端 `DISPLAY1` DPI 96，`DISPLAY2` DPI 144。

| 标签 | 线上 | `GetCursorPos`（PMv2） | `GetPhysicalCursorPos` | DPI-unaware（对照） | |
| --- | --- | --- | --- | --- | --- |
| mon0-origin | (0,0) | (0,0) | (0,0) | (0,0) | ✔ |
| mon0-mid | (640,400) | (640,400) | (640,400) | (640,400) | ✔ |
| seam-left | (1279,400) | (1279,400) | (1279,400) | (1279,400) | ✔ |
| seam | (1280,400) | (1280,400) | (1280,400) | (1280,267) | ✔ |
| seam-right | (1281,400) | (1281,400) | (1281,400) | (1281,267) | ✔ |
| mon1-center | (2240,540) | (2240,540) | (2240,540) | (1920,360) | ✔ |
| mon1-bottom-left | (1280,1079) | (1280,1079) | (1280,1079) | (1280,719) | ✔ |
| far-corner | (3199,1079) | (3199,1079) | (3199,1079) | (2559,719) | ✔ |

读法：**线上坐标是虚拟桌面的物理像素，服务端缩放不改变它。** DPI 感知程序（以及 `GetPhysicalCursorPos`）看到的
就是线上坐标；只有 DPI-unaware 程序看到的是缩放后的逻辑坐标，而且是**按显示器各自换算**——
混合 DPI 下 `(2240,540)` → `1280 + (2240−1280)×⅔ = 1920`、`540×⅔ = 360`，左屏不变。这是 Windows 对老程序的虚拟化，
和 Adit 发什么无关，所以比对用 PMv2 的 `cursor` 字段，unaware 只作对照列出。

### C. 不奏效的做法：给用户设 LogPixels=144（`registry-logpixels144-2x1280x800.logger.jsonl`，session 6）

先试过不改代码的办法：离线加载 `aditptr` 的 `NTUSER.DAT`，设 `Control Panel\Desktop\LogPixels=144`、
`Win8DpiScaling=1`，再新建会话。**会话内 DPI 仍是 96**——RDP 会话的缩放取自客户端，不看用户自己的显示设置。
那一轮 9 个点同样 9/9 一致，但因为 DPI 没变，它不算缩放证据；它的 `drive.py` 输出被随后同名的 A 轮覆盖，
只保留了记录器样本。注册表值事后已删除。

## 四、顺带发现并修复：`ironrdp-pdu` vendor 补丁在 main 上失效

第一次连本机 Windows，helper 在约 350 ms 处必然断开（单屏、双屏都一样）：

```
ClearCodec decode failed ... invalid rlex: suite exceeds region pixel count
```

原因：`01843d6`（#236）改 `crates/adit-rdp/Cargo.toml` 时把 `[patch.crates-io]` 里的
`ironrdp-pdu = { path = "vendor/ironrdp-pdu" }` 那一行替换掉了，`Cargo.lock` 于是从 crates.io 解析 `ironrdp-pdu`，
`dff91a2` 的 ClearCodec 修复（其中正是「单色调色板 RLEX 也占 1 位停止索引」那条）从那以后就没进过 helper。
抓到的那个 35 字节 PDU 正是单色调色板 RLEX。本 PR 恢复这一行，`Cargo.lock` 里 `ironrdp-pdu` 随之变回路径包
（不再有 `source`/`checksum`）；重建后连接稳定，本文所有轮次都基于修复后的 helper。

## 五、本 PR 对工具的两处小改动

- `rdp_probe` 的 `LAYOUT` 命令：每项可带 `@百分比`（`desktopScaleFactor`），用于第三节；
  同时把 `left` 改为累加宽度——`--monitors` 在 PR #247 已修过 `index × width` 的同一个 bug，`LAYOUT` 这条路径还留着。
- `drive.py` 增加 `--relayout`：连接后发一次 `LAYOUT`，等 `layout_result` 和 3 s 稳定期，再读拓扑、开始逐点比对。

## 局限（如实列出）

1. **GUI 一体化验收（`scripts/run_pointer_acceptance.sh`，本地 OS 指针 → 线上 → 服务端一次跑通）没有做。**
   它要求一个 2416×898 的窗口精确放在 `(0,0)`，而这台 VM 的控制台只有 1600×1200 一种显示模式（`EnumDisplaySettings`
   只列出这一个），窗口放不下、`SetCursorPos` 也到不了右半屏。所以「本地 → 线上」仍只靠 #233 那一跑，
   「线上 → 服务端」靠本文，两段没有在同一次运行里串起来。
2. **测的是服务端缩放，不是客户端缩放。** 第三节改的是 Windows 会话的 DPI；Adit 窗口所在的本地显示器缩放 ≠ 100%
   时本地坐标 → 线上坐标的换算，仍未验证（`rdp_pointer_acceptance` 本身也假定 100%）。
3. 缩放是连接后用 Display Control 通道设的；helper 在连接时的 Client Core Data 里 `desktopScaleFactor` 填的是 0，
   连接即带缩放的那条路径没有覆盖。
4. 服务端是本机回环，客户端与服务端同一台机器、同一个 Windows 版本（Server 2022）。Windows 10/11 客户端版的
   RDP 服务端、以及经过 RD Gateway 的连接没有测。
5. 每个点只测了「到达」，没有测拖动轨迹、按键、滚轮；坐标都在显示器并集之内，没有测并集之外（不等大屏左下的空洞）的钳位行为。
6. session 3 与 session 7 两次运行分别因 Git Bash 路径格式、probe 未重建，在发出第一个点之前就失败，没有产出数据，不计入上表。

## 文件

| 文件 | 内容 |
| --- | --- |
| `CursorLogger.cs` | 会话内记录器源码（.NET Framework 4 `csc.exe` 编译；文件头有计划任务的注册方式） |
| `oracle.py` | 读记录器最新样本的 oracle（`pointer` / `topology`） |
| `run.sh` | 每轮的注销 + `drive.py` 调用 |
| `*.jsonl` | `drive.py` 的逐点记录（`server_topology`、`relayout`、`pointer`） |
| `*.txt` | 同一轮的完整控制台输出（含 `rdp_probe` 全部输出） |
| `*.logger.jsonl` | 该轮会话内记录器的原始样本（按会话号从 `samples.jsonl` 切出） |
