# macOS RDP 剪贴板文件承诺（#93）

## 环境和边界

2026-11-25，macOS 26.6.2（25G83），Apple Silicon arm64，Xcode 26.6，Rust 1.98.0。从 `origin/main` 的 `6adca05328cffbb722dc8219b2db13c49e228fa0` 开始。详见 [环境输出](environment.txt)。

第一轮用**真实 Adit GUI 和访达**，但用 [fixture.rs](fixture.rs) 替代了 RDP helper。夹具走生产 GUI 的带版本 IPC：发送远端文件描述，等 GUI 真正发出 `RequestFileContents` 后才返回对应范围。它不连接 RDP 服务端，也不实现 CLIPRDR 网络协议。

追加验收用同一版 Adit GUI 和**未经修改的生产 helper**，连接 QEMU 中 Debian 13 arm64 的 **GNOME Remote Desktop 48.1**。在远端文件管理器复制，经真实 RDP/CLIPRDR 传输，最后拖入本地访达。两轮验收的证据分开保存。

发布包仍不附带 macOS RDP helper。当前源码在非 Windows 上会查找无 `.exe` 后缀的 `adit-rdp-host`；本机可以单独构建 helper，开发运行可以用 `ADIT_RDP_HOST` 指定路径。冒烟测试让真实 helper 连接本机 TCP 测试监听器，监听器接受后立即断开；它只验证了进程、版本握手、TCP 连接和错误回报，不是成功的 RDP 登录。

## 每项结果

| 项目 | 结果 | 证据 |
|---|---|---|
| 发布 macOS 文件承诺 | 已验证。通用粘贴板包含 AppKit 文件承诺类型，打开拖出窗口没有请求文件数据。 | [类型](pasteboard-types.txt)、[拖放前日志](before-drop.txt) |
| 访达 ⌘V | 不支持。在该系统上使用通用粘贴板的文件承诺时，访达的「Paste」为灰色；按 ⌘V 后目录仍为空，未请求数据。不要将此项视为已实现。 | [截图](finder-paste-disabled.png)、[拖放前日志](before-drop.txt) |
| 夹具 IPC：访达拖放和按需读取 | 已验证。用鼠标从 Adit 的「远端文件拖出」窗口拖到真实访达；接收方获得一个目录。仅在拖放后出现 17 个请求（16 个 65,536 字节和最后 37 字节），均为原始索引 0、代次 1。1,048,613 字节的 `pattern.bin` 比对一致；`empty.txt` 为 0 字节且未发请求。 | [拖放截图](finder-drop.png)、[目录内容](finder-files.png)、[拖放后日志](after-drop.txt)、[SHA-256 与 cmp](checksums.txt) |
| 失败及安全策略 | 自动化已验证：不安全路径与重复名称拒绝；代次失效、空响应、超长响应不会提交部分文件，已存在文件不会被覆盖。工作区原有测试验证本地文件元数据和内容不能发送到 RDP；未对真实 RDP 服务端测试反向复制。 | [工作区测试](test.txt) |
| macOS 构建、Clippy、测试 | 通过：`cargo build`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`。依赖 `block 0.1.6` 仍有 Cargo 的 future-incompatibility 提示。 | [构建](build.txt)、[Clippy](clippy.txt)、[测试](test.txt) |
| 真实 helper 单独构建、Clippy、测试、启动 | 通过：`cargo build --release --manifest-path crates/adit-rdp/Cargo.toml`，以及同一 manifest 的 `cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`；另通过实进程 IPC/TCP 冒烟测试。 | [构建](helper-build.txt)、[Clippy](helper-clippy.txt)、[测试](helper-test.txt)、[启动](helper-smoke.txt) |
| Windows x64 (MSVC) 交叉检查 | 通过：用 `cargo-xwin 0.24.1`、MSVC CRT/SDK 和 LLVM 23.1.6，完成 `adit-ui --all-targets` 检查及工作区全目标 Clippy（`-D warnings`）。未在 Windows 上链接或运行。 | [检查](cross-windows-all-targets.txt)、[Clippy](cross-windows-clippy.txt)、[初次缺少目标头文件的日志](cross-windows.txt) |
| Linux x64 (GNU) 交叉检查 | 通过：用 messense 的 GCC 15.2.0 完成 `adit-ui --all-targets` 检查及工作区全目标 Clippy（`-D warnings`）。检查未被 GUI 库阻塞；未在 Linux 上链接或运行。 | [检查](cross-linux-all-targets.txt)、[Clippy](cross-linux-clippy.txt)、[初次缺少交叉编译器的日志](cross-linux.txt) |
| 真实 RDP：远端复制 → 访达拖放 | 已验证 GNOME Remote Desktop 48.1。正常 NLA 登录后，CLIPRDR 协商得到文件流和剪贴板锁。远端 Nautilus 复制文件后，服务端先发送文件描述，拖放前收到 0 个内容请求；拖放后收到 17 个带 `clipDataId=1` 的请求。访达中的 1,048,613 字节与远端源文件逐字节一致。 | [环境](real-environment.txt)、[远端文件](real-remote-copy.png)、[拖放前](real-before-drop.png)、[拖放后](real-finder-drop.png)、[服务端前置日志](real-grd-before-drop.txt)、[服务端拖放后日志](real-grd-after-drop.txt)、[helper CLIPRDR 日志节选](real-helper-cliprdr.txt)、[SHA-256 与 cmp](real-checksums.txt) |

此项只包含远端到本地。共享 IPC 仍沿用已有的文件大小字段（缺省长度与显式零字节在该字段中不可区分），不修改协议或 #94 的跟踪范围。真实服务端只测试了 GNOME Remote Desktop 的单一普通文件，未测试真实 Windows/XRDP、远端目录或反向发送；目录和零字节文件由 IPC 夹具覆盖。真实服务端验收只测拖放，未重新测试访达 ⌘V。

## 复现真实 RDP 验收

1. 在本机 QEMU 中启动 Debian 13 arm64 guest，本机不提供硬件虚拟化，使用 TCG。将 guest 的 3389 转发到 Mac 的 `127.0.0.1:13389`，不要对外开放端口。
2. 在 guest 安装 `gnome-remote-desktop mutter nautilus wireplumber dbus-user-session libgl1-mesa-dri mesa-vulkan-drivers`。在具备 sudo 权限的专用测试用户下运行 [grd-fixture.sh](grd-fixture.sh)，测试密码仅通过 stdin 输入。连接后，在同一用户的 systemd 会话启动远端文件管理器：

   ```sh
   systemd-run --user --unit=adit-nautilus --setenv=GDK_BACKEND=wayland \
     --setenv=WAYLAND_DISPLAY=wayland-0 /usr/bin/nautilus "$HOME/rdp-file-fixture"
   ```

3. Mac 上用 `cargo build --release --manifest-path crates/adit-rdp/Cargo.toml` 生成 helper，用 `ADIT_RDP_HOST` 指定路径。在 Adit 建立到 `127.0.0.1:13389` 的 RDP 会话，不保存密码。在远端 Nautilus 选择 `remote-pattern.bin`，按 Ctrl+C，打开 Adit 工具栏的「远端文件拖出（只复制）」，用鼠标将文件拖入空的访达目录。
4. 拖放后通过 SCP 取回 guest 原文件作为独立对照，运行 `shasum -a 256` 和 `cmp`。用 guest 的 `journalctl --user -u gnome-remote-desktop-headless` 比较拖放前后内容请求。helper 日志用 `RUST_LOG=warn,adit_rdp=debug,ironrdp_connector=info,ironrdp_cliprdr=debug` 采集。

## PR #257 审查修正后复测

用同一 Debian 13 / GNOME Remote Desktop 环境和未改动的生产 helper，运行修正后的 Adit（`02b34ba`）：

- 第一次复制 `remote-pattern.bin` 并打开拖出窗口，再次复制时旧拖出窗口自动关闭。[再次复制前](review-before-recopy.png)、[再次复制后](review-after-recopy.png)。
- 另选字节相同的 `replacement.bin`，在 Adit 主窗口复制并拖放到真实访达。拖放前 0 个内容请求，拖放后 17 个带剪贴板锁的内容请求；1,048,613 字节的 SHA-256 和 `cmp` 均一致。按 helper 日志中的第一个请求和最后一个响应计，17 个请求用时约 0.125 秒（上次同大小文件约 1.566 秒）；这不是大文件或广域网基准。[拖放前](review-before-drop.png)、[拖放后](review-finder-drop.png)、[helper 日志节选](review-helper-cliprdr.txt)、[服务端前置日志](review-grd-before-drop.txt)、[服务端拖放后日志](review-grd-after-drop.txt)、[字节验证](review-checksums.txt)。
- 分离窗口的文件承诺状态迁移及 helper 回包绕过 UI 定时器由本次新增自动化测试覆盖。真实访达拖放在主窗口完成；分离窗口的标题栏并未新增拖出按钮。
- [macOS 严格 Clippy](review-native.txt)、[Linux x64 GNU 交叉严格 Clippy](review-linux.txt)、[Windows x64 MSVC 交叉严格 Clippy](review-win.txt)、[macOS 工作区测试](review-tests.txt)（884 通过、0 失败、21 忽略）、[macOS GUI 构建](review-build.txt)。

## 复现访达验收

从仓库根目录执行，`work` 应为新建的本地测试目录：

```sh
cargo build
evidence=tests/rdp/results/2026-11-25-macos-clipboard-file-promise
work="$HOME/adit-mac-promise-test"
mkdir -p "$work/destination"
rustc --edition 2021 "$evidence/fixture.rs" -L dependency=target/debug/deps \
  --extern adit_domain=target/debug/libadit_domain.rlib \
  --extern adit_storage=target/debug/libadit_storage.rlib \
  --extern adit_rdp_proto=target/debug/libadit_rdp_proto.rlib \
  -o "$work/fixture"
"$work/fixture" --seed "$work/config"
"$work/fixture" --fixture "$work/reference.bin"
ADIT_CONFIG_DIR="$work/config" ADIT_RDP_HOST="$work/fixture" \
  target/debug/adit-app > "$work/gui.txt" 2>&1
```

1. 在 Adit 连接 `Synthetic macOS RDP`，取消保存密码选项后，输入任意虚构占位内容（夹具不校验密码）。打开会话顶部工具栏，点击「远端文件拖出（只复制）」。
2. 用访达打开 `$work/destination`。按 ⌘V，确认目录仍为空、日志没有 `REQUEST`。从 Adit 拖出窗口的文字区域拖到该访达目录。
3. 确认访达出现 `RDP-Promise-Fixture`。检查 `gui.txt` 的范围请求，并运行：

```sh
actual="$work/destination/RDP-Promise-Fixture"
shasum -a 256 "$work/reference.bin" "$actual/pattern.bin" "$actual/empty.txt"
cmp "$work/reference.bin" "$actual/pattern.bin"
test -f "$actual/empty.txt" && ! test -s "$actual/empty.txt"
```

真实 helper 的独立冒烟命令为：

```sh
ADIT_RDP_IPC_TEST_HELPER="$PWD/crates/adit-rdp/target/release/adit-rdp-host" \
  cargo test -p adit-session real_helper_and_production_bridge_exchange_a_synthetic_local_session -- --ignored
```
