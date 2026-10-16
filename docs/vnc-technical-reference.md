# VNC 实现与产品调研：Adit 技术参考

调研日期：2026-10-16（Asia/Shanghai）。Adit 评估基线：[`d838a27`](https://github.com/weironz/adit/commit/d838a27d2bb62cdcbd7fdd2df14add8cca95fde0)。

本文回答三个问题：当前 VNC 是否完成、主流方案该如何比较、Adit 下一步应补哪些核心能力。本文是有日期的技术评估；任务状态、讨论和验收记录以文中链接的 GitHub Issues 为准，不在此维护第二份进度表。

## 1. 结论与完成边界

**Adit 的 VNC 首版已完成，完整的通用 VNC 客户端能力尚未完成。** 新建会话、标准密码连接、画面、键鼠、缩放、全屏、基础剪贴板和手动重连已实现。x11vnc / TigerVNC 已做真实连接测试；用户已确认首屏和鼠标操作明显改善。正式发行版尚未发布，不能将开发构建可用写成已经发布。实现与验收记录见 [#201](https://github.com/weironz/adit/issues/201) 和 [基线 CI（12/12 通过）](https://github.com/weironz/adit/actions/runs/38871302371)。

**MobaXterm 是很合适的多协议运维工作台参照，但没有证据支持“VNC 综合最好”的排名。** 原生通用客户端重点参考 TigerVNC，Linux 3D / 远程可视化参考 TurboVNC，Windows 现场支持参考 UltraVNC，商业一体化参考 RealVNC Connect；浏览器接入参考 noVNC / Apache Guacamole。选择依据应是实际服务端、安全类型和使用场景，而非品牌或功能数量。依据与限制见第 3 节。

**建议继续使用 Rust + `vnc-rs` 路线，先做安全接入和协议完整性，再做画质自适应。** 暂无充分理由替换已通过实测的后端。若认证兼容成为明确阻塞，再以同一测试矩阵比较 LibVNCClient 或商业 SDK。WebView + noVNC 可用于未来 Web 产品，但不适合作为当前 iced 原生客户端的默认改造路线。

## 2. 调研方法与证据等级

- 产品能力来自官网、官方手册、官方仓库和协议规范；没有用厂商互相比较的营销榜单给产品打分。
- **已验证**：Adit 源码、CI 或本次工作链中的实际连接结果；**官方说明**：文档声明支持，未在本次横向实测；**建议/估算**：工程判断，不是厂商保证或已实现能力。
- 未做 MobaXterm、TigerVNC、TurboVNC、RealVNC 的同机同网性能竞赛，不能由本文得出帧率、延迟或带宽的冠军。
- 在线手册可能先于发行包更新。尤其 TigerVNC 的 Wayland / H.264 条目与 Royal TS 的插件改版，应核对实际安装版本；本文不把主分支或预览功能当作所有发行版共有功能。
- 不记录真实测试地址、密码、认证报文或用户桌面截图。采购价格、订阅档位和 OEM 条款变动较快，仅链接官方入口，不把当前报价写成长期结论。

## 3. 方案和产品地图

### 3.1 原生客户端、服务端与多协议工作台

以下是角色与能力比较，不是性能排名。“开源”指对应组件的许可；客户端、服务端、商业 SDK 及所捆绑依赖须分别核对。

| 产品/项目 | 开源或商业形态 | 角色、主要平台 | 适用场景与可借鉴能力 | 边界与 Adit 关系 |
|---|---|---|---|---|
| **MobaXterm** | 闭源主程序，包含开源组件；Home / Professional | Windows 多协议工作台，内嵌 VNC 客户端 | 统一会话、SSH 网关、终端/SFTP/桌面协同；26.5 扩展 VNC 编码和认证 | Adit 的直接产品体验对照；免费版有会话、隧道等限制；不能由此推断全部 VNC 扩展均支持。[官网文档](https://mobaxterm.mobatek.net/documentation.html)、[版本记录](https://mobaxterm.mobatek.net/download-home-edition.html)、[版本区别](https://mobaxterm.mobatek.net/download.html) |
| **TigerVNC** | 开源，GPL 系列及所含组件许可 | 通用原生 Viewer；Linux 虚拟桌面/共享桌面服务端，Viewer 有多平台发行包 | 自动选择编码/像素格式、TLS/X.509、安全类型、系统键路由、远端改分辨率 | 适合作为协议与交互基准；Viewer 平台支持不等于各平台都有同等 Server。当前手册还列出 w0vncserver，是否可用须核对发行包与桌面环境。[项目](https://tigervnc.org/)、[Viewer 手册](https://tigervnc.org/doc/vncviewer.html)、[Wayland 服务端](https://tigervnc.org/doc/w0vncserver.html)、[许可](https://github.com/TigerVNC/tigervnc/blob/master/LICENCE.TXT) |
| **TurboVNC + VirtualGL** | 开源，组件许可分别核对 | Linux/Unix 服务端和跨平台 Viewer；远程 3D/可视化 | Tight/JPEG、连续更新、流控、SSH 会话管理、无损补刷；VirtualGL 负责 3D 渲染配合 | 更适合高变化画面与 GPU 工作站，不能将服务器侧 GPU 加速等同于客户端加个编码。是 WAN 优化参考。[特性](https://turbovnc.org/About/Features)、[背景](https://www.turbovnc.org/About/Introduction)、[VirtualGL](https://virtualgl.org/About/Background)、[许可](https://github.com/TurboVNC/turbovnc/blob/main/LICENSE.txt) |
| **UltraVNC** | 开源 GPL；插件单独核对 | Windows Server / Viewer，远程支持 | 文件传输、Repeater、SC、加密插件 | Windows 服务端兼容测试重点；DSM/认证/文件扩展不自动与其他 VNC 互通。PcHelpWare、uvnc2me 不属于同一 RFB 兼容集合。[官方产品表与兼容说明](https://uvnc.com/) |
| **TightVNC** | 开源 GPL；另有商业源码/SDK 许可 | 以 Windows Server / Viewer 为重点，亦有其他组件 | Tight/JPEG、Windows 文件传输、缩放、只读密码 | 官方 FAQ 说明传统连接的桌面流未加密，建议 SSH 隧道。协议参考价值高，不应因名称含 Tight 就假定具备 TLS 或 UTF-8。[介绍](https://www.tightvnc.com/intro.php)、[安全说明](https://www.tightvnc.com/faq.php)、[许可](https://www.tightvnc.com/licensing.php) |
| **RealVNC Connect** | 闭源商业产品；另有 OEM / SDK | 多平台 Viewer / Server，云连接及直连按产品/订阅配置 | 端到端加密、身份与权限、连接管理，商业支持 | 成对部署功能更完整；第三方 Viewer 不能据此获得 RealVNC 云连接或全部私有安全扩展。Adit 仅支持 VncAuth 时不能宣称兼容默认安全配置。[加密](https://help.realvnc.com/hc/en-us/articles/37653068502045-Connection-Encryption-Methods)、[第三方边界](https://www.realvnc.com/en/connect/download/vnc/linux/)、[OEM](https://www.realvnc.com/en/realvnc-oem-solutions/) |
| **Remmina** | 开源 | Linux 多协议工作台 / Viewer | 质量、色深、光标、只读、剪贴板开关；多协议会话组织 | 适合参考 Linux 原生体验；其 VNC 插件支持不等于每个发行包启用了所有可选库/安全后端。[VNC 页面](https://remmina.org/remmina-vnc/)、[插件能力](https://remmina.org/remmina-features/) |
| **mRemoteNG** | 开源 GPL | Windows 多协议标签式连接管理器 | 连接目录、统一入口和插件组织 | 适合比较会话工作流；不是证明某一 VNC 解码器性能领先的依据。[官方仓库](https://github.com/mRemoteNG/mRemoteNG) |
| **Royal TS / TSX** | 商业连接管理产品 | Windows / macOS，具体插件按平台和版本 | 凭据引用、Secure Gateway、VNC 交互策略；新 RoyalVNC 强调认证与隔离 | 老页面仍讲 TightVNC/UltraVNC，当前 Royal TS 文档已讲 RoyalVNC，不能混写为同一代实现。2026 改版文章曾以 Beta 介绍，部署需核对实际版本。[当前连接文档](https://docs.royalapps.com/products/royalts/reference/connections/vnc.html)、[插件改版说明](https://blog.royalapps.com/blog/royal-ts-v26-a-closer-look-at-the-new-royalvnc-plugin) |
| **Devolutions Remote Desktop Manager** | 商业产品，Free / 付费能力有别 | 多平台多协议工作台 | 凭据库、连接权限、网关；VNC 与团队运维流程整合 | 对团队治理的参考价值大于给 VNC 编码器排名。Devolutions Gateway 是额外部署的网关，不能与桌面 Viewer 混为一谈。[连接管理](https://devolutions.net/remote-desktop-manager/features/remote-connection-management/)、[Gateway](https://devolutions.net/gateway/) |
| **Apple Screen Sharing / Remote Desktop** | Apple 平台内置/商业工具 | macOS 桌面共享与管理 | Apple 桌面生态和权限协同 | Apple Remote Desktop 扩展与传统 VncAuth 是不同兼容层级；允许第三方密码连接不等于提供全部 ARD 能力。[Apple 官方说明](https://support.apple.com/en-mo/guide/remote-desktop/apde0dd523e/mac) |

### 3.2 浏览器接入与 Linux 共享桌面服务端

| 方案 | 架构/许可 | 强项 | 必须分清的边界 |
|---|---|---|---|
| **noVNC + websockify** | 浏览器 JavaScript RFB 客户端，主要 MPL-2.0；可用 WebSocket→TCP 代理 | 本地光标、Unicode 剪贴板、多种编码及认证，适合嵌入 Web 控制台 | 不是 VNC Server；服务端须支持 WebSocket 或增加代理。WSS 只保护到其终止点，代理到 VNC Server 的链路须另行保护；VeNCrypt Plain 支持不能等同于浏览器能处理任意 VeNCrypt TLS 子类型。[官方仓库与能力清单](https://github.com/novnc/noVNC) |
| **Apache Guacamole** | 浏览器→Guacamole→guacd→VNC，Apache 项目，实际依赖另行核对 | 集中认证、权限、录制、多协议 Web 入口 | 与原生直连相比多一层网关部署。VNC 连接上的 SFTP 文件传输是另开的 SSH/SFTP 能力，不是通用 RFB 文件传输。[VNC 配置手册](https://guacamole.apache.org/doc/gug/configuring-guacamole.html#vnc) |
| **x11vnc** | X11 实体桌面共享 Server，GPL-2.0 | 分享已有 X 显示；支持若干 TLS/认证及厂商扩展 | 不是通用 Wayland Server。官方 README 当前标注维护者缺位，适合兼容回归，不宜视作无维护风险的新架构基座。[官方仓库](https://github.com/LibVNC/x11vnc) |
| **wayvnc** | wlroots Wayland Server，项目 ISC，依赖另核对 | 无头/实体 Wayland 会话、VeNCrypt 等安全接入 | 官方明确不支持 GNOME / KDE / Weston；Wayland 不能一概写成兼容或不兼容。它依赖 neatvnc，Adit 连接它仍须匹配 RFB 安全类型。[官方仓库](https://github.com/any1/wayvnc) |

RustDesk、TeamViewer、AnyDesk 等可以作为远程控制体验的邻近对照，但不应直接列为任意第三方 RFB/VNC Server 的替代 Viewer，也不应将其编码/中继能力算作标准 VNC 已有能力。本文后续计划聚焦 Adit 的 VNC 客户端，不扩展成自建远控云、VNC Server 或完整远程支持平台。

### 3.3 MobaXterm 是否是首选参照

截至调研日，官方 Home 下载页列出的当前版本是 **26.5（2026-09-11）**，并列出新增 Tight、Hextile、RRE、TRLE 编码，Tight、SASL、UltraVNC、MsLogon2 认证，以及快捷键、分辨率变化和剪贴板改进。[官方变更记录](https://mobaxterm.mobatek.net/download-home-edition.html)

因此，MobaXterm 的 VNC 已有持续投入；Adit 可以借鉴其“保存会话→安全接入→操作桌面→文件协同”的完整流程。但该页面没有给出足以确认 VNC 引擎全部来源、每个 TLS 子类型或所有服务端兼容性的证据；本文不猜测内部实现，也不使用 RDP 的 Enhanced graphics 选项代替 VNC 能力说明。

建议对照组：

- **日常混合运维**：MobaXterm + Adit，比较会话、网关、键盘、剪贴板和恢复流程。
- **通用 RFB 正确性**：TigerVNC Viewer + Adit，连接相同服务端、安全类型和桌面。
- **WAN / 3D**：TurboVNC Viewer/Server 配对作为专门场景参照。
- **Windows 服务端兼容**：UltraVNC、TightVNC；**安全策略严格的成套部署**：RealVNC Connect。
- **Web 入口**：noVNC；**集中访问平台**：Guacamole，分别评估部署与端到端链路。

上述选择是基于官方功能定位的工程建议，不是实测名次。

## 4. RFB 协议层的关键区别

VNC 体验至少取决于五层：服务端采集/会话、认证与传输、画面编码、流控与解码、客户端显示/输入。客户端只改其中一层，不能保证所有服务器的体验一致。

| 层面 | 关键点 | 对 Adit 的影响 |
|---|---|---|
| 基础协议 | RFB 3.3/3.7/3.8 存在握手差异；Raw 是兜底，CopyRect 复用已有像素 | 版本和编码需按实际协商测试，不能以单个 3.8 服务端代表全部兼容 |
| 安全 | VncAuth 为传统 DES challenge-response，不加密桌面流；None 无认证 | 强制安全策略、SSH 隧道或经验证的 TLS，必须与密码存储区分 |
| 压缩 | ZRLE 适合无损桌面；Tight 可组合无损与 JPEG；JPEG 有损 | 静态文字和大面积变化的最优策略不同；“压缩级别更高”不一定延迟更低 |
| 流控 | 增量请求可以等待变化；固定频率发请求不代表同等 FPS | 接收解码不能被定时器限速；需区分更新批次、待响应请求和真正呈现的帧 |
| 光标 | CursorPseudo 携带图像、透明度/掩码与热点；本地移动可省去视觉往返 | 协商扩展与真正显示服务端形状是两件事，空光标也要有语义 |
| 剪贴板 | 传统 CutText 是 Latin-1；UTF-8 需扩展协商 | 不能把 UTF-8 字节直接塞进传统消息，也不能因键盘支持 Unicode 就宣称中文复制可用 |
| 分辨率 | DesktopSize 告知服务器尺寸；ExtendedDesktopSize / SetDesktopSize 可协商调整 | 本地缩放、远端改分辨率、多显示器是三个不同功能 |
| 文件 | 厂商扩展差异大；通用文件互操作不能由“支持 VNC”推出 | 优先复用独立 SFTP，明确它需要额外凭据和 SSH 服务 |

基础消息、安全和字符集依据 [RFC 6143](https://www.rfc-editor.org/rfc/rfc6143.html)；扩展协商以 [rfbproto 社区规范](https://github.com/rfbproto/rfbproto/blob/152107db63cd34b3536ad8ddf54a0cfc9017a9f9/rfbproto.rst) 为参考。该社区文档不是新 RFC，也不能代表每家厂商所有私有扩展。ContinuousUpdates / Fence 等扩展必须先确认对端支持，不能盲发。

## 5. 可用于 Adit 的技术实现路线

| 路线 | 优点 | 成本/约束 | 建议 |
|---|---|---|---|
| **继续 `vnc-rs`，向上游补能力或维护小范围补丁** | Rust 异步，现有集成和测试可保留；库声明 MIT OR Apache-2.0 | 当前 0.6.0 的安全类型、事件边界与扩展能力有缺口，需要协议工程和维护 | **默认路线**；先补安全选择策略、完整更新事件、UTF-8，再评估 TLS；每个补丁带测试与退出条件。[上游](https://github.com/HsuJv/vnc-rs) |
| **LibVNCClient（LibVNCServer 项目中的客户端库）** | 官方列出更广的认证与编码支持，如 VeNCrypt、SASL、MSLogonII、ARD | C FFI、库构建、TLS/JPEG 依赖及所有平台打包；GPL 许可须评估，不能直接复制到当前 MIT 声明的工作区并维持原有许可假设 | 如确有认证兼容阻塞，做限定原型，比较测试通过率与维护成本；不要仅因“成熟”立即重写。[官方能力表](https://github.com/LibVNC/libvncserver) |
| **TigerVNC / TurboVNC 核心或外部 Viewer** | 实现成熟、可以迅速获得对照结果 | 原生 C/C++ / Java 或外部进程，窗口嵌入、许可、输入和升级各有成本 | 首先作为验收工具；外部启动可做特定环境兜底，但不能算原生内嵌能力完成 |
| **noVNC / Guacamole** | Web 入口成熟，有各自的协商与交互实现 | 引入浏览器/服务端网关，与 Adit 当前 iced、无 WebView 的路线不同 | 若将来明确要做 Web 运维平台再评估；不为修复几个协议缺口重构整个桌面架构 |
| **Remote Core SDK（TightVNC）** | 商业 Viewer 组件，官方提供无 GUI 的跨平台 C++ 版本 | 商业条款、二进制分发、FFI 和平台支持须与厂商核对 | 作为买与做的备选；订阅/再分发权不可从 TightVNC 免费使用条款推导。[官方许可入口](https://www.tightvnc.com/licensing.php) |
| **RealVNC SDK / OEM** | 成套安全与嵌入支持，官方 C API | 商业授权、供应商依赖、支持矩阵、第三方 RFB 兼容范围和离线条件需验证 | 有企业/OEM 需求时再选，不作为当前普通桌面首版的前置条件。[OEM](https://www.realvnc.com/en/realvnc-oem-solutions/)、[SDK 文档](https://www.realvnc.com/en/developer/docs/latest/overview.html) |

许可栏是依赖选型约束，不是完整法律结论；采用新组件时应保存对应版本的许可和第三方声明。独立 helper 可提供故障隔离，**不能仅凭进程分离就断言没有许可证义务**。目前没有把以上备选库引入仓库。

## 6. Adit 当前实现审计

### 6.1 已具备的能力

| 能力 | 观察结果 | 源码/验证入口 |
|---|---|---|
| 会话配置 | VNC 类型、默认 5900、无需 SSH 用户名、密码单独保存 | [域模型](../crates/adit-domain/src/lib.rs)、[配置保存](../crates/adit-ui/src/profiles.rs) |
| 连接/认证 | TCP、VncAuth / None、共享会话、30 秒握手上限、认证失败原标签重试 | [VNC worker](../crates/adit-session/src/vnc_client.rs)、[会话层](../crates/adit-session/src/lib.rs) |
| 画面 | ZRLE / CopyRect / Raw，归一化 padding 为 alpha；服务端尺寸变化 | 同上；[线协议测试](../crates/adit-session/tests/vnc.rs) |
| 交互 | X11 keysyms、Unicode keysym、左右修饰键、失焦释放、移动合并、点击滚轮 | [键盘编码](../crates/adit-ui/src/input.rs)、VNC worker |
| 显示与会话操作 | 标签页、适应窗口/原始比例、全屏、Ctrl+Alt+Del 按钮、手动重连 | [显示](../crates/adit-ui/src/workspace.rs)、[会话操作](../crates/adit-ui/src/session_ops.rs) |
| 剪贴板 | ASCII 文本双向，可关闭；有会话归属检查 | worker、[UI 事件](../crates/adit-ui/src/update_loop.rs) |
| 关闭与资源 | 独立取消信号可终止握手；输入队列有界，满队列明确关闭而非吞按键释放 | worker；握手取消/队列测试 |
| 自动验证 | 线协议、认证重试、尺寸、CopyRect、突发画面、10000 次移动后的点击顺序 | [测试](../crates/adit-session/tests/vnc.rs)、[CI](../.github/workflows/ci.yml) |

当前渲染复用了 RDP 的帧数据类型与图像切片，但传输和键盘编码各自独立。RDP 有的文件/图像剪贴板或全局键盘 hook，不能直接记为 VNC 已支持。

### 6.2 必须如实保留的缺口

1. **安全接入不完整。** VNC 会话配置的 SSH jumps 当前明确拒绝；TLS/VeNCrypt 未接入。`vnc-rs` 0.6.0 握手在同时提供 None/VncAuth 时优先 None，应用当前无强制认证/加密策略。应允许显式无认证配置，但不能在用户要求认证或加密时静默降级。
2. **更新批次与流控不完整。** 当前每 33 ms 发增量请求，接收已改为立即驱动；仍不是按完整 FramebufferUpdate 完成推进，也没有 ContinuousUpdates/Fence 协商。0.6.0 的公开事件没有常规完整更新结束通知，须补库接口，不能把某个 RawImage 当成一帧。
3. **光标仅是本地普通箭头。** 已协商 CursorPseudo，但没有处理 `SetCursor` 形状/热点/隐藏事件；这改善跟手感，不等于真实远端光标已完成。对端不支持扩展时也要覆盖双光标回退行为。
4. **中文剪贴板未实现。** 当前主动限制 ASCII；还没有完整 Latin-1 转码或 Extended Clipboard 的 UTF-8 协商。键盘 Unicode keysym 不等于 IME、多字符提交、组合字符或所有布局均已验证。
5. **WAN 自适应未实现。** Adit 未协商 Tight，也不处理 `JpegImage`；固定 RGBA32。虽然库里有 Tight 解码器，启用前仍需 JPEG 事件处理、资源限制、质量/压缩协商和回归验证。
6. **恢复与诊断不足。** 有手动重连；没有 VNC 专用自动重连/半开检测和安全策略恢复。`vnc_probe` 可测像素覆盖率，但没有协商详情、完整更新耗时、RTT/流量的产品诊断面板。
7. **只读和输入策略不足。** 没有会话级只读开关；Alt+Tab / Win 等系统键未提供 VNC 专用捕获和明确释放捷径。工具栏能发 Ctrl+Alt+Del 的 keysyms，不保证所有 Windows 服务端能触发安全注意序列，须验证服务端权限/配置。
8. **显示边界有限。** 本地缩放不改变远端桌面；未接入主动 SetDesktopSize、多屏选择或高 DPI 专门验证。图像尺寸上限目前为每轴 8192、总像素 8,294,400，双 4K 等布局会超过限制；不能只放开上限而不做内存预算。
9. **服务端覆盖仍窄。** 已测 x11vnc、TigerVNC 与一个授权 WAN 目标；未完成 Windows TightVNC/UltraVNC 默认配置、RealVNC、Apple、Wayland 的系统性矩阵。跨平台“能编译”不是对应服务器“能连接”的证明。
10. **凭据存储是跨协议遗留约束。** 当前目录内凭据使用加密，但 KDF 的秘密输入来自内置 key，没有用户主密码；不能把它描述为能防御持有配置文件和程序的人。需单独评估可选主密码/密钥管理与同步迁移，见 [现有安全模型](../README.md#credential-storage)。

上述库行为核对本地锁定的 `vnc-rs = 0.6.0` 源码（`client/connector.rs`、`client/connection.rs`、`event.rs`、`limits.rs`），不是从上游主分支能力推测 Adit 已实现。依赖声明见 [Cargo.toml](../crates/adit-session/Cargo.toml)，第三方文档与 Adit 接入层须分别评估。

### 6.3 已完成的性能修复意味着什么

同一授权 WAN 目标、2560×1369：优化前完整首屏像素覆盖约 12.452 秒，优化后两次约 1.643 / 2.192 秒；握手均约 1 秒。主要原因是解码器两槽输出队列与应用 16 ms 轮询形成吞吐限制，已改为事件驱动接收；连续鼠标移动也已合并，且协商了本地光标。

这些是少量顺序测量，**不是产品对比基准、不是 P95、不是屏幕到屏幕延迟**。统计点是本地共享帧缓冲像素覆盖，不是 GPU 已显示，也不是协议完整更新事件。用户确认首屏与鼠标明显改善，证明此次实际使用问题得到改善；仍不能代替后续弱网、持续变化、4K 和长期运行测试。[实现/测量记录](https://github.com/weironz/adit/issues/201#issuecomment-6211024063)、[只读诊断工具](../crates/adit-session/examples/vnc_probe.rs)

## 7. 核心开发计划清单

**以下是调研后的建议工作包，尚未开始开发，不代表已承诺发布日期。** 优先级以当前“普通 Linux / Windows 桌面、原生运维客户端”为目标；P0 表示下一轮应优先补的基础，P1 是日常可用性/安全兼容，P2 是条件成立后推进。

估算单位为一名熟悉 Rust/iced/RFB 的开发者的有效人日，包含对应实现、测试和文档，不包含等待设备/授权/证书环境及发布排队。协议库改动、Windows 服务端权限和 TLS 互操作是主要不确定项；这些范围不是一天内逐项勾选即可完成的 UI 开关。

| 顺序/优先级 | 工作包（动态状态见 Issue） | 范围与验收重点 | 估算 | 依赖 |
|---|---|---|---:|---|
| 01 / P0 | [#202](https://github.com/weironz/adit/issues/202) 安全类型选择与禁止静默降级 | 显式 None/VncAuth/要求安全传输策略；同时提供 None+VncAuth 时遵守配置；未满足策略不得进入桌面 | 2–3 人日 | 无 |
| 02 / P0 | [#203](https://github.com/weironz/adit/issues/203) 完整更新边界、流控与脱敏诊断 | 库输出更新完成事件；普通模式限制待响应请求；空闲不重刷；持续更新仅在协商成功后启用 | 4–7 | 无；为编码/恢复提供基础 |
| 03 / P0 | [#204](https://github.com/weironz/adit/issues/204) 内置 SSH 网关接入 | 独立 SSH 身份与主机密钥校验；VNC 走受控流；错误/关闭可取消且释放资源 | 4–7 | 安全策略；复用现有 SSH，但处理 #101 / #113 边界 |
| 04 / P0 | [#205](https://github.com/weironz/adit/issues/205) 服务端与弱网验收矩阵 | Windows TightVNC/UltraVNC、Linux Tiger/Turbo、X11/Wayland；有版本、设置、成功/失败证据 | 4–7 首轮 | 测试主机；贯穿后续工作 |
| 05 / P1 | [#206](https://github.com/weironz/adit/issues/206) Latin-1 / UTF-8 文本剪贴板 | 扩展协商后中文、emoji、多行双向；旧端正确回退；限长与压缩上限；跨标签隔离 | 3–6 | 协议层扩展 |
| 06 / P1 | [#207](https://github.com/weironz/adit/issues/207) 服务端光标形状与热点 | 正确 RGBA/掩码、热点、空光标；DPI/缩放和不支持扩展时回退，无双光标 | 2–4 | 无 |
| 07 / P1 | [#208](https://github.com/weironz/adit/issues/208) Tight/JPEG 与会话画质档位 | 每个宣告支持的子编码都处理；无损文字档/WAN 档；可恢复无损，收益由基准决定 | 4–7 | 更新边界、矩阵 |
| 08 / P1 | [#209](https://github.com/weironz/adit/issues/209) VeNCrypt/X.509 安全直连 | 验证证书链/主机名；可管理自签名信任及变化；拒绝错误证书和禁止降级；分清匿名 TLS | 7–12 | 安全策略、协议库能力 |
| 09 / P1 | [#210](https://github.com/weironz/adit/issues/210) 系统键路由与国际键盘 | 本地/远端路由、释放捷径、AltGr/IME/多字符提交验证；安全注意序列明确限制 | 2–4 | 输入矩阵 |
| 10 / P1 | [#211](https://github.com/weironz/adit/issues/211) 会话级只读模式 | 禁止键鼠/粘贴输入，方向性剪贴板策略明确；只读切换前释放所有已按下输入 | 1–2 | 无 |
| 11 / P1 | [#212](https://github.com/weironz/adit/issues/212) 断线检测与受控重连 | 半开/重置/睡眠恢复；有限退避；不重试错误密码/证书/主动断开；不降低原安全要求 | 3–5 | 流控、安全策略；借鉴 #112 |
| 12 / P2 | [#213](https://github.com/weironz/adit/issues/213) 单屏远端分辨率协商 | 默认仍本地缩放；主动更改需能力确认与明确开关；拒绝/超时保持现状，不影响其他共享者 | 4–7 | 更新边界、矩阵 |
| X1 / 跨协议 P1 | [#214](https://github.com/weironz/adit/issues/214) 可选主密码与凭据迁移 | 真实用户秘密参与密钥派生、旧文件迁移/回滚、锁定、同步冲突与不可恢复情形明确 | 5–10 | 独立安全设计；不因 VNC 代码接入而自动完成 |

推荐分阶段推进：

1. **阶段 A：可控接入与可测量（01–04，约 14–24 人日）。** 先补安全策略、更新边界/诊断、SSH 网关，并建立矩阵。矩阵准备与实现穿插，但不要把共享一名开发者的任务重复算成并行缩短日历。
2. **阶段 B：日常桌面体验（05–07、09–11，约 15–28 人日）。** 中文复制、真实光标、画质档位、系统键、只读和受控恢复；每个工作包独立提交验收。
3. **安全直连分支（08，约 7–12 人日）。** 若下一批目标必须使用 VeNCrypt，应提前到阶段 A，届时总工期相应增加；若均经 SSH/VPN，可先完成阶段 B。
4. **后续显示能力（12，约 4–7 人日）。** 有确实需要远端改尺寸的目标再做。X1 是独立的应用安全工程（5–10 人日），涉及全协议与同步，单独评审和排期。

完成 A+B 的初步范围约 **29–52 人日**，按一人投入约 6–11 个有效工作周；不含 VeNCrypt、安全存储迁移、动态分辨率和等待外部环境。各范围置信度中等偏低，完成阶段 A 的协议改动与兼容测试后应重新估算。没有要求立刻把清单所有项目开发完。

### 7.1 暂缓项和触发条件

| 项目 | 当前建议 | 何时重新评估 |
|---|---|---|
| 脏矩形/脏切片 GPU 上传 | 先度量，不立即改共享 RDP 渲染器 | 4K/多会话下确认帧缓冲全量 clone、上传占 CPU/内存或显示延迟预算的主要部分 |
| 多显示器布局与超过当前像素上限的桌面 | 单独设计内存预算、显示选择和输入几何 | 有真实双屏/4K 用户与服务器，单屏动态尺寸已稳定 |
| H.264 / 视频优先路径 | 不以“更先进”为由优先实现 | Tight/JPEG 后仍有明确视频/3D吞吐缺口，并有同版本服务端/解码器支持与基准收益 |
| UltraVNC DSM / MSLogonII、SASL、RealVNC RA2、Apple ARD | 按目标补，不追求一次全覆盖 | 测试矩阵中有真实受阻服务器；能明确协议资料、凭据策略与验收标准 |
| VNC 内置文件/图像传输 | 暂缓；普通文件优先独立 SFTP | 明确要求无 SSH 的 Windows 文件传输，且已选定厂商扩展；不将未知 files 标志当成通用文件协议 |
| Reverse VNC、Repeater | 暂缓 | 有远程支持/NAT 反连场景，不只是直连运维 |
| 录屏、音频、远端打印、云中继、团队权限平台 | 不作为本轮 VNC 核心 | 独立产品需求与运维/存储/服务端预算明确 |
| VNC 独立 helper 进程 | 先保持线程隔离，记录不可信解码资源上限 | 引入 C SDK/崩溃风险或库行为证明进程隔离收益显著；不能为了形式与 RDP 完全相同而增加 IPC |

## 8. 下一轮验收与横向测试设计

### 8.1 服务端矩阵

| 目标 | 安全配置 | 当前证据/下一步 |
|---|---|---|
| x11vnc + Xvfb / X11 | VncAuth | CI 已验证；继续保留回归，并记录维护风险 |
| TigerVNC Xvnc | VncAuth；后续 X509/VeNCrypt | VncAuth CI 已验证；TLS 另测，不复用明文结果 |
| TurboVNC | VncAuth / 经 SSH；后续 TLS | 下一轮真实测试；覆盖高变化画面及流控 |
| Windows TightVNC / UltraVNC | 标准 VncAuth；厂商认证/插件分别建条目 | 需要 Windows 服务端实测；UAC、锁屏、服务模式、Ctrl+Alt+Del 特别记录 |
| RealVNC Server | 实际订阅允许的直连安全配置 | 不改成无认证来伪造“默认兼容”；所需类型未支持应明确失败 |
| wayvnc / TigerVNC w0vncserver | 对应发行版与桌面环境，按实际 TLS/认证 | 标明 compositor / portal / 权限，客户端仍按 RFB 协商 |
| macOS Screen Sharing / ARD | VncAuth 与 Apple 扩展分开 | 后续目标，不以 Windows/macOS 客户端编译通过替代服务端测试 |

### 8.2 统一负载与指标

- 原生比较至少选择 Adit、MobaXterm 26.5、TigerVNC；3D 另加 TurboVNC 成对部署。固定服务端版本、分辨率、桌面内容、显示缩放、安全层与客户端机器。
- 静态文本/编辑、快速滚动、窗口拖拽、图像/视频变化分组；1080p、当前约 3.5M 像素桌面、单 4K 分开测试。4K=8,294,400 像素，已接近当前硬上限，观察完整链路内存。
- 网络至少包含 LAN、RTT 80/180 ms、带宽 5/20 Mbps，以及受控丢包场景；使用隔离测试网络或代理，不改用户生产网络。TCP 丢包导致的阻塞与应用调度分开记录。
- 每条件至少 10 次连接，记录原始样本、中位数/最小/最大；不要用 10 次估计可靠 P95。稳定性另跑 2–8 小时，记录资源峰值、断连和退避。
- 指标分层：TCP/认证耗时、首个像素、完整协议更新、像素覆盖、实际显示、输入到远端测试程序收件/画面响应；CPU/RSS、收发字节、待响应请求数和队列高水位。
- 输入往返用受控测试窗口按序号改变像素并记录接收；如做屏幕到屏幕延迟需单独仪器/采样方案，不以 TCP 建连时间或鼠标主观感觉代替。
- 硬性验收：输入无丢释放/错序；关闭/切标签无卡键；内存和待响应请求有界；错误证书/错误密码不得自动降级；不支持的扩展安全回退，不能黑屏或崩溃。

### 8.3 代码、凭据和发布门槛

功能改动按仓库执行 `just ci`，推送后检查**该提交 SHA** 的完整 CI；x11vnc/TigerVNC 实连继续纳入 Linux CI，Windows 服务端结果单独留证。诊断默认脱敏，不保存认证数据、键入内容或剪贴板正文；桌面快照仅在显式操作时输出到用户指定位置。凭据不得进入 argv、日志、issue 或 Git。

本文只交付调研和计划，不实施新协议能力、不变更服务端配置、不发版。发行时另按 [release.md](release.md) 执行；开发构建验证通过不自动触发 release。

## 9. 资料索引

正文各表已就近链接一手资料。复查时优先使用以下入口；其中主分支页面会变化，固定版本结论应另记录 tag/SHA。

- [RFC 6143](https://www.rfc-editor.org/rfc/rfc6143.html)；[rfbproto 固定快照](https://github.com/rfbproto/rfbproto/blob/152107db63cd34b3536ad8ddf54a0cfc9017a9f9/rfbproto.rst)。
- [MobaXterm 当前版本记录](https://mobaxterm.mobatek.net/download-home-edition.html)；[产品文档](https://mobaxterm.mobatek.net/documentation.html)。
- [TigerVNC Viewer](https://tigervnc.org/doc/vncviewer.html)；[TurboVNC 特性](https://turbovnc.org/About/Features)；[UltraVNC](https://uvnc.com/)；[TightVNC FAQ](https://www.tightvnc.com/faq.php)。
- [RealVNC 加密方法](https://help.realvnc.com/hc/en-us/articles/37653068502045-Connection-Encryption-Methods)；[OEM/SDK](https://www.realvnc.com/en/realvnc-oem-solutions/)。
- [noVNC](https://github.com/novnc/noVNC)；[Guacamole VNC](https://guacamole.apache.org/doc/gug/configuring-guacamole.html#vnc)；[Remmina](https://remmina.org/remmina-features/)；[RoyalVNC](https://docs.royalapps.com/products/royalts/reference/connections/vnc.html)；[Devolutions RDM](https://devolutions.net/remote-desktop-manager/features/remote-connection-management/)。
- [LibVNCClient/Server](https://github.com/LibVNC/libvncserver)；[vnc-rs 上游](https://github.com/HsuJv/vnc-rs/tree/99ed1a28553c7594212adf30b9517d57fe46558d)；[TightVNC 商业 SDK 许可](https://www.tightvnc.com/licensing.php)。
