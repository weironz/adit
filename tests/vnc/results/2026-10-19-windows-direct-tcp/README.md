# Windows 直连 TCP 故障验收（2026-10-19）

本目录记录 hosted Windows runner 经公网直接连接 Linux 合成 RFB 服务的运行，
补充 VM 经 NetBird TCP 终结网关不能提供的直连 socket 证据。目标仅产生
800×600 红、蓝、绿色块，无真实桌面、剪贴板或主机凭据。
固定合成认证值是公开的 `adit-ci`；不能用本夹具宣称认证安全性。

## 运行来源

| 运行 | 源码 | 结果范围 |
| --- | --- | --- |
| 39250884698 | 2dd773f | 15916 无连接，客户端 TCP 超时；零 VNC 样本，失败保留。 |
| 39252193394 | 7c4747a | 5900 同样无连接，零 VNC 样本；自有链删除、实际规则恢复。 |
| 39252709893 | d2b1cbb | 精确 runner 来源的 5901→5900 内核转发命中，但后续原生 nft 入站策略拒绝；合成握手未完成，没有认证。转发和 filter 规则恢复。 |
| 39253156937 | 1c6eac8 | 通过原生 nft 和 iptables 两层来源/端口限定，结果见本目录原始记录和独立审计。 |

前三轮都是设施失败，不计为产品回归通过，也不能把接口准备、构建成功计作 VNC 样本。
最初 cleanup.json 比较整个 iptables-save 文本，把时间戳和策略流量计数变化误报为
规则改变；原始文件保留，独立语义比较排除这些变化后确认实际规则一致。
后续控制器已修正比较方法，同时保留宿主机原始快照在私有临时目录。
完整生产规则与私有系统信息不发布到仓库。

## 合格运行的实际结果

[39253156937](https://github.com/weironz/adit/actions/runs/39253156937) 已 completed/success。
客户端时间轴（相对其启动）：{"ready": 117, "disconnected": 70068, "recovered": 86221, "resumed": 89342} ms。
Linux INPUT/OUTPUT 各观察到 8 个实际丢包；同一会话恢复完整蓝、绿帧，
VncAuth/ZRLE/只读保留。客户端正常退出，stderr 为空。
这是单次样本，不能输出可靠分位数或仅凭时间差指定由哪个 Windows 计时器触发。

清理后独立复查合成服务 PID 不存在、controller unit 和清理 timer 均 inactive；
filter/NAT/native nft 规则内容全部恢复。Linux 合成服务使用 SIGTERM 结束，
所有线程随进程退出；没有生成 graceful peer cleanup 事件，不把它写成正常 RFB 收尾。

## 合格运行应满足的条件

Windows 端使用生产 SessionManager 和真实 TCP，要求 VncAuth/ZRLE、只读、无 Fence。
预检仅核对零挑战合成握手并退出，不发送认证值；然后生产探针建立正式会话。
红帧后先保持 40 秒静态桌面，再在 Linux INPUT/OUTPUT 精确丢弃该来源/端口
的 TCP 报文 45 秒。要求看到实际丢弃计数、旧帧撤销、同一标签且仅一个 session
恢复完整蓝帧与新绿帧，不能仅通过重新 TCP 连接来宣称恢复。

结束时需同时核对自有进程退出、自有 iptables 链不存在、filter/NAT/native nft
规则内容与测试前一致；独立 systemd 15 分钟定时清理用于控制器意外退出。
未修改全局 TCP 参数、现有桌面服务或其他来源连接。

这仍不是 Windows 物理网卡切换、OS 待机/唤醒或真实厂商桌面的证明。
另有 Windows 合法挂起 incremental 请求 35 秒不误断的回归；本夹具静态阶段
仍返回空更新，因此不能拿这一静态阶段替代完全无应用回应的测试。

## 独立校验

`python tests/vnc/results/2026-10-19-windows-direct-tcp/audit.py`

逐文件哈希、成功与失败终态、完整色块/同标签策略、真实 OS 丢包计数和规则恢复
共同决定结果。任何单项成功不足以覆盖整体失败。
