# x64 CI

正式检查由 `.github/workflows/ci.yml` 执行，只能手动触发（`gh workflow run ci.yml --ref main`），矩阵两个子任务：

| 平台 | Runner | 标签 |
| --- | --- | --- |
| Windows x64 | GitHub 托管 `windows-2025` | `windows-2025` |
| Linux x64 | GitHub 托管 `ubuntu-24.04` | `ubuntu-24.04` |

ARM64 与 macOS 暂停。

CI、发布和手动环境检查使用相同的托管镜像。独立的 `ui-native.yml` 在 `ubuntu-24.04` 上运行。Win11 VMware 虚拟机用于桌面和协议验收。

### Linux 半边可以本地复刻

没有 Linux runner 时，`ci.yml` 的 Linux 部分可以在 Docker 里跑一遍，用于在提交前
发现只有 Linux 才会暴露的问题。这不是替代 CI 的正式门禁（容器里的发行版、图形栈、
`sudo`/netns 权限都和真实 runner 不同），但它能挡住绝大多数 Linux-only 的编译失败。

两个真实 runner 上才会失败、容器里同样会失败的坑：

- `fuser`（Linux 的 RDP 文件剪贴板，走 FUSE）不开 `libfuse3` feature，由 fuser 自己
  调用 `fusermount3` 挂载，所以构建不需要 `libfuse3-dev`，安装包只依赖提供
  `fusermount3` 的 `fuse3`。不要重新打开这个 feature：链接 libfuse 会把包绑死在一个
  soname 上，Ubuntu 25.10+ 和 Fedora 44+ 只有 `libfuse3.so.4`，v0.1.78 的 deb 就因为
  依赖 `libfuse3-3` 在 Ubuntu 26.04 上装不上（#253）。
- `#[cfg(windows)]` 专属的构造会让对应 enum variant 在 Linux 上变成 dead code，
  而 CI 的 `clippy -D warnings` 会把它变成错误。仓库约定是给这类 variant 加
  `#[cfg_attr(not(windows), allow(dead_code))]`。

## 依赖准备

- Windows：MSVC x64 C++ Build Tools、Windows SDK、Git、PowerShell 7 和 Rust stable/Clippy。`.github/scripts/setup-windows-runner.ps1` 检查已有 MSVC/SDK，并在 runner 可写工具目录安装经过 SHA-256 校验的 just 1.59.0，不提升服务账号权限。缺少 MSVC/SDK 时需先在机器上安装对应组件。
- Linux：Ubuntu/Debian x64、可用的 Docker daemon 及免交互 sudo。`.github/scripts/setup-linux-runner.sh` 按缺失项安装编译器、GUI 库、Xvfb、TigerVNC/x11vnc、Python Tk/YAML 和网络测试工具；Rust stable/Clippy 由工作流准备。
- 两个平台都在 runner 实际服务账号下准备和使用工具链，不能以登录用户的 PATH 代替服务环境。

每个平台在新建的托管虚拟机上检出当前提交并运行完整任务。编译并行数为 2，开发和测试构建关闭调试信息与增量编译。

## 验证范围

两平台保留工作区 build、Clippy、测试和可选 VNC 验收入口的编译检查。Windows 还构建、检查独立的 RDP helper workspace。

Linux 额外执行换行和已保存输入证据审计、真实 x11vnc/TigerVNC、共享桌面 resize、隔离 network namespace 恢复、SSH 网关/凭据/光标、真实 sshd 集成、默认/Vulkan 无界面启动，以及 deb 安装启动检查。另有 `rpm` 任务（暂停，已注释），在最新 Fedora 稳定版（现为 `fedora:45`）容器里编译、打包 rpm，再到同版本的干净容器里安装并启动；rpm 的库依赖取自编译所在系统，所以必须在目标版本里编译。恢复后发布门禁要求这个任务为绿。历史已捕获证据的审计不代表重跑原生 Windows 键盘验收。

集成夹具沿用既有固定 X display/loopback 测试端口，启动前检查占用，拒绝替换已有资源。VNC Docker 夹具标记当前 run ID，退出时只移除本次拥有的容器。每次运行的夹具有独立的托管虚拟机。无界面启动使用独立配置目录，结束时停止其进程组。

## 本地与发布

本地开发仍使用 `just ci` / `just ci-vnc`。正式 runner 的依赖准备不安装或启动生产 VNC 服务，不修改系统显示/键盘/睡眠配置。

正式 CI 和发布的 Windows 部分使用 GitHub 托管 `windows-2025`，Linux 部分（含 deb）使用 GitHub 托管 `ubuntu-24.04`；ARM64/macOS 原始配置保留为注释。手动环境初始化入口为 `.github/workflows/runner-bootstrap.yml`，Windows 与 CI/发布共用 `scripts/runner-bootstrap-windows.ps1`，Linux 复用已有依赖脚本。历史专用验收工作流不自动触发；发布流程仍须另行明确授权。普通 CI 和 bootstrap 不改版本、不打 tag、不创建 GitHub Release。
