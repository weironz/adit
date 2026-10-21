# 自托管 x64 CI

正式检查由 `.github/workflows/ci.yml` 执行，只调度两个仓库专用 runner：

| 平台 | Runner | 必需标签 |
| --- | --- | --- |
| Windows x64 | `home-workstation-windows-adit-01` | `self-hosted, Windows, X64, home-workstation, repo-adit` |
| Linux x64 | `home-workstation-linux-adit-01` | `self-hosted, Linux, X64, home-workstation, repo-adit` |

ARM64 与 macOS 暂停，不回退到 GitHub 托管机器。工作流可由 main push、同仓库 PR 或手动触发；外部 fork PR 不在持久化 runner 上执行。

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

这次迁移只处理正式 `ci.yml`。历史专用验收工作流与 `release.yml` 没有被自动触发；发布流程仍须另行明确授权。本 CI 不改版本、不打 tag、不创建 GitHub Release。
