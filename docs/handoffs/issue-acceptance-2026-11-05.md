# Adit GitHub issue 实机验收交接

交接日期：2026-11-05（北京时间）。目标仓库：`D:\codes\adit`，GitHub：<https://github.com/weironz/adit>。

本文为用户要求纳入仓库的任务交接记录；GitHub Issues 仍是需求和验收状态的权威来源。本文没有密码、密钥或解密后的凭据。接手后先读仓库 `AGENTS.md`，刷新远端状态，再执行验收。

## 1. 用户目标与本线程实际交付

用户要求：继续修复 GitHub issue；使用 computer-use 完成能自主完成的实机验收，通过后关闭 issue；恢复此前停掉的远端 RustDesk；明确报告确实存在的限制。用户随后指定优先验收 #92，并已明确授权验收通过后直接关闭。

本线程推进到的结果：

- 本地构建过当前 main 的 Adit；单窗和双窗原生 UIA 验收各退出 0。
- 生产 RDP helper 连接过真实 GNOME Remote Desktop，收到光标位图、热点、Hidden、显示能力以及桌面帧；CLIPRDR 文件流能力协商有双端日志。
- 当前版本 Adit GUI 成功连接真实 RDP，实际看到 GNOME 登录界面；密码框处出现文本形状光标。但没有完成连续、留档的光标往返切换、热点和退出远端区域恢复验收。
- 已将部分证据写入 #92、#94、#116 评论，链接见后文。
- 远端 RustDesk 服务曾恢复至 enabled/active，`--service`、`--server`、`--tray` 进程出现；当时绑定的是登录界面，未证明原用户桌面完全恢复。
- 截至初次交接，本线程没有关闭任何 issue，没有发布版本。随后补入验收样本和 IPC 测试稳定性修正，见下方增量。不要把此前其他线程的合入提交当成本线程新增修复。

## 2. 为什么迟迟没闭环：需要纠正的执行问题

除真实缺口外，主执行 agent 的方法也导致了延误：

1. **界面证据定位不可靠。** `cua_repl` 对 Adit 的 AX 状态和截图有时对应不同前台内容。曾把 Windows 锁屏图片误判为本机必然锁屏，并多次让用户解锁；也捕获过 Codex、Chrome。后来调用 AX 暴露的 `Raise` 后，确实取得 Adit 画面。后续必须在每次关键截图核对标题栏、窗口 ID、进程和可见内容，不能从一张截到的其他窗口画面推出系统不可操作。
2. **误判连接模式。** 曾将成功的 RDP 连接说成 GNOME 用户桌面镜像模式；服务端 journal 实际表明系统服务在处理连接并发送重定向。此结论已在 #92 评论更正。不能把“停止 RustDesk 后一次连接成功”解释成已证实 RustDesk 导致 session inhibited。
3. **验收没有一次做完整并保存。** 同一项反复检查状态、重启连接、临时注入，却没有冻结被测二进制、捕获连续操作和像素证据。#92 已接近完成，但仍没有可复查的最终报告。
4. **关闭和发布条件混淆。** 曾把 CI/新版本发布描述成一切 issue 关闭的绝对前提，过于笼统。CI 是发布门禁；issue 是否 completed 要看原范围和验收标准。仓库要求 shipped 工作按版本归档，不能未发布就谎称 shipped。参照现有 #227（代码验收后关闭，但当时明确未发布）与 AGENTS.md，明确说明完成、发布、外部验收各自状态；有歧义时提出具体关闭方案，而非反复卡住全部项目。
5. **没有区分功能尚缺与验收尚缺。** #94 的双向直接拖放、#93 的 macOS 仍缺实现，继续重复能力协商不会使其闭环；#92 的 GUI 缺口则应集中完成最小场景，不必扩展为无限平台认证。
6. **错误放大硬限制。** 一块物理显示器不自动等于不能验收 RDP 多屏；可以先确认服务器是否允许创建多个虚拟显示器。SSH/ping 超时也不证明主机关机。

后续不要继续本线程的“状态检查 -> 未完成 -> 让用户继续”的循环。为每个选中的 issue 写出有限的通过条件，完成一次连续验收，保存证据，然后做关闭决定。

## 3. 2026-11-05 现场刷新结果

以下为交接时新查到的事实，其余历史证据明确标注日期：

- 本地 HEAD：`46c01098a49c753b06de59a83fc27de27d4abdb1`，`fix(ui): reject ambiguous split file drops (#229)`。没有执行 fetch，不能仅凭本地 origin 引用断言远端分支未变化。
- 最新 GitHub Release：`v0.1.77`，2026-10-26 发布。main 上的后续实现不能当成已发布。
- runner：`home-workstation-windows-adit-01`、`home-workstation-linux-adit-01` 均 offline。
- [CI 41264012037](https://github.com/weironz/adit/actions/runs/41264012037) 最终 completed/cancelled：Windows 子任务 success，Linux 子任务 cancelled，整轮没有通过。不要沿用历史“Windows 正忙、Linux 排队”的状态。
- GitHub milestones API 返回空列表。没有创建 milestone，也不能伪造某版本已交付。
- 开放 issue 共 13 项，新增 [#230 VNC 字体重影](https://github.com/weironz/adit/issues/230)。本线程没有分析其截图或复现。
- 对既有 GNOME 主机的有界 SSH 检查超时。未重新验证远端 RustDesk、RDP 和登录会话当前状态；不能宣称服务仍健康或主机已关机。
- 当前未发现 `adit-app` / `adit-rdp-host` 正在运行。之前 #92 的临时配置目录本次检查不存在，不能复用旧 PID、HWND 或连接状态。

既有工作区脏状态（未改动，勿清理或混入提交）：

```text
 M .gitignore
?? adit-workspace-concept.png
?? target-issue-227/
?? target-issue94-session/
?? target-issue94-ui-refusal-20261029/
?? target-rdp-epoch-helper-isolated/
?? target-rdp-epoch-session-isolated/
```

## 4. 优先处理 #92：服务端光标

Issue：<https://github.com/weironz/adit/issues/92>。

### 已有证据

- main 已实现服务端光标的 helper -> IPC -> SessionManager -> GUI 路径，相关实现始于 `473a155`。交接时没有重新跑全部测试。
- 2026-11-01 真实 GRD 系统模式连接：收到 `Pointer(Bitmap)`，24×24，热点 (4,4)，以及 `Pointer(Hidden)`。
- 2026-11-02 使用当前 main 本机构建的 `adit-app.exe` 和独立 helper，隔离配置中只放目标 RDP profile 及其对应的加密凭据。GUI 曾显示已连接并出现 GNOME 登录界面。
- 点击密码框时，截图可见文本形状光标；此前其他位置可见箭头。**没有做成连续、明确区分本地工具光标叠加与远端绘制的最终证据；不能只据聊天截图声称热点全部通过。**
- [已写回的证据和连接模式纠正](https://github.com/weironz/adit/issues/92#issuecomment-6557417703)。

### 接手后最小验收建议

1. 刷新目标连接和会话；可先用 GNOME 登录界面的文本框做光标切换，不必为光标测试完成一次新登录。
2. 构建并记录 app/helper 的源码提交、构建命令和 SHA-256；helper 必须使用独立 workspace，明确设置 `ADIT_RDP_HOST`，避免拾取根 target 中的陈旧 helper。
3. 在独立临时配置中连接目标，确认画面和输入活性，保存 Adit 可见标题栏截图。不要修改用户真实配置或盲目关闭其他远控服务。
4. 连续记录：背景箭头 -> 文本框 I-beam -> 背景箭头，至少两轮；将服务端 Pointer 消息、光标图像与 GUI 位置对应起来。
5. 验证热点：在已知可点击控件/文本框边界，确认点击目标与光标热点一致。补一个缩放状态以及鼠标离开远端区域后本地光标恢复的检查；若原标准只要求形状绘制，不要无界扩成多平台认证。
6. 区分截图工具添加的本地光标/蓝色点击圈与应用绘制的远端光标；必要时用生产渲染截图 API 或带已知图案的隔离服务端辅助验证。模拟服务端只能证明其覆盖范围，真实 GRD 场景另列。
7. 如果发现双光标、偏移、隐藏后残留等可复现缺陷，先修再验；完成后把精确范围和证据写入 issue 并关闭。用户已授权“通过后关闭”，无需重复求同意。

代码入口（行号可能漂移，先 rg 确认）：

- `D:\codes\adit\crates\adit-rdp\src\session.rs`：`pointer_update`，`HostMsg::Pointer`。
- `D:\codes\adit\crates\adit-rdp-proto\src\lib.rs`：`PointerUpdate`。
- `D:\codes\adit\crates\adit-session\src\rdp_client.rs`：`update_pointer`、`session_rdp_cursor`、`ipc_cursor_stays_separate_from_frame_pixels_and_validates_malformed_bitmap`。
- `D:\codes\adit\crates\adit-ui\src\desktop_panes.rs`：`rdp_cursor_layer`（约1204行）、`rdp_cursor_bounds`、`rdp_cursor_anchor`、`RdpCursorLayer::draw`、`mouse_interaction`（远端形状存在时应 Hidden）。

## 5. 其余 issue 的真实剩余范围

| Issue | 已有工作 | 尚不能关闭的原因 / 下一步 |
| --- | --- | --- |
| [#97 多显示器](https://github.com/weironz/adit/issues/97) | 布局选择、持久化、恢复已合入；真实 GRD 返回 max_monitors=5，观测到单屏拓扑 | 未执行多屏请求、拓扑确认、切换与坐标验收。先调查服务器的虚拟显示器能力；max_monitors 只表示能力，单屏物理输出也不证明不能虚拟多屏。 |
| [#116 DPI/无障碍](https://github.com/weironz/adit/issues/116) | 本地原生 UIA 单窗/双窗生产控件验收均 exit0 | 报告明确 screen_reader_tested=false，双窗 remote_server_tested=false；实际 Narrator、IME、系统 DPI 仍缺。不能把恢复焦点的夹具标志当实际中文输入法。 |
| [#205 VNC 字符](https://github.com/weironz/adit/issues/205) | 多厂商输入、兼容策略及大量历史实测已有 | 最新分阶段 ACK/UTF-8 夹具未在真实 Windows 服务端复跑；旧轮虽文本为空但全生命周期含24次 release，仍为失败。另有 dead-key + Win 组合未决，不可只复跑一项便关闭整体。 |
| [#94 拖放](https://github.com/weironz/adit/issues/94) | Windows 复制后 OLE 拖出切片；远端请求串行化 #228、落点拒绝 #229；真实 CLIPRDR 能力协商 | 文件 offer/实际字节/Explorer 拖出未端到端通过；直接本地文件拖入远端未实现。当前本地文件发送暂时安全禁用。不要把协商、剪贴板复制粘贴说成双向直接拖放。 |
| [#93 跨平台文件剪贴板](https://github.com/weironz/adit/issues/93) | Linux/X11 FUSE 桥接代码合入 | 原生忽略测试、真实互操作未完成；macOS 文件承诺缺实现。三平台机制确实不同。 |
| [#223 ClearCodec](https://github.com/weironz/adit/issues/223) | 缓存边界修复、持久错误、GUI错误优先及400个正常流回放 | 缺原事故输入或同类真实失败流；GUI无响应与codec异常的因果未复现。正常流和合成IPC均不能替代。 |
| [#212 NIC/睡眠](https://github.com/weironz/adit/issues/212) | 断线/半开恢复与 Linux 双接口已有证据 | Windows 真NIC/OS睡眠仍缺精确宿主映射、可信管理链路和独立恢复保障。不要再次盲目禁NIC或睡眠。 |
| [#89 手工交互](https://github.com/weironz/adit/issues/89) | 多项逻辑测试已存在 | 原 issue 包含键盘钩子、SFTP覆盖、exit保活、同步删除阈值、修饰键释放、仅本次信任、错密重连、克隆、排序；需逐项真机记录。不要笼统断言 computer-use 完全不能验收；客观交互可自动做，主观手感仍应如实限定。 |
| [#88 Telnet硬件](https://github.com/weironz/adit/issues/88) | RFC/IAC回环测试 | 需用户授权的真实Cisco/IPMI等设备，重点CR NUL。没有找到连接不等于没有可用设备。 |
| [#87 OneDrive](https://github.com/weironz/adit/issues/87) | 编译期client id及release Secret引用已有 | 2026-11-02只读核查未见 ADIT_SYNC_ONEDRIVE_CLIENT_ID Secret；注册是否批准不明。刷新现状，若已有合法注册，配置后做真实OAuth/同步。不能未经授权新建账户或扩大云访问。 |
| [#90 签名](https://github.com/weironz/adit/issues/90) | 采购/分发需求 | v0.1.77安装包内嵌PE证书表为空的只读检查；macOS未验证。缺签名服务/证书，不是继续写测试能解决。 |
| [#230 字体重影](https://github.com/weironz/adit/issues/230) | 交接时新发现开放 issue，含截图 | 已定位并修复：远端曲面纹理按 fit 系数放大，最近邻在分数放大倍数下**复制**纹素，一根 1 像素笔画在部分位置落到两个屏幕像素，文字即读作重影。修复后整数放大用最近邻、其余用线性重采样。合成器截图按两种与渲染器模型无关的方法验证（帧缓冲颜色集合、被复制的笔画列比例），1:1 对照逐像素不变。证据：`tests/vnc/results/2026-11-06-vnc-glyph-ghosting/`。不证明服务端编码或厂商实现的兼容性。 |

#116 评论：<https://github.com/weironz/adit/issues/116#issuecomment-6557419895>。
#94 评论：<https://github.com/weironz/adit/issues/94#issuecomment-6557419858>。
#205 夹具与失败原始记录：`D:\codes\adit\tests\vnc\results\2026-10-21-local-closeout\unicode\README.md`。

## 6. 工具、验收材料和秘密边界

主要临时工作目录：`D:\codes\adit-accept94`（仓库外，本次交接保留）。

- `src\main.rs` / `target\debug\accept94.exe`：驱动真实 helper 的临时探针。读取 stdio framed IPC，打印 ClipboardFiles / FileContents / Pointer / Tile 等；输出可能含文件名、剪贴板文本，不可原样上传。
- `remote.py`：paramiko 的远端脚本/上传助手，支持 `-c` 和 `--upload`。**历史脚本内含明文登录凭据，而且使用 AutoAddPolicy；不要复制进 Git、评论、共享文档或打印全文。接手应改用现有安全凭据渠道并验证主机身份，不要把此脚本当安全SSH模板。** 本次没有更改该脚本或凭据。
- `run*.out`、`dumps*`：多次真实连接尝试输出和PNG。第一批记录主要是GDM登录界面，目录名不代表模式已被证实。`dumps-user` 也不能证明用户镜像模式。
- `clip*.py`、`s*.sh`、`inject.sh`：失败/不完整的注入实验，不是通过证据。read_async 的 PyGObject 类型/参数问题未彻底解决；不能据注入失败断言 GNOME 焦点安全策略就是根因。
- helper 日志通常在 `%APPDATA%\Adit\rdp-helper.log`，GUI丢弃stderr；日志可能轮次混杂，必须按新PID/时间过滤。不要公开原始按键扫描码或剪贴板内容。

已核实本地文件仍存在，但二进制未嵌入可验证的源码SHA，不能只凭版本号/时间戳证明来源：

```text
D:\codes\adit\target\debug\adit-app.exe
D:\codes\adit\crates\adit-rdp\target\debug\adit-rdp-host.exe
D:\codes\adit-accept94\target\debug\accept94.exe
```

先前隔离配置路径为 `%TEMP%\adit-issue92-a390d076a4794e579c6eebb1adbab11c`，只提取目标profile和对应加密凭据。2026-11-05检查该目录已不存在；其他历史临时目录可能尚在，清理时仅清理确认归本任务所有的文件。

真实用户配置曾定位到 `C:\Dropbox\adit`（包含多份冲突副本），不要覆盖或批量清理。读取profile时只输出必要元数据；不要导出全部凭据。用户曾在聊天提供过另一个账户密码，因登录目的地歧义，没有使用或另存，本文不转录。

## 7. 远端改动与 RustDesk 恢复状态

历史授权实机：`192.168.72.20` / `will-opt7010`，Ubuntu 26.04.1、GNOME Remote Desktop 50.2。该地址在2026-11-05 SSH超时，仅记为本次不可达。

此前为了验收做过的实际改动（并非全部已回滚）：

- 装过 `wl-clipboard`、`xclip`，放置了 `/tmp/accept94*` 脚本、测试文件，以及用户 Desktop 下的 `accept94` 测试目录；有 clipboard owner 常驻进程。清理前识别本任务PID和路径。
- 切换过系统/用户 GRD，设置过用户 RDP enable=true、screen-share-mode=mirror-primary、port=3389、credentials 认证；最后只把用户 service 停止，没有完整恢复所有历史 gsettings。系统 GRD 当时 active。
- 曾停掉 RustDesk 服务及server进程；用户明确要求恢复后，执行 `sudo -n systemctl start rustdesk`，随后 enabled/active，service/server/tray 均出现。后续检查仍active，但服务端绑定GDM用户；不是原will桌面已恢复的证明。
- 2026-11-02发现真实 `will` 图形会话356，Type=wayland、Remote=yes、Service=gdm-password、State=active；seat0则是GDM greeter c22。会话ID现在不可复用。
- 无证据能确认旧控制台会话消失的原因；不要归因于某次RDP连接，也不要承诺原桌面已完整恢复。

RustDesk 与 GRD 可能存在互操作问题，但本线程未确立因果。接手优先保留RustDesk恢复状态，不要为测试再次停它，除非具体诊断确有必要且有恢复步骤。

## 8. CI runner 恢复入口

部署手册（本地，未保证全部仍有效）：
`C:\Users\willz\Documents\New project 2\docs\devops\github\runner\adit-runner-deployment.md`。

| 项目 | Linux | Windows |
| --- | --- | --- |
| 历史主机 | 192.168.72.6 / ubuntu06 | 192.168.72.103 / WIN11-VM |
| runner | home-workstation-linux-adit-01 | home-workstation-windows-adit-01 |
| 根目录 | /home/ubuntu/actions-runners/repos/weironz/adit/linux-01 | C:\actions-runners\repos\weironz\adit\windows-01 |
| 服务 | actions.runner.weironz-adit.home-workstation-linux-adit-01.service | actions.runner.weironz-adit.home-workstation-windows-adit-01 |

历史Windows runner曾自行恢复并完成当前SHA的Windows子任务；没有证据表明本线程修复了其根因。Linux没有可信管理链路恢复记录。

本机VMware曾只列出 `D:\VMs\Win11\Win11.vmx`，不能未经UUID比对就当成目标runner宿主。`vmrun getGuestIPAddress ... -wait` 曾长时间不返回，后来的有界查询报告 VMware Tools 未运行。不要递归扫整个硬盘或盲目启动/重启虚拟机。

恢复顺序：确定主机/UUID/管理入口 -> 检查runner服务与出站网络 -> 只重启对应实例 -> GitHub online -> 真实任务完成。服务active或GitHub online都不单独代表验收通过。保留专属目录、账号和端口分配，不启用托管runner回退、不更改ARM/macOS暂停策略。

`runner-bootstrap.yml` 可检查依赖；不能用 `release.yml` 测试环境。用户没有要求发版，勿触发发布。

## 9. 最小命令清单

以下命令在 `D:\codes\adit` 的 PowerShell 中执行；没有敏感参数：

```powershell
git status --short
git log -1 --oneline
gh issue view 92 --comments
gh api repos/weironz/adit/actions/runners
gh api repos/weironz/adit/actions/runs/41264012037/jobs
gh run list --workflow ci.yml --branch main --limit 3
gh release view v0.1.77

cargo build -p adit-app --locked
cargo build --manifest-path crates/adit-rdp/Cargo.toml --locked
Get-FileHash target/debug/adit-app.exe -Algorithm SHA256
Get-FileHash crates/adit-rdp/target/debug/adit-rdp-host.exe -Algorithm SHA256

just ci
# 独立 workspace，不包含在 cargo test --workspace 中
cargo test --manifest-path crates/adit-rdp/Cargo.toml --locked
```

按改动范围选测试，不要每个状态查询后重复全量构建。主/独立workspace严格Clippy要求见AGENTS.md。

本机Python优先使用：
`C:\Users\willz\.cache\codex-runtimes\codex-primary-runtime\dependencies\python\python.exe`。
系统python/py可能是Store别名。

## 10. 给接手 agent 的启动指令

> 先读本交接和 D:\codes\adit\AGENTS.md，核实当前GitHub/工作区/目标连通性。优先完成 #92 的真实GUI光标形状切换、热点及离开远端区域恢复；固定二进制来源与证据，通过后按用户授权关闭，并准确声明发布状态。随后评估 #97 的真实虚拟多屏能力与 #230 的新缺陷。不要重做已完成修复，不要把工具失焦推断成系统锁屏，不要再次让用户重复“继续”。确实阻塞时报告具体工具错误、已排除项与所需最小条件。保护现有工作区和远端RustDesk，密码不写入交接、Git或日志；发布仍需用户另行要求。

## 11. 初次交接后的增量

- `39315c4` 已推送 main：回收 workbench 普通链接/OSC 8 链接验收样本，浅色验收选择 Solarized Light；IPC 超时测试增加写入尝试记录，在 Windows TCP reset 情况下仍断言没有发送 Connect 或凭据。
- 对该提交的本地 `just ci` 完整通过；native-workbench-acceptance 严格 Clippy 及库测试 353 通过、2 忽略。首次 CI 失败的原有超时测试已保留失败记录，修正后重跑通过。
- 22 个非 main 分支及 6 个 detached HEAD 已审计，既有业务功能均找到 main 对应整合。另发现原生拖放、SVC 分片约束、helper 输出预算三项未完成草稿，用户随后明确要求收尾。
- 本文前面的环境、PID、会话和 runner 状态均为对应日期快照；接手需刷新。草稿收尾最终状态见 [草稿清单](draft-closeout-2026-11-05.md)。