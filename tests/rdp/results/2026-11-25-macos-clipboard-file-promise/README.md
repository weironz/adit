# macOS RDP 剪贴板文件承诺（#93）

## 环境和边界

2026-11-25，macOS 26.6.2（25G83），Apple Silicon arm64，Xcode 26.6，Rust 1.98.0。从 `origin/main` 的 `6adca05328cffbb722dc8219b2db13c49e228fa0` 开始。详见 [环境输出](environment.txt)。

测试用的是**真实 Adit GUI 和访达**，但用 [fixture.rs](fixture.rs) 替代了 RDP helper 的服务端数据来源。夹具走生产 GUI 的带版本 IPC：发送远端文件描述，等 GUI 真正发出 `RequestFileContents` 后才返回对应范围。它不连接 RDP 服务端，也不实现 CLIPRDR 网络协议。真实 helper 的构建及 TCP/IPC 冒烟测试另做，**没有真实服务端的文件传输端到端结果**。

发布包仍不附带 macOS RDP helper。当前源码在非 Windows 上会查找无 `.exe` 后缀的 `adit-rdp-host`；本机可以单独构建 helper，开发运行可以用 `ADIT_RDP_HOST` 指定路径。冒烟测试让真实 helper 连接本机 TCP 测试监听器，监听器接受后立即断开；它只验证了进程、版本握手、TCP 连接和错误回报，不是成功的 RDP 登录。

## 每项结果

| 项目 | 结果 | 证据 |
|---|---|---|
| 发布 macOS 文件承诺 | 已验证。通用粘贴板包含 AppKit 文件承诺类型，打开拖出窗口没有请求文件数据。 | [类型](pasteboard-types.txt)、[拖放前日志](before-drop.txt) |
| 访达 ⌘V | 不支持。在该系统上使用通用粘贴板的文件承诺时，访达的「Paste」为灰色；按 ⌘V 后目录仍为空，未请求数据。不要将此项视为已实现。 | [截图](finder-paste-disabled.png)、[拖放前日志](before-drop.txt) |
| 访达拖放和按需读取 | 已验证。用鼠标从 Adit 的「远端文件拖出」窗口拖到真实访达；接收方获得一个目录。仅在拖放后出现 17 个请求（16 个 65,536 字节和最后 37 字节），均为原始索引 0、代次 1。1,048,613 字节的 `pattern.bin` 比对一致；`empty.txt` 为 0 字节且未发请求。 | [拖放截图](finder-drop.png)、[目录内容](finder-files.png)、[拖放后日志](after-drop.txt)、[SHA-256 与 cmp](checksums.txt) |
| 失败及安全策略 | 自动化已验证：不安全路径与重复名称拒绝；代次失效、空响应、超长响应不会提交部分文件，已存在文件不会被覆盖。工作区原有测试验证本地文件元数据和内容不能发送到 RDP；未对真实 RDP 服务端测试反向复制。 | [工作区测试](test.txt) |
| macOS 构建、Clippy、测试 | 通过：`cargo build`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`。依赖 `block 0.1.6` 仍有 Cargo 的 future-incompatibility 提示。 | [构建](build.txt)、[Clippy](clippy.txt)、[测试](test.txt) |
| 真实 helper 单独构建、Clippy、测试、启动 | 通过：`cargo build --release --manifest-path crates/adit-rdp/Cargo.toml`，以及同一 manifest 的 `cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`；另通过实进程 IPC/TCP 冒烟测试。 | [构建](helper-build.txt)、[Clippy](helper-clippy.txt)、[测试](helper-test.txt)、[启动](helper-smoke.txt) |
| Windows / Linux 编译 | 未完成。由 macOS 分别交叉检查 `cargo check -p adit-ui --target x86_64-pc-windows-msvc` 和 `--target x86_64-unknown-linux-gnu`；Windows 缺少 C 目标头文件，Linux 缺少 `x86_64-linux-gnu-gcc`，均在依赖 `ring` 处中止。审查了 cfg：原生依赖和 AppKit 实现仅在 macOS 构建；未运行 Windows/Linux CI 或原生回归。 | [Windows 日志](cross-windows.txt)、[Linux 日志](cross-linux.txt) |
| 真实 RDP 服务端的文件互操作 | 未验证。本机没有可用的真实 RDP 服务端；不将夹具数据等同于远端 Windows/XRDP 的 CLIPRDR 流量。 | 上述测试边界 |

此项只包含远端到本地。共享 IPC 仍沿用已有的文件大小字段（缺省长度与显式零字节在该字段中不可区分），不修改协议或 #94 的跟踪范围。

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
