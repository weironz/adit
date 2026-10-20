# #223 ClearCodec 实际采集与严格回放（2026-10-21）

本目录归档一次已授权、隔离 APPDATA 的 Windows VM 控制连接取证。共收到 **400 个真实 ClearCodec 流**，同一 helper PID 25860，索引从 0 到 399 连续；sidecar 最终均为 `ok`。严格离线回放通过，0 pending、0 live decode errors、0 offline decode failures，回放命令退出 0。

**本次没有覆盖原始 panic 或同类旧步长越界的真实流。** 元数据中 glyph store 83 次、hit 36 次，36 次均成功；shape / aspect ratio change 均为 0，`legacy_stride_oob_successes=0`。因此本次只证明这 400 个实际流在新 helper 上完成采集及严格回放，不能称为 2026-10-20 原始 panic 包的回放，也不能把构造的 2×2→1×4 回归算成实测复现。

## 证据与边界

- `capture-manifest.json`：400 个私有原始 bin 的 SHA-256、长度、sidecar SHA-256、geometry、surface ID、helper PID、最终 outcome；不包含任何 codec 字节或原始 sidecar 文本。
- `replay.txt`：真实严格回放日志，仅将仓库和私有目录路径替换为占位符。保留真实统计及测试结果，没有合成运行输出。
- `helper-unit-tests.txt`：真实 helper `cargo test --lib` 日志，33 passed / 0 failed / 0 ignored；保留真实 linker warning。不能据此宣称本条日志证明 Clippy 无警告。
- `control-provenance.json`：allowlist 形式的 driver/helper 路径占位符、PID、可执行文件 SHA-256、退出码及日志行数。原始 control stdout/stderr 未复制。
- `source/` 与 `source-manifest.json`：3 个工作区源文件快照及 SHA-256。它们表示归档时源码；单独的源码 SHA 不构成可重复构建或已发布版本的证明。
- `summary.json`：机器可读结果。父任务另报告 all-targets Clippy、4 个 manifest tests、helper build 通过，但没有独立日志；本目录不为这些检查伪造日志。

**控制 driver 退出 0，但其 stderr 记录 helper 结束 code 1。** 私有 helper log 中未匹配到 ClearCodec decode failure 或 panic；这不能解释 code 1 的原因，也不能将此次 helper 结束记作正常退出。显式解码错误→受控 Error/Closed→GUI 保留错误状态的构造回归与此次正常解码采集是不同证据。

原始 codec 数据、PNG、helper 原始日志与 control stdout/stderr 均保留在私有测试目录，未进入仓库。本目录不包含凭据、屏幕像素或远程桌面明文。

## 离线复核

在仍持有私有原始流的同一受控环境，为测试子进程设置 `ADIT_CLEAR_DIR` 为私有 `Adit` 目录、`ADIT_CLEAR_SURFACE=1280x800`，运行：

```text
cargo test --manifest-path crates/adit-rdp/Cargo.toml --test clearcodec_dump captured_streams_replay -- --ignored --nocapture
```

默认拒绝 pending；本次未设置 `ADIT_CLEAR_ALLOW_PENDING`。每次新连接必须使用新的独立 APPDATA，以保留从 0 开始的完整缓存顺序，避免不同连接混入。
