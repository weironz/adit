# Roadmap — done

Shipped work, newest first. Kept rather than deleted because the reason a thing
was built the way it was is the part that gets lost, and because several entries
here are the only record of a failure that cost real time.

For what is *not* built, see [roadmap.md](roadmap.md). For how it works, see
[features.md](features.md) and [architecture.md](architecture.md).

---

> 从 v0.1.66 往下的旧章节仍是英文；v0.1.67 起的新条目跟着
> [roadmap.md](roadmap.md) 用中文。

## v0.1.69

**全局热键透传到全屏远程桌面。** 报上来的现象是截图工具的 `Ctrl+Shift+X` 触发了本地
而不是远端。那不是"转发漏了"：别的程序用 `RegisterHotKey` 注册的热键，Windows 在任何
窗口看到这次按键之前就已经派发掉了，Adit 压根没收到，也就无从转发——任何 RDP 客户端
都转发不了自己没拿到的东西。mstsc 的做法、也是这里的做法，是装一个 `WH_KEYBOARD_LL`
钩子：它跑在热键派发之前，把键吞下来，再以扫描码发给远端。

一个全局钩子的回调会对整台机器的每一次按键运行，吞错一个键，坏掉的观感是整个桌面而
不只是这一个窗口。三条约束都是承重的：**只在 RDP 全屏时挂**（`RdpTick` 也会摘，因为
会话可能不经过"退出全屏"就结束——掉线、关标签——而一个为已经消失的桌面留着的钩子会
继续吃热键却无处可发）；**回调只读一个 atomic、往 channel 塞一条记录**，因为 Windows
给低级钩子设了截止时间，超时的钩子会被系统*静默移除*，慢回调不会报错，只会停止工作，
所以真正的发送放在 UI 线程上；**`Ctrl+Alt+Enter` 永不吞掉**，否则一个没找到工具栏的
用户在全屏里只能靠杀进程出来。

三个小一点的：注入的按键跳过，否则别的工具用 `SendInput` 发的键会绕回来成环；
Ctrl/Alt 是否按下问系统而不是自己记，因为摘钩期间会漏掉抬起事件，一个被认为卡住的
修饰键会永久吞掉那条退路；摘钩时丢弃吞下但尚未发出的按键，它们属于一个已经不在该
模式下的会话。

**没有在真机上验证过。** 值得测的是非正常路径：`Ctrl+Alt+Enter` 是否仍能退出全屏，
退出后本地热键是否恢复，以及全屏会话被直接关闭或掉线（而不是正常退出全屏）之后是否
恢复。

**剪贴板图片（`CF_DIB`）双向。** 之前不是 bug——图片只是唯一还没实现的剪贴板格式：
文本通了，文件通了，而截图两者都不是，磁盘上没有对应文件，字节也不是字符串。轮询靠
`GetClipboardSequenceNumber` 而不是比较内容：一张全屏截图几十兆，轮询每秒两次，比内容
等于每次都把整张图从剪贴板拷出来，只为了发现它没变。

**会话与标签多选。** 在一个分支上由另一个 agent 完成，审核后修复再合并。修法是结构性
的而不是十几处补丁：让选择状态只有一个来源——"当前选中的那一个"就是只含一项的选中
集合——于是"单选"和"多选"不再是两条会各自漂移的代码路径。没有单独的"批量"动作：选中
多个之后，默认动作本身就是批量的。

**全屏真的全屏，黑边修掉。** 顶部菜单栏在全屏时仍占着高度，是因为可视区域的计算还在
减去它；把那个布尔量换成一个明确的"chrome 高度"（RDP 全屏时为 0）之后，只剩一个地方
决定这件事。⛶ 的提示文字现在跟着状态走，而不是恒为"退出全屏"。

**发版默认只出 Windows x64。** arm64、Linux、macOS 收进一个 `all_platforms` 开关。做成
开关而不是注释掉代码，是因为"重新打开"于是变成发版时的一次选择，而不是一次得有人记得
回滚的提交；而且跳过了什么会记在那次 run 自己的 inputs 里，事后能分辨"没构建"和
"构建了但失败了"。

## v0.1.68

**RDP 悬浮工具栏。** 参照 RustDesk：全屏时顶部中央一条可收起的浮动工具栏，带画质预设
（画质档只在连接时的 Client Info PDU 里生效，所以切换会重连）。

第一版一直在闪。原因是 iced 0.14 的 `stack`：光标只交给最上层，同时会给下层报一个
"指针已离开"——于是"鼠标在工具栏上"和"鼠标离开了触发区"两个判断互相驱动，形成振荡。
这个会话里两个独立的闪烁 bug，最后都是这一条。收起后的 ⌄ 小标签改为悬停才浮现。

**RDP 剪贴板文件传输，双向，含二进制文件。** helper 侧是 MS-RDPECLIP 的
`FileGroupDescriptorW` / `FileContents`；本地侧是 Windows COM 的延迟渲染
（`IDataObject` / `IEnumFORMATETC` / `IStream`，用 `windows::core::implement` 生成
vtable），`OleSetClipboard` 跑在一个带消息泵的专用 STA 线程上。

两个代价高的细节记在这里：发布线程 id 之前必须先用 `PeekMessageW` 把消息队列逼出来，
否则第一次 offer 会被静默丢弃；以及读取失败必须返回错误 HRESULT，绝不能返回零字节——
后者会让对面拿到一个"成功传输的空文件"。

**缩放与黑闪的一系列修复，以及移除"缩放适应窗口"。** 整屏黑闪的原因是每一个 resize
事件都触发一次分辨率重协商，新分辨率的贴图重传和 iced 的异步图片上传 worker 撞在一
起；解法是防抖，加上落地瞬间用旧贴图遮挡。"缩放适应窗口"最后整个删掉了——它解决的问题
没人认得出来，它自己带来的黑边却是看得见的。

**settings 写失败之后就再也不保存了。** `persist_settings_if_changed` 在写失败时仍然
推进了它的比较基准，于是之后每一次都被判为"没有变化"。一个设置文件因此停在三周前的
状态，并直接导致一次错误的排查结论——我读了那个文件，据此"排除"了一个实际上开着的
开关。

## v0.1.67

**会话编辑对话框改版。** 加宽、分区、可键入；分组改成从已有分组里选，而不是把名字重新
敲一遍。同批修掉了若干"点进去出不来"的死角。

## v0.1.66

**Telnet.** A first-class protocol beside SSH / SFTP / RDP. IAC negotiation
answers ECHO, SUPPRESS-GO-AHEAD, TERMINAL-TYPE and NAWS, and refuses everything
else *explicitly* — an unanswered option leaves some servers waiting forever.
It replies only on a real state change, which is the guard that stops two
implementations acknowledging each other in a loop. Not yet verified against
real hardware (see [roadmap.md](roadmap.md)).

**GitHub Gist over OAuth device flow.** Was a pasted personal access token.
Device flow is the one GitHub flow needing no client secret and no loopback
listener. Worth recording *why*, because the obvious reason is wrong: GitHub
**does** support PKCE (added July 2025), but it does not distinguish public from
confidential clients, so the web flow still demands a client secret and PKCE
buys a desktop app nothing here. Pasting a token still works, for networks where
a browser round trip does not.

**Protocol pills.** The tile says what kind of machine a host is; the pill says
how you reach it. Two separate questions, and the same Ubuntu box answers both —
a wall of identical orange cards could not tell SSH from RDP. Telnet's pill is
red: it is the one protocol here with no encryption, and the badge was already
being drawn.

**Real logos.** Ubuntu, Windows, Red Hat, Alpine and four generic marks as
compiled-in SVG, replacing geometric glyphs. A new profile takes an icon from
its protocol — RDP means Windows, everything else starts at Ubuntu — because a
field nothing ever filled meant the logos were invisible by default.

**Group icons.** A `group_icons` map beside the group list rather than turning
`groups` into structs: the list carries order and membership that thirty-odd
call sites read as plain names. No format migration, on a file whose last
rewrite lost a whole catalog. Three places would have silently dropped the
icons — the merge, the save path, and `SyncFinished` — each looking like the
feature never worked rather than like a bug.

**SFTP as a protocol.** Reaching a host's files no longer means opening a shell
you did not want. It dials its own connection, which is the point and also its
one caveat: MFA hosts still need the ride-along path.

**SFTP shell: tab completion and history.** Completes commands, then local or
remote paths depending on which argument of which command. Only the prefix every
candidate shares is inserted; where that adds nothing the candidates are listed,
because a Tab that appears to do nothing is the worst of the three outcomes.
Doing this first required fixing the arrow keys, which were being typed into the
line as `[A`.

**RDP dirty rectangles.** Was one rect grown around everything that changed, so
a blinking cursor and a clock in opposite corners shipped most of the screen.
Now up to sixteen, merging only when merging is *free* — overlapping or tiling
edge to edge — which is what lets a full repaint's hundreds of adjacent tiles
collapse back to roughly one rect while unrelated activity stays apart.

**Reconnect stops asking for a password** it already had in the credential
store.

---

## v0.1.65

**Cloud sync.** Six providers behind one trait: GitHub Gist, WebDAV,
S3-compatible, Google Drive, OneDrive, Dropbox. Per-session three-way merge
keyed by UUID against a stored ancestor. Design and provider quirks in
[cloud-sync.md](cloud-sync.md).

Two of its rules exist because breaking them destroyed a real 152-session
catalog, and both are load-bearing:

- **The ancestor only advances to a state read back and confirmed.** Without it
  a lost race deletes a session on the *recovery*, not on the race.
- **An absent remote document is not a deletion.** Deleting the file from a
  provider's web UI — a reasonable way to say "start over" — was read as "the
  other machine deleted all 152 sessions", and that deletion propagated home.
  Behind it sits a brake: a sync whose result would empty a populated machine is
  refused outright.

**One settings page.** 应用 / 外观 / 终端 / 日志 / 同步与云 behind a category
rail, replacing three separate dialogs that meant three places to look for one
setting. Every setting is its own card.

**English translation.** Lookup keyed by the Chinese source string, falling back
to it — every string here was written in Chinese first, so English is a
translation of it, and a missing entry shows the original rather than a bare
key. The language lives in an atomic rather than on `AditApp`, because the
alternative is threading it through every widget helper in the crate for a value
that never differs between them.

**Toolbar removed**, and the tab strip hidden when idle. Every toolbar button
was already a menu item.

---

## Earlier — the RDP campaign

**H.264 for RDP** — AVC420, then AVC444 written from scratch (two AVC420 views
recombined into persistent per-surface YUV 4:4:4 planes). The wire is Annex-B,
not the length-prefixed NALs the library's documentation describes.

**The RDP scroll-ghosting hunt.** Root cause was the tile-application rule:
fresh passes (TILE_FIRST/TILE_SIMPLE) must blit whole, upgrade passes must clip
to the region rects. FreeRDP clips both, which is provably wrong for fresh
passes on a Windows stream. Proven from captured bytes rather than reasoned
about — see [rdp-debugging.md](rdp-debugging.md), which exists because two
plausible theories were refuted by the capture before the real one was found.

**RemoteFX Progressive decoding**, without which a healthy session renders a
solid black desktop — a symptom that looks like a connection bug and is not.

**Auto-login and fullscreen for RDP.** Microsoft-account hosts need the local
SAM name for interactive logon even though NLA accepts the email alias.

---

## v0.1.31 – v0.1.36 — the phase-2 batch

Six items planned together in July 2026, each researched against OpenSSH, PuTTY,
SecureCRT/Xshell, Termius and WezTerm before any code was written.

**Jump hosts (`ProxyJump`), v0.1.31.** Hop 0 dials its own TCP; every further hop
opens `direct-tcpip` through the previous one and runs a *fresh handshake* over
that channel — which is what makes every host key on the chain, bastions
included, verified through the tunnel rather than assumed. All intermediate
handles are kept alive for the session lifetime: drop one and its channel dies
under everything stacked above it. SFTP and tunnels ride the same final session,
because a second direct dial cannot reach a non-routable target. Hops reuse the
profile's one credential (see [roadmap.md](roadmap.md)).

**Interactive MFA, v0.1.32.** Keyboard-interactive used to be a password fallback
that answered *every* server prompt with the saved password via a keyword
heuristic. The driver now auto-fills only account-password fields — masked or
labelled as such, excluding second factors and new-password prompts — and
surfaces anything else as a dialog, across as many rounds as the server runs.
Auth happens before the main channel loop exists, so the command channel is
pumped in a `tokio::select!` to let answers and disconnects arrive mid-handshake.
Cancelling aborts the whole connect with `AuthenticationCancelled` rather than
falling through to key or agent auth.

**Key passphrase in its own field, v0.1.33; PuTTY `.ppk`, v0.1.36.** The
passphrase used to be the login-password field doing double duty, and a key that
failed to load returned `Ok(false)` — a silent fallthrough that read as "wrong
password". It now raises `KeyPassphraseRequired` / `KeyPassphraseWrong` to the
UI, while the opportunistic `~/.ssh` default-key scan still skips quietly.
`.ppk` then needed no parser at all: russh 0.61 enables `ssh-key`'s `ppk`
feature, and the same `decode_secret_key` call routes `PuTTY-User-Key-File-*`
content to it — v2 (SHA-1 MAC) and v3 (Argon2id) — so the passphrase already
threaded through in v0.1.33 decrypts them.

**Per-session appearance, v0.1.34.** A profile carries an environment
(None/Dev/Staging/Prod/Custom), an accent colour and a badge label, all
`#[serde(default)]` so old files load unchanged; the tab shows the badge. The
enum rather than a bare colour picker is the point — red/amber/green presets are
mistake-proof, with Custom as the escape hatch — because the whole feature exists
to prevent running something on the wrong server.

**OSC 8 hyperlinks, v0.1.35.** The link id lives on the cell but *off* the SGR
pen, so a mid-link SGR reset doesn't drop the link; it resets on RIS and across
the alternate screen. Opening is Ctrl/Cmd+click, so a plain click still selects
and mouse reporting still passes through, and it goes through a confirmation
showing the real destination. The allowlist is `http(s)` and printable ASCII
only, which is what rejects `file:`/`javascript:` and the Unicode bidi/format
characters that could make the shown URL lie; opening is shell-free (`rundll32
url.dll,FileProtocolHandler` with the URL as a single argv).

**Integration tests against a real `sshd`.** A Docker-backed suite
(`crates/adit-ssh/tests/integration.rs`, feature `integration`) driving the
`spawn_*` API, plus a Linux CI job; the default Windows `cargo test --workspace`
needs no Docker. In-process russh tests validate the protocol, not interop with
the OpenSSH `sshd` people actually connect to — banner/kex negotiation, PAM,
`AllowTcpForwarding` semantics. The cautionary precedent is WezTerm's harness,
whose pubkey auth hung **only** inside GitHub Actions.

---

## Before that — the phase A–E backlog

The first native push, worked through as a phased plan. Dates are vaguer here:
a few of these (snippets, the sixth split pane, the non-Windows packages)
landed much later than the rest, and the plan they came from recorded ✅ marks
rather than reasons, so several entries below are a statement of fact and
nothing more.

**Host-key verification.** An unknown key can pause the handshake and show its
SHA256 fingerprint for accept/reject before being recorded; a **changed** key
always prompts, shows the previously stored fingerprint beside the new one, and
names the MITM risk. `known_hosts` stays OpenSSH-format, including hashed
(`|1|…`) entries, wildcard and negated patterns, and the `@revoked` /
`@cert-authority` markers — a tolerant parser is what stops an import silently
dropping a security marker. Entries can be listed and deleted from the UI, which
returns that host to first-use. New keys are trusted silently by default; that
default, and how it departs from the plan, is
[decisions.md #18](decisions.md#18-new-host-keys-are-trusted-silently-by-default--reversal-of-the-phase-2-plan).

**Keepalive and auto-reconnect.** A 30 s keepalive dropping after 3 missed
replies, which also removed an earlier 20 s inactivity timeout that had been
killing idle sessions outright. Reconnect backs off 1→30 s over at most 10
attempts and arms **only** for a session that actually reached "connected" and
was not disconnected on purpose — otherwise a wrong password or an unreachable
host would loop forever.

**Scrollback search** (Ctrl+Shift+F), with wraparound, auto-scroll to the current
hit, and an `n/total` counter.

**Fonts and colour schemes.** Six monospace presets, a 9–28 px size stepper, and
seven full 16-colour ANSI palettes with a live preview. The size is not cosmetic:
the whole cell grid rescales from it, so render, hit-testing and column fitting
all derive from one number.

**Mouse reporting passthrough, bracketed paste, and paste safety.** Mouse events
reach `vim`/`tmux`/`htop` instead of selecting text; a multi-line paste asks for
confirmation, skipped when the remote app is already in bracketed-paste mode.

**Broadcast input.** Keystrokes fan out to every connected session, with an
always-visible badge showing the reach count — the badge is the feature's safety
mechanism, because the failure mode is leaving it on without noticing.

**Keyword highlighting.** Colour output locally by pattern, the way SecureCRT
does, for text the server sent uncoloured. The whole design follows from one
rule: **never repaint a cell the server coloured**, since server colour carries
classification we cannot reconstruct (blue is a directory, green and red are the
two sides of a diff). `Color::Default` is exactly "the pen was at SGR 39 here",
so the existing model already had the test. It lives in the UI beside scrollback
search rather than in the terminal core, which keeps the grid a faithful record
of what the server sent. Two of the design's own conclusions turned out to be
wrong and are recorded rather than edited away, in
[keyword-highlighting.md](keyword-highlighting.md).

**SFTP dual-pane file manager.** Local and remote panes, sortable columns,
multi-select, pane-to-pane drag, drag-from-Explorer upload, recursive directory
transfers, cancellable transfers, and a queue with progress and speed.

**Port forwarding.** All three OpenSSH forms — local (`-L`), remote (`-R`) and
dynamic SOCKS5 (`-D`) — saveable per profile and auto-started on connect.

**Snippets, tab rename, and command-bar history.** Saved commands sent to the
active session in one click; tabs renamed in place; the command bar steps back
through what was typed.

**Split panes.** Up to six sessions tiled at once, each with its own PTY size and
its own hit-testing, the focused pane driving keyboard input and the status bar.

**Session logging.** A configurable folder and filename pattern (`%N %H %Y %M %D
%h %m %s`) with a live preview, auto-start on connect, and a plaintext mode that
strips escape sequences.

**Imports.** `~/.ssh/config` (`Host`/`HostName`/`User`/`Port`/`IdentityFile`,
multiple aliases per block) and a SecureCRT session tree, whose folder structure
becomes groups and whose port fields are hex DWORDs.

**Packaging beyond the Windows x64 installer.** Windows-on-ARM, `.deb` and `.rpm`
for x86_64 and aarch64, and an unsigned `.dmg` per macOS architecture, all built
and attached by the release workflow — plus an in-app updater that compares
semver against GitHub releases and launches the installer. All of it unsigned
(see [roadmap.md](roadmap.md)).
