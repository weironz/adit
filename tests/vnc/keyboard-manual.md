# 原生键盘人工验收（#210）

仅用于现有隔离 TigerVNC :112 / loopback 15915，**DPI 验收结束后**再由操作者启动独立目标。它会抬起并聚焦新窗口；不重启 VNC、不改产品或 Windows 设置、不自动按系统键。复用容器中的 Tk/X11 依赖：

```sh
python3 /keyboard_target.py --display 112 --seconds 600
# 以下命令在同一容器的另一个 shell 中执行。
python3 /keyboard_target.py --command CLEAR
python3 /keyboard_target.py --command PHASE --phase modifiers
python3 /keyboard_target.py --command STATUS
python3 /keyboard_target.py --command QUIT
```

控制 socket 默认为 `/tmp/adit-vnc-keyboard.sock`，权限 0600；存在时拒绝覆盖。只保留内存中最近 256 条白名单事件，溢出计数；非白名单输入只加 `ignored`，不保存键值、文本、修饰状态或时间。窗口和 STATUS 显示过滤后的 KeyPress/KeyRelease、X11 keysym、原始 modifier 位、固定阶段及相对时间；不读剪贴板，不设自由文本输入框。CLEAR 清事件/计数，PHASE 只允许 `idle/modifiers/win/alt_tab/altgr/ime/release`。收集 STATUS 后 QUIT；也可点击 Quit target，默认十分钟自动退出，不写用户配置或后台持久服务。

1. 记录 Adit 版本、当前 Windows 键盘布局/IME、抓取模式和远端 X11 布局；点击目标空白处。每个阶段 CLEAR，再 PHASE。只输入公开合成白名单：左右 Win/Ctrl/Alt/Shift、AltGr、Tab/Enter/Escape、`a A q @`、`中文`、`é`、`e` 加 U+0301、`😀`。
2. `modifiers`：用户**物理按下并松开**左右修饰键，再输入 `a` / Shift+A；检查成对 Press/Release 和 modifier 状态。不要按 Win+L、Ctrl+Alt+Del 等会锁定本机或进入安全桌面的组合。
3. `win` / `alt_tab`：分别在客户端未抓取、明确启用抓取时，由用户物理按 Win、Alt+Tab。记录本地开始菜单/任务切换是否出现、目标收到的 keysym 和释放事件。本目标没有窗口管理器；收到 Tab 只能证明键路由，不能证明远端任务切换。按客户端本地 **Ctrl+Alt+Enter** 退出/释放，确认本地可操作，再恢复目标焦点。若当前版本此快捷键只退出全屏，须另核实抓取已释放，不能把退出全屏等同释放成功。
4. `altgr`：仅在已知支持相应映射的现有布局上，物理输入 AltGr 组合（例如德语布局 AltGr+Q → `@`）；同时看左右 Ctrl/Alt 或 ISO_Level3_Shift、最终 `@` 及松开。无相应布局就记录未验收，不临时改布局制造通过。
5. `ime`：用户切换本机真实 IME，观察 Adit 原生候选/预编辑窗口，物理完成 `中文`；随后在已有可用输入法中测试 `é`、`e`+组合重音、`😀`。记录提交一次、位置/焦点正确和切标签时无残留；目标事件只能证明提交后到达的字符。没有真实组合/候选窗口过程的结果不能证明原生 IME。**ASCII 自动注入、VNC 协议 TEXT、剪贴板粘贴均不能替代此步骤。**
6. `release`：短暂按住修饰键后，用本地退出快捷键释放抓取，再点回目标输入 `a`。检查没有卡住的 Ctrl/Alt/Shift；记录本地操作正常与目标事件。保存各阶段过滤后的 STATUS，并标记人工操作者、真实操作方式、成功/失败/未验收，最后 QUIT。

Linux Xserver 的 keysym/布局与 Windows 扫描码不是一回事：AltGr 可表现为 `Alt_R`、`ISO_Level3_Shift` 或 Ctrl+Alt，Win 可映射为 Super/Meta。Tk/X11 对 Unicode、组合重音、emoji 的支持受服务端布局、XIM/Tk 版本及字体影响；未知 keysym 在允许字符已确认时只记 `<unmapped>`，非白名单字符仅计数。原始 state 位需连同实际布局解释，不能用显示字形缺失推断客户端丢键，也不能仅凭 UTF-8 剪贴板成功宣称键盘/IME成功。失败应先区分客户端捕获、RFB 键事件、X11 映射和字体显示。
