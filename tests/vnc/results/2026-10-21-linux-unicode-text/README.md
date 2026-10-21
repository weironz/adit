# #205 Linux 三厂商实际 Unicode 文本消费

本轮通过生产 `SessionManager` / `vnc_fixture_driver TEXT`，把字符发送到真实
厂商 VNC 服务端，再独立读取原生 Tk Entry 的实际文本模型。**12/12 通过**：

| 服务端 | ASCII `abc` | 中文 `中文` | 非 BMP `😊` | 组合字符 `e` + U+0301 |
| --- | --- | --- | --- | --- |
| x11vnc 0.9.16 | 通过 | 通过 | 通过 | 通过 |
| TigerVNC 1.13.1 | 通过 | 通过 | 通过 | 通过 |
| TurboVNC 3.3.1 | 通过 | 通过 | 通过 | 通过 |

每项新建 VNC 连接，使用公开合成认证，经 UTF-8 stdin 传递；没有剪贴板替代。
目标 control 只支持清空、读取、退出，不能接收 expected 文本或直接插入文本。
字符由原生 Entry 类绑定消费；不是记录 keysym 后手工追加文本。

独立核对每例输入前为空、目标 PID 不变、真实 X11 input focus 等于 Entry XID、
输入时始终聚焦、无丢焦及事件截断。ASCII/中文/emoji/combining 分别取得
3/2/1/2 次按下及相同次数释放，实际 `Entry.get()` 逐 Unicode scalar 相等。
Tcl 可能暴露有效 UTF-16 代理对，检查器只合并有效配对并拒绝孤立 surrogate。

## 保留的失败及修复

`first/` 的12例都在发送 TEXT 前被焦点门槛拒绝：Tk 内部 focus 为真，但没有
窗口管理器时 X11 真实 input focus 仍是 PointerRoot(1)。因此它们是夹具准备失败，
不能记为厂商 Unicode 失败。目标现在显式选择自己的 Entry X11 focus、同步后
重新查询，并保留同样严格的输入前/后/输入时检查。

第一版清理扫描器的 argv 自身含随机目标路径，误把自身算入目标并 SIGTERM。
已排除扫描器自己的 PID；第二轮三个目标都正常退出且 `cleanup_verified=true`。
第一轮残留的三个随机脚本随本轮自有容器一起销毁，没有影响旧容器。
`second/` 完整保留修复后的12项结果、原生按键、目标/driver stdout 和 stderr。

## 来源与范围

driver 重新构建退出0，SHA256 为
`5bb7117156fa94adf718daf3445eee3a0c91928e993eb05dc7956186fef58ad2`。
所链接生产代码与 `eea9b178d8fae1641b3e5dde2484d240b2e51b56` 的 Git blob 一致，
见 `provenance.json`；构建时另有未链接进此 driver 的 UI 隔离实验，已明确记录。
容器基于现有本地镜像 `adit-vnc-integration:local`，实际 image ID、Ubuntu/Python
版本和厂商版本保留在结果中。只发布 127.0.0.1 的15902/15903/15904；没有
变更宿主网络或厂商安全策略。自有容器与发布监听最终均移除，见 `cleanup.json`。

这是短样本的真实厂商应用消费证据，不扩成所有字体/应用、物理键盘、原生 IME、
弱网性能或新一轮长期运行证明。Windows TightVNC 实际消费与 UltraVNC 非 BMP
截断限制仍独立由 #205 跟踪。没有消耗新的 GitHub Actions。

运行：先按 `tests/vnc/README.md` 创建带 loopback 端口的自有 matrix 容器，再执行：

```powershell
python tests/vnc/run_unicode_text_matrix.py --container <owned-container> `
  --driver target/debug/examples/vnc_fixture_driver.exe --driver-source <full-sha> `
  --results <fresh-absolute-results-directory>
python tests/vnc/results/2026-10-21-linux-unicode-text/audit.py
```

脚本验证实际端口映射、记录二进制 hash；`--driver-source` 是显式声明而非脚本
自行证明构建来源。本轮另核生产 Git blob，不能给任意旧 EXE 填新 SHA 冒充来源。
