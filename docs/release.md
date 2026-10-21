# 发布操作手册

当前发布工作流只使用仓库专用 Windows x64 runner `home-workstation-windows-adit-01`，没有托管 runner 回退。ARM64、macOS 和额外 Linux 发布任务暂停；原始发布工作流完整保留在 `.github/workflows/release.yml` 的注释中，原 CI 的 macOS/ARM 安装器任务也已保留为注释。

## 初始化与只读检查

运行环境初始化，不会构建正式版本或发布：

```powershell
gh workflow run runner-bootstrap.yml -f platform=windows
```

已准备好环境后，只检查依赖，并验证精确提交门禁（需先让当前 main CI 通过）：

```powershell
gh workflow run runner-bootstrap.yml -f platform=windows -f check_only=true -f validate_release_gate=true
```

Windows 脚本 `scripts/runner-bootstrap-windows.ps1` 与普通 CI 共用。GitHub CLI、just 和 Inno Setup 使用校验 SHA-256 的便携工具，安装在 runner 可写缓存；不提升 NETWORK SERVICE 权限、不更改服务账号、不重启服务。裸机缺少 Git/PowerShell/MSVC/SDK 时，已提升权限的管理员可显式使用 `-InstallSystemDependencies`，默认服务模式不会尝试提权。

Linux 初始化可用 `-f platform=linux`，两平台一起检查用 `-f platform=all`。Linux 使用现有依赖脚本，支持 `--check-only`。

## 发布门禁

`scripts/check-release-gate.ps1` 只读检查以下条件：

- 只能从 main 发布，而且远端 main 仍等于启动时的 SHA。
- 工作区、RDP helper、IPC crate 当前版本一致；请求必须是下一个 patch 版本。
- 同一 SHA 的 push CI 必须 completed/success；两个实际任务都必须在指定的 Windows/Linux x64 自托管 runner 上成功，不能用 skipped/托管 runner/其他提交替代。
- 请求的 tag 与 Release 都不存在；API 权限或网络错误不当作“尚不存在”。

`scripts/test-release-gate.ps1` 覆盖合法路径和拒绝条件，不连接网络、不推送。bootstrap 可在只读 token 下验证真实门禁；其临时 0.0.0 依赖探针安装包只在临时目录编译，不运行或发布。

## 正式发布（必须另行明确授权）

先确认当前 main 的 CI 成功，然后按实际下一个 patch 版本手动触发：

```powershell
gh run list --workflow ci.yml --branch main --limit 1
gh workflow run release.yml -f version=0.1.76
gh run watch <run-id> --exit-status --interval 30
```

示例版本仅展示格式，不能直接照抄旧版本。`all_platforms` 输入已注释停用，不能用它恢复多平台发布。发布期间不要往 main 推送，包括文档提交。

工作流顺序：checkout 精确 SHA → 只读门禁 → 检查依赖 → bump 三份 manifest/两个 lockfile → 校验版本锁步 → locked release 构建 app 和独立 helper → Inno 编译并确认资产 → 再检查 main → 原子推送 bump/tag → 创建 GitHub Release。只有这些步骤全部完成才算发布成功。

安装包失败时不会先推 tag。远端 main/tag 原子推送失败不会只落一个引用；若引用已推送而后续 GitHub Release 创建失败，需要检查现有 tag/Release，不能假设完全回滚或盲目重用版本。

最后以实际资产确认：

```powershell
gh release view v0.1.76
git pull --ff-only
```

应有 `adit-installer-vX.Y.Z.exe`。不从本地上传发布物，不手工绕过绿灯门禁，不把依赖探针或 CI artifact 当正式发布。

<!-- 以下是迁移前的完整操作手册，仅为历史参考，当前规则以上方为准。
# 发版操作手册

给人看的一步一步发版流程。发版**全程在 CI 上完成**——本地不构建、不发布任何
发布物，人只做四件事：确认、触发、盯、验收。所有机制（为什么这样设计）写在
[.github/workflows/release.yml](../.github/workflows/release.yml) 的注释里，
这里只写怎么做。

## 一次性准备

- 安装并登录 [GitHub CLI](https://cli.github.com/)：`gh auth login`。
  触发、盯进度、验收全靠它，不需要别的权限配置。
- 确认自己对仓库有 `contents: write` 权限（要触发 workflow 并让它推送 bump
  提交和 tag）。

## 第 1 步：确认 main 处于可发状态

```bash
git status --short --branch                       # 工作区干净，且与 origin/main 同步
gh run list --workflow ci.yml --branch main --limit 1   # 最近一次必须是 success
```

两个条件缺一不可：

- **工作区干净、已推送。** 发的是远端 main 的头提交，本地没推上去的改动不会
  进这个版本。
- **main 的 CI 是绿的。** release.yml 的第一步就是门禁：当前提交必须有一次
  已完成且结论为 success 的 ci.yml 运行，否则直接拒绝，不会产生任何提交或
  tag。CI 还在跑就等它跑完再触发，门禁不等人。

可选的双保险——本地跑一遍和 CI 同样的门禁（`just ci` 只覆盖 Windows，CI 的
Linux 构建及真实协议集成本地看不到，所以它是补充，不是替代；当前正式 CI
仅启用自托管 Windows/Linux x64，ARM64/macOS 已暂停，见 [CI 环境](ci.md)）：

```bash
just ci
```

## 第 2 步：确定版本号

```bash
git tag --sort=-creatordate | head -1
```

在最新 tag 的基础上递增 patch 位（例如 `v0.1.74` → 本次发 `0.1.75`）。
版本号格式必须是 `X.Y.Z`，**不带前导 v**——带 v 会被工作流第一步拒掉。

## 第 3 步：触发发布

```bash
gh workflow run release.yml -f version=0.1.75        # 把版本号换成第 2 步定的
```

默认只发 Windows x64 安装器。如果这次还要 arm64 安装器、Linux deb/rpm、
macOS dmg，加一个开关：

```bash
gh workflow run release.yml -f version=0.1.75 -f all_platforms=true
```

这些额外平台的包是在 x64 发布之后**追加**到同一个 Release 上的，所以事后想
补发也可以用同样的版本号加 `all_platforms=true` 再跑一次。

触发后拿到运行地址：

```bash
gh run list --workflow release.yml --limit 1
```

## 第 4 步：发布期间冻结 main

**从触发这一刻起到工作流结束，不要往 main 推任何东西——连文档提交也不行。**

工作流在最后才执行 `git push origin HEAD:main` 推送 bump 提交。这期间只要有
别人（或你自己）推了新提交，那次推送就成了非快进（non-fast-forward）而被拒，
结果是：门禁、构建、测试**全绿，但 Release 一个字节都没发布**。这是最容易踩
的坑，因为从日志前几步完全看不出来。

冻结期内本地可以继续写代码，只是先别 push。

## 第 5 步：盯进度

```bash
gh run watch <run-id> --exit-status --interval 30
```

- 默认（仅 x64）约 **18 分钟**；勾了 all_platforms 会更久（arm64 一个任务就
  约 12 分钟，且排在 x64 发布之后才开始）。
- 工作流依次做：校验版本号 → 绿 CI 门禁 → bump 三个 crate 版本 → 锁步校验 →
  构建 app 和 RDP helper → 提交 `chore: release vX.Y.Z` 并打 tag 推送 →
  Inno 打包 → 发布 GitHub Release。
- **任何一步失败都不会留下半成品**：提交和 tag 是在构建成功之后才推送的，
  失败了就修问题再用同一版本号重新触发（只要没走到发布那步，版本号可以复用）。
- `gh run watch` 偶尔会因本地网络抖动中断（GitHub API 连接断开），这不代表
  工作流失败——重新执行同一条 watch 命令接着盯即可，或用
  `gh run view <run-id>` 查当前状态。

## 第 6 步：验收——看资产，不看退出码

```bash
gh release view v0.1.75
```

**必须亲眼确认 Release 存在、标题正确、资产齐全。** 不要用运行是否成功来
代替验收，两个原因：

- `gh run watch --exit-status` 的退出码是**管道里最后一条命令**的返回值，
  写成 `gh run watch ... | tail` 会把任何失败都变成干净的 0。
- 运行本身也可能真的"全绿但没发布"——就是第 4 步那个非快进推送失败。

应看到的资产：

- 默认：`adit-installer-vX.Y.Z.exe`（x64 安装器）
- 勾了 all_platforms 时额外有：`_arm64.exe` 安装器、`adit_X.Y.Z_*.deb`、
  `adit-X.Y.Z-1.*.rpm`、`adit_X.Y.Z_*_unsigned.dmg`

Release 说明由 `--generate-notes` 自动生成（按合并的 PR 汇总），需要润色的话
在 GitHub 网页上直接编辑即可。

## 第 7 步：同步本地

工作流会把 bump 提交（`chore: release vX.Y.Z`）推到 main，本地拉一下保持同步：

```bash
git pull --ff-only
```

## 出问题怎么办

| 现象 | 原因 | 处理 |
| --- | --- | --- |
| 第一步就红：`version must be X.Y.Z` | 版本号带了 `v` 或格式不对 | 去掉 `v` 重新触发 |
| 门禁红：`ci.yml has never run` / `still in_progress` / `concluded failure` | main 的 CI 还没跑完或不是绿的 | 等 CI 绿了，或修好 main，再用同一版本号触发 |
| 构建红 | 编译问题 | 修好后重新触发；只要没走到发布，版本号可以复用 |
| 全绿但 Release 不存在 | 发布期间有人推了 main，bump 推送非快进被拒 | 确认 main 无新提交后重新触发；已经走到发布步骤的版本号不能复用，改发下一个 patch |
| `gh run watch` 报 `unexpected EOF` 后退出 | 本地网络抖动，与工作流无关 | 重跑同一条 watch，或用 `gh run view <run-id>` 查状态 |
| 某个平台资产缺失（arm64 / deb / rpm / dmg） | 默认不发 | 用同一版本号加 `-f all_platforms=true` 补发，资产会被追加到已存在的 Release |

## 不要做的事

- **不要从本地发版。** justfile 里没有 release 配方是有意为之；
  `just installer` / `just bump` 只用于本地冒烟测试，产出的东西永远不要上传。
- **不要绕过门禁。** 门禁拒绝时正确做法是修好 main 的 CI 再触发，而不是想办法
  跳过——v0.1.69 曾经从红 main 发出去过，这个门禁就是那时候加的。
- **不要手工打 tag 推送。** tag 由工作流在构建成功后创建，手工打的 tag 没有
  对应的 bump 提交，版本号会对不上。
- **发版期间不要推 main。** 见第 4 步——这是唯一一种"绿得毫无破绽却什么都没
  发布"的失败方式。

-->
