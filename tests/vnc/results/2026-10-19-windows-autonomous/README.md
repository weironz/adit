# Windows runner 自动验收原始证据

本目录归档隔离 GitHub-hosted Windows Server 2025 的分项实测。结果只证明各自声明的范围；Issue #205/#207/#210/#212/#213 是未完成事项的唯一状态入口。没有发布版本，也没有操作用户的桌面或使用用户的主机凭据。

| 运行 | 归档 | 实际覆盖 |
| --- | --- | --- |
| [39176249851](https://github.com/weironz/adit/actions/runs/39176249851) | `recovery-39176249851` | 生产 SessionManager、Windows 真实 TCP、35 秒静态画面及真实 Fence 回应；保持 TCP Established 后停止应用回应，超时撤下旧帧，同 tab 自动重连，完整新帧恢复，ZRLE/只读策略不变。此处显式 loopback None 合成 peer。 |
| [39176083194](https://github.com/weironz/adit/actions/runs/39176083194) | `lock-39176083194` | 真实 UltraVNC 1.8.3.0 服务，SCM/worker/活动 console 归属；WTS unlocked→locked；生产 VncAuth 客户端取得完整锁屏画面，两个原桌面颜色标记消失。 |
| [39177242959](https://github.com/weironz/adit/actions/runs/39177242959) | `dpi-capability-39177242959` | Settings 实际 OS DPI 96→144→192→96；原分辨率恢复。这是 runner 能力证据，本身不证明 Adit 的 DPI 几何。 |
| [39177975803](https://github.com/weironz/adit/actions/runs/39177975803) | `service-39177975803` | Windows 原生分辨率 1280×1024→1600×1200→原 1024×768；每次新建生产 VncAuth 客户端核对完整帧和颜色标记，再验证真实锁屏。 |
| [39229588182](https://github.com/weironz/adit/actions/runs/39229588182) | `service-39229588182` | 同一轮服务/分辨率/UAC/锁屏全通过。创建一次性管理员，实际取得 limited token=3；consent.exe 与 input desktop access denied=5 确认安全桌面；生产 VncAuth 完整画面中可见 Windows Command Processor 的权限提示；独立清理客户端取消，ERROR_CANCELLED=1223；账户删除、UAC 策略恢复、服务/worker/端口/自有防火墙清理均核对。 |
| [39230932522](https://github.com/weironz/adit/actions/runs/39230932522) | `native-app-39230932522`、`recovery-39230932522` | 新二进制应用缩放 100/150/200% × Fit/1:1，各三个窗口尺寸，加远端尺寸请求三个窗口，共 21 次点击全部 ≤2 像素；恢复子任务也成功。整轮 workflow 的后续 OS DPI 夹具因控制文件临时访问冲突失败，原 stderr/JSON 保留，不称整轮成功。 |
| [39231634543](https://github.com/weironz/adit/actions/runs/39231634543) | `native-dpi-39231634543` | 实际 OS HWND DPI 96/144/192 × Fit/1:1，各三个窗口尺寸，加远端尺寸请求三个窗口，共 21 次点击，最大误差 2 像素。6 组 × 箭头/I-beam/自定义掩码/隐藏共 24 次 GPU 光标检查，非零热点、透明孔、工具栏/窗口外本地指针恢复通过。OS DPI 和分辨率恢复，app/peer 正常退出。 |
| [39231770633](https://github.com/weironz/adit/actions/runs/39231770633) | `service-39231770633` | 服务/系统分辨率/UAC/锁屏整轮通过；本轮以生产 SessionManager 的 VncAuth 输入驱动发送 Escape down/up，实际权限请求进程回报 ERROR_CANCELLED=1223，证明安全桌面协议输入真实生效。不是独立清理客户端代发；仍不等同物理键盘 hook、IME、批准提权或解锁登录。 |

新二进制来自源码 `731317988428a9e1a4e3dedcc19dcd10f0052573`，SHA256 `896b2731bb63562f797d5e822e2319101f0e823b175c214810c2918fd323bed5`。OS DPI 运行的夹具源码为 `797f7121d977f2522da1717ad4ddbc3bee7f9832`；复用前再次验证 Rust/资源输入及编译器完全一致。构建来源与测试来源分别记录，不冒充同一提交。

`original-failures/native-39230110816-*` 保留修复前 200% DPI 的真实失败：1000×700 物理窗口客户区是 487×314.5 逻辑像素，实时 resize 错用了恢复窗口的 320 下限，导致计算仍用旧几何。修复只过滤零值/非有限值后，应用缩放和 OS DPI 矩阵都覆盖了此小窗口情况。单元回归也保留最小化不能覆盖实时尺寸、恢复尺寸仍受下限保护的断言。

UAC 画面在 `service-39229588182/uac/uac.png`；锁屏前后 PNG 在各自 `lock-screen` 中。PNG 从原 PPM 无损转换，保留解码 RGB；锁屏结果含原像素 SHA256。UAC 的 `snapshot_sha256` 是原 PPM 文件哈希，不能与转换后的 PNG 文件哈希直接比较。

服务端原生分辨率切换使用新的连接获取画面，不能单独证明一个连续连接的 resize 或客户端 SetDesktopSize；后两项需要结合已有真实 TigerVNC 共享尺寸回归与原生 GUI 测试。早期 UAC 取消使用独立清理客户端；39231770633 改为生产输入路径。两者均不能写成物理系统键抓取、批准提权或解锁通过。锁屏也不证明解锁、密码输入或安全注意序列。

`service-39178614367` 保留组合运行失败：其中 UAC 子项成功，但取消后 runner 控制台盖住了测试窗口，锁屏前颜色标记检查失败。修复自有测试窗口焦点后，39229588182 整轮通过。`original-failures` 保留原生窗口和恢复夹具的失败结果；未把失败或部分通过改写成全部通过。

runner 的 `powercfg /a` 明确不支持睡眠/休眠；本轮 TCP 应用无响应不是 OS 丢包黑洞、物理网卡切换或真实睡眠恢复。注入指针也不证明物理键盘 Win/AltGr、中文 IME 或实际键盘布局。

复核：安装 Pillow 后，在仓库执行 `python tests/vnc/results/2026-10-19-windows-autonomous/audit.py`。它只读校验所有归档字节哈希、独立像素哈希、锁屏状态与恢复/清理断言。`.gitattributes` 防止原始证据被 Git 换行转换；`manifest.json` 不包含自身。

复跑入口：`.github/workflows/vnc-windows-autonomous.yml` 的 `scope=recovery/capabilities/native/dpi`，以及 `.github/workflows/vnc-windows-lock.yml`。所有系统变动仅允许 disposable hosted Windows；原生二进制复用要同时验证完整 Rust 输入、编译器和 exe 哈希，并分别记录构建来源与本轮测试来源。
