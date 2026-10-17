# Linux 原生凭据库界面验收

本记录补充 #214 的界面证据。测试运行真正的发布构建 `adit`，通过
X11 鼠标和键盘事件操作 iced 设置页、新建会话页、普通会话入口和 GTK
文件选择器；没有调用 `AditApp::update`、`CredentialStore` 等内部接口。
agent 逐张检查原始截图，并在 UI 操作后独立检查磁盘版本、条目数、权限和
SHA-256。它不是无人值守的截图断言测试，也不能替代 Windows 安全对话框的
人工验收。

机器结果见 [JSON](results/2026-10-17-linux-vault-gui.json)。以下密码全是公开的
一次性合成测试值，不得用于实际配置：VNC `adit-ci`、主密码 A
`SyntheticVault-A-2026`、主密码 B `SyntheticVault-B-2026`。
没有访问用户的 Windows 虚拟机、公共 VNC 主机或真实配置目录。

## 构建和隔离边界

- 源码：`38c4f43232fcab25d1f87566851714600870a482`。
- [CI 38993651189](https://github.com/weironz/adit/actions/runs/38993651189)，
  产物 `adit-linux-packages-x86_64`，包版本 `0.1.75`。
- Linux `adit` SHA-256：
  `47396dc5de7fc2c0981dc6f0cf7f8e3b1582045bbea4e24f225407afb3123a93`。
- 容器 `adit-vault-ui-38c4f43`，Ubuntu 24.04.5，Xvfb `:121`
  1400×1000、iced 窗口 1360×860，Mesa 软件渲染。
- 唯一配置目录 `/tmp/adit-vault-ui/config`；`HOME`、运行时目录也在同一
  独占临时目录下。真实配置未挂载进容器。
- 独立 TigerVNC 1.13.1，显示 `:130`，`127.0.0.1:15920`、800×600、
  `VncAuth`。它与应用在同一个专用容器，只监听容器本地回环，没有发布端口。
- 远端画面为 `xsetroot -solid '#244a72'`，用于确认认证后收到的实际帧。

复现时可下载同一 CI 产物并 `dpkg-deb -x` 到专用目录；不要从源码重新构建
后沿用这里的二进制 SHA。容器需要 Xvfb、xdotool、TigerVNC、X11 工具、
中文字体、Mesa，以及 `dbus-x11`、GTK desktop portal 和应用的运行时依赖。
这次使用既有测试容器，补齐依赖后从 CI deb 解包，未重新编译 Linux Rust。

```sh
# 所有命令只在专用的可丢弃 Linux 测试容器中执行。
mkdir -p /tmp/adit-vault-ui/config /tmp/adit-vault-ui/home /tmp/adit-vault-ui/runtime
chmod 700 /tmp/adit-vault-ui/runtime
Xvfb :121 -screen 0 1400x1000x24 -nolisten tcp > /tmp/adit-vault-ui/xvfb.log 2>&1 &
printf adit-ci | tigervncpasswd -f > /tmp/adit-vault-ui/vnc-passwd
chmod 600 /tmp/adit-vault-ui/vnc-passwd
Xtigervnc :130 -rfbport 15920 -rfbauth /tmp/adit-vault-ui/vnc-passwd \
  -SecurityTypes VncAuth -geometry 800x600 -depth 24 -localhost yes \
  > /tmp/adit-vault-ui/tiger.log 2>&1 &
DISPLAY=:130 xsetroot -solid '#244a72'
DISPLAY=:121 ADIT_CONFIG_DIR=/tmp/adit-vault-ui/config \
  HOME=/tmp/adit-vault-ui/home XDG_RUNTIME_DIR=/tmp/adit-vault-ui/runtime \
  LIBGL_ALWAYS_SOFTWARE=1 WGPU_BACKEND=gl \
  dbus-run-session -- /opt/adit-under-test/usr/bin/adit \
  > /tmp/adit-vault-ui/app.log 2>&1 &
```

无窗口管理器的 Xvfb 不会自动设置键盘焦点。每次应用启动、GTK 文件选择器
关闭后，先用 `xdotool search --name '^Adit'` 获取真实窗口 ID，然后
`DISPLAY=:121 xdotool windowfocus WINDOW_ID`。漏做会使鼠标点击有效但键盘
输入落到根窗口或 portal 窗口，这是夹具焦点问题，不应解释成凭据库输入失败。
不要向任何实际账户的认证窗口输入这些合成值。

## 实际执行顺序与判定

1. 从“文件 → 设置 → 应用”滚动到“凭据库主密码”，点“读取状态”。看到
   “兼容模式（无主密码）”。首次打开显示“尚未读取”，必须先读取状态才能
   使用解锁等相关按钮；读取会清空已填的秘密，随后再输入。
2. 在“新主密码”和“确认新主密码”输入 A，勾选理解说明，点“启用保护”。
   看到已启用和旧模式备份路径。磁盘 `credentials.json` 为 v2、0 条，
   旧 v1 备份存在，二者权限均为 0600。
3. 关闭设置，“新建主机 → VNC”，名称 `Synthetic-Vault-VNC`，主机
   `127.0.0.1`、端口 `15920`、密码 `adit-ci`，保持必须密码认证，点“保存”。
   v2 增为 1 条，原始 JSON 和 profiles 文件均不包含 VNC 密码明文。
4. 双击左侧该会话，确认连接状态及完整蓝色远端帧；不调用内部连接方法。
5. 返回设置，点“锁定”。状态为已锁定，既有 VNC 会话继续显示已连接。
   锁定前后文件 SHA 一致。
6. 填错误主密码，点“解锁”：显示“主密码错误或凭据文件已损坏”，输入被
   清空，文件 SHA 保持一致。填 A 后解锁成功。
7. 点“导出加密备份”，在真实 GTK 保存窗口选择
   `/tmp/adit-vault-ui/export-A.json`。文件为 0600、514 字节，`cmp` 与
   导出时的 v2 库完全相同，不包含主密码。
8. 当前密码填 A，新密码和确认填 B，点“修改主密码”。看到成功及旧保护
   备份路径。锁定后 A 被拒绝，B 能解锁；已有 VNC 会话未断开。
9. 当前密码填 B，点“关闭保护”。看到恢复兼容模式及原保护备份路径。
   当前文件为 v1、1 条，仍是密文；原 v2 B 库保存为 0600 备份。
10. 导入文件主密码填 A，点“导入加密凭据”，在真实 GTK 打开窗口选择
    `export-A.json`。看到“已导入……请用导入文件的主密码解锁”。磁盘变回
    v2、1 条，导入完成时 `cmp` 与导出 A 文件完全相同，原 v1 文件有备份。
    当前库变为锁定，A 能解锁。已有 VNC 会话继续运行。
11. 终止并冷启动仅此容器的 Adit 进程，保留同一专用配置目录。双击普通会话
    入口：提示“凭据库已锁定，请先在设置中输入主密码解锁”，只显示手工
    VNC 密码输入框，没有静默使用保存密码建立连接。
12. 取消手工密码框，进入设置，读取状态后用 A 解锁，再双击同一会话。
    使用保存的凭据成功认证，收到完整蓝色帧。没有输入第二次 VNC 密码。
13. 再次导出并取消 GTK 选择器：显示“已取消，凭据未修改”，SHA 不变。
14. 导入 A 文件时故意提供错误的文件主密码：显示主密码错误，文件 SHA
    不变，当前库仍保持可用，既有会话继续运行。

上述步骤的磁盘检查只读取测试夹具目录。文件替换和迁移由 UI 操作触发，
没有用 Python 改写凭据库来伪装界面成功。核对条目数量和版本可使用：

```sh
python3 - <<'PY'
import hashlib, json, pathlib, stat
p = pathlib.Path('/tmp/adit-vault-ui/config')
for f in sorted(p.glob('credentials.json*')):
    if f.name.endswith('.lock'):
        continue
    raw = f.read_bytes()
    data = json.loads(raw)
    print(f.name, data['version'], len(data['entries']),
          oct(stat.S_IMODE(f.stat().st_mode)), hashlib.sha256(raw).hexdigest())
    assert b'adit-ci' not in raw
    assert b'SyntheticVault-' not in raw
PY
```

## 范围限制

这证明 Linux 原生 iced UI、GTK 文件选择器、普通 VNC 会话入口及磁盘状态
之间的实际集成。SSH/RDP 冷启动凭据连接使用各自既有真实服务测试，不能从
这一次 VNC UI 结果单独推出它们的全部界面行为。

本记录不宣称完成 Windows 安全设置/认证对话框的人工验收，不覆盖 Windows
Hello、系统证书库、DPAPI 策略或 OS 休眠。截图与 JSON 均只包含合成测试
配置；没有提交 credentials 文件、可解密备份或真实主密码。

为补强进程退出和旧密码拒绝的证据，随后重复一次 GUI A→B 修改和冷启动：
先确认 UI 已锁定，在旧 A 解锁失败前后执行 SHA-256 对比并通过；终止
旧 PID 644 后，确认 `/proc/644` 已不存在且 `pgrep -x adit` 返回无进程，
然后启动新 PID 1126。锁定时无新 VNC 连接；设置中读取状态并用 B 解锁后，
普通双击入口在 TigerVNC 日志产生新的 VncAuth(2) 连接（53906），
与重启前已关闭的连接（57966）不同，并收到完整远端帧。最后的库为 B 保护。
此重复轮的进程和 SHA 证据也写入 JSON，服务端日志只含合成回环靶标。
