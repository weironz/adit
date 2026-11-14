# 自托管 x64 CI

正式检查由 `.github/workflows/ci.yml` 执行，只调度两个仓库专用 runner：

| 平台 | Runner | 必需标签 |
| --- | --- | --- |
| Windows x64 | `home-workstation-windows-adit-01` | `self-hosted, Windows, X64, home-workstation, repo-adit` |
| Linux x64 | `home-workstation-linux-adit-01` | `self-hosted, Linux, X64, home-workstation, repo-adit` |

ARM64 与 macOS 暂停，不回退到 GitHub 托管机器。

### 当前环境的 Linux runner 缺口

这台宿主机上只登记了 Windows runner（`home-workstation-windows-adit-01`），
GitHub 侧没有可调度的 `home-workstation-linux-adit-01`。因此 `ci.yml` 矩阵里的
`linux-x64` 子任务，以及只跑 Linux 的 `ui-native.yml`，会一直停在 queued，直到
被下一次运行取消——**它们并不是「跑失败」，而是根本没有开始执行**。

判断一次 CI 结果时必须区分这两种情况：看 `jobs` 里子任务的 `status` 是
`queued`（等 runner）还是 `completed`（跑完了）。只有 Windows 子任务能作为
当前环境的实际门禁；Linux 侧的覆盖需要在 Linux 机器上单独复验。工作流可由 main push、同仓库 PR 或手动触发；外部 fork PR 不在持久化 runner 上执行。

### 固定并发组会把下一次运行一起堵住

`ci.yml` 的并发组是固定的（`group: adit-self-hosted-ci`，`cancel-in-progress: false`）。
由于 `linux-x64` 永远停在 queued，**整个 run 会一直占着这个组**，下一次 dispatch
的 run 只能停在 queued，直到前一个被取消。所以 Linux runner 缺失的后果不止于
「Linux 腿不跑」——它还让**新 run 的 Windows 腿也排队**。

新 dispatch 的 run 迟迟不动时，先取消上一个还占着组的 run：

```bash
gh run list --workflow ci.yml --limit 3
gh run cancel <上一个 run id>
```

取消后新 run 才会开始跑 Windows 腿。

### Linux 半边可以本地复刻

没有 Linux runner 时，`ci.yml` 的 Linux 部分可以在 Docker 里跑一遍，用于在提交前
发现只有 Linux 才会暴露的问题。这不是替代 CI 的正式门禁（容器里的发行版、图形栈、
`sudo`/netns 权限都和真实 runner 不同），但它能挡住绝大多数 Linux-only 的编译失败。

两个真实 runner 上才会失败、容器里同样会失败的坑：

- `fuser`（Linux 的 RDP 文件剪贴板，走 FUSE）用 pkg-config 探测 `fuse3`，缺了会在
  **编译期**报 `Package fuse3 was not found in the pkg-config search path`，而不是
  链接期报缺符号。因此 `libfuse3-dev` 必须在 `.github/scripts/setup-linux-runner.sh`
  的依赖列表里。
- `#[cfg(windows)]` 专属的构造会让对应 enum variant 在 Linux 上变成 dead code，
  而 CI 的 `clippy -D warnings` 会把它变成错误。仓库约定是给这类 variant 加
  `#[cfg_attr(not(windows), allow(dead_code))]`。

## 依赖准备

- Windows：MSVC x64 C++ Build Tools、Windows SDK、Git、PowerShell 7 和 Rust stable/Clippy。`.github/scripts/setup-windows-runner.ps1` 检查已有 MSVC/SDK，并在 runner 可写工具目录安装经过 SHA-256 校验的 just 1.59.0，不提升服务账号权限。缺少 MSVC/SDK 时需先在机器上安装对应组件。
- Linux：Ubuntu/Debian x64、可用的 Docker daemon 及免交互 sudo。`.github/scripts/setup-linux-runner.sh` 按缺失项安装编译器、GUI 库、Xvfb、TigerVNC/x11vnc、Python Tk/YAML、RPM 和网络测试工具；Rust stable/Clippy 由工作流准备。
- 两个平台都在 runner 实际服务账号下准备和使用工具链，不能以登录用户的 PATH 代替服务环境。

每个平台只执行一个连续任务。checkout 强制切换到被测提交，再清理除两个 workspace 的 target 目录以外的未跟踪文件，保留本机 Cargo 产物供复验和后续运行复用；产物有效性由 Cargo 按源码和工具链校验。编译并行数为 2，开发/测试产物关闭调试信息以控制 VM 的内存与磁盘使用。不上传 Cargo 构建缓存。

## 验证范围

两平台保留工作区 build、Clippy、测试和可选 VNC 验收入口的编译检查。Windows 还构建、检查独立的 RDP helper workspace。

Linux 额外执行换行和已保存输入证据审计、真实 x11vnc/TigerVNC、共享桌面 resize、隔离 network namespace 恢复、SSH 网关/凭据/光标、真实 sshd 集成、默认/Vulkan 无界面启动，以及 deb/rpm 安装启动检查。历史已捕获证据的审计不代表重跑原生 Windows 键盘验收。

集成夹具沿用既有固定 X display/loopback 测试端口，启动前检查占用，拒绝替换已有资源。VNC Docker 夹具标记当前 run ID，退出时只移除本次拥有的容器。工作流级并发组串行化不同 run，防止夹具重叠。无界面启动使用独立配置目录，结束时停止其进程组。

## 本地与发布

本地开发仍使用 `just ci` / `just ci-vnc`。正式 runner 的依赖准备不安装或启动生产 VNC 服务，不修改系统显示/键盘/睡眠配置。

正式 CI 和发布工作流均使用自托管 x64 runner；发布只启用 Windows x64，原始多平台配置保留为注释。手动环境初始化入口为 `.github/workflows/runner-bootstrap.yml`，Windows 与 CI/发布共用 `scripts/runner-bootstrap-windows.ps1`，Linux 复用已有依赖脚本。历史专用验收工作流不自动触发；发布流程仍须另行明确授权。普通 CI 和 bootstrap 不改版本、不打 tag、不创建 GitHub Release。
