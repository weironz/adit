# #210 基础前台系统键与修饰键顺序：最终实际 VM 记录

被测产品提交 `a549ffb404536936b51361370ffb786b65e97e5a`，目标端 EXE
SHA-256 `8c668bc74059b480a392fb33a7b3439ae31f274cfc2cd0260d4c05099895fe20`。
目标 WIN11-VM Session 2，app=12056、peer=4396、observer=1568。
真实工具栏开启抓取，dispatcher 与前台窗口线程一致，实际字段见 grab-app.jsonl；
生产 hook 仍忽略 injected。输入来自现有 RDP helper 扫描码，
没有把它标成物理键盘硬件认证。目标端 hash 与[完整本地来源](../2026-10-21-window-shift-alias-local/README.md)
及6个 Git 源码 blob 一致，没有运行托管 Actions。

## 通过的实际行为

| 情况 | 实际判据 |
| --- | --- |
| Win+R | Super_L、r 按下/释放各一，完整顺序一致 |
| Alt+Tab、左右 Win | 各自完整释放，再进入下一组；不再发生 Alt-up 晚于下一组 Win-down |
| 左 Shift→Win→R / Win→左 Shift→R | 大写 R 与所属修饰键正确；Shift-up 不再被后续 Win 越过 |
| 右 Shift→Win→R / Win→右 Shift→R | 两种顺序完整通过，右 Shift 为0xffe2，等待超过超时阈值后 hook 仍 armed |
| 右 Ctrl / 右 Alt | 分别为0xffe4 / 0xffea，完整按下/释放；不等于德语 AltGr 文本语义验收 |
| 已持有主/数字键盘 Enter，后按 Ctrl/Alt | 两种 Enter 的8条顺序一致，Ctrl/Alt-down 在 Enter-up 前，无按键残留 |
| 持 Win 时 Ctrl+Alt+Enter | 本地释放抓取，所属 Win/Ctrl/Alt 补发释放，没有向 peer 发送新的 Enter |
| 普通 Shift 后再 Win+R | 普通窗口 Shift pair 与随后 Win+R 均正常，未触发边界超时 |
| 持 Win 切主机页 / 最小化失焦 | 各补发一次所属 Super-up，迟到的 up 不重复发送，抓取关闭 |
| 普通英文字母 | 两次公开测试词的字母逐一成对到达；不能当成中文预编辑通过 |

关键闭环证据：`right-shift-diagnostic-app.jsonl` 中真实窗口记录
NativeCode Windows `57398 = E036` 的按下/释放，同时 peer 收到完整右 Shift
组合；独立 observer 保留 vk=161、scan=54、flags=1/129、injected=false。
这证明最终同时匹配了 hook 与普通窗口身份，没有篡改系统标志。

`*-peer/app/observer.jsonl` 为逐阶段累积快照，不能相加计数。最终 peer 共
118条 key，全部释放有归属，各完整阶段结束无持键。核心顺序矩阵的46条
与先前已通过的窗口修饰键顺序一致。重复 R-down 控制请求经过 RDP 输入
归一化，未当成物理 hardware auto-repeat 的证明。

## 明确保留的未验收项

`cjk-preedit.commands.txt`、`cjk-mode-preedit.commands.txt` 是中文模式准备尝试，
实际 peer 收到 ASCII `zhongwen`。私有画面没有建立可接受的中文预编辑/
选词提交证据。点击系统输入法指示器后重新聚焦也未取得中文提交，因此
**本轮抓取模式下的中文 IME 仍未验收**，不以语言列表只有 zh-Hans-CN 或
没有首屏按键作为替代证明；两次尝试及全部 ASCII 输入都保留。

前置 German dead-key 状态、抓取模式国际布局/AltGr 回归、跨厂商 CJK/非 BMP
消费及物理硬件 repeat 继续由 #210 跟踪。没有据此关闭整个 Issue。
先前两个右 Shift 失败分别保留在[hook 前缀失败](../2026-10-21-modifier-order-vm-prefix/README.md)
和[窗口别名失败](../2026-10-21-window-shift-alias-vm-failure/README.md)，不覆盖成成功。

## 清理与来源

[清理源码](cleanup-all-owned.ps1) 只处理三处明确 GUID 根目录，逐个核对
规范化绝对路径、owner、非 reparse root、已知 cleanup.ps1 hash 和资源状态，
再删除自有目录。[独立报告](cleanup.json) 证明：自有 payload 进程为空、15915
无监听、三处共享与任务为空、三处目录已删除、左右 Shift/Ctrl/Alt/Win 高位
状态均未按下。语言标签仍仅 zh-Hans-CN，WinRM 仍 Stopped，防火墙仍1/0/0。
没有修改账号、密码、VPN、语言配置或用户 Adit 配置。

所有自有 GUI/peer/observer、测试控制台和 RDP 驱动已退出。诊断图片只保留
在本机私有临时目录，不提交 VM 截图、EXE、压缩包或凭据。文本换行统一 LF；
`sha256.json` 使用 LF 规范化后的归档字节，源码另核对被测提交的 Git blob。
`driver-part1-safe-output.txt` 属于先前 case3/4；最终驱动的受限输出为
`driver-final-safe-output.txt`，只接收测试前缀报告，不读取任意剪贴板文本。

`audit.py` 同时检查最终成功序列、未验收中文尝试、旧失败保留、源码/归档
hash 和清理状态。审计通过只对应上表和这些证据完整性，不扩大验收范围。
commands.txt 中 `ASCII ` 后面的单个空格是实际空格输入，保留其协议语义，
不按普通源代码末尾空白清除。
