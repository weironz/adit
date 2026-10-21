# #212：Windows VM 宿主映射与恢复准备，只读证据（2026-10-21）

**本轮没有执行目标 VM 的电源、睡眠、NIC、路由或网络配置变更。** 目标 UUID 为 `A8A34D56-3ADC-763F-E8EC-203ACC603E63`。当前尚未取得这个 UUID 对应的可信宿主管理链路，真实 Windows NIC 切换与 OS 睡眠恢复继续保持未验收。

## 新证据与确切失败边界

1. 本机 VMware Workstation 26.0.0 build-25388281 的 `vmrun -T ws list` 成功返回 0 台运行中 VM；inventory 只有三台 Ubuntu，逐一按 Windows Guid 字节序转换后的 BIOS UUID 均不匹配目标。本机 VMware/VMX/vmrest 进程和 8697 listener 均为空。Hyper-V inventory 实际抛出 `VirtualizationException / Value cannot be null (Parameter name)`，记录为失败，不能当成空 inventory 成功。见 `local-host-control.json`。
2. 目标 72 子网经 NetBird `wt0`，不是本机 VMnet8 的 75 子网。路由节点 `willoptpc-172-11.netbird.selfhosted / 100.70.172.11` 当前 Connected、Relayed，宣告 `192.168.72.0/24`；**路由角色不能证明它是目标 VM 的宿主**。
3. 本机 NetBird CLI/daemon 均 0.81.2。先前只检查用户 known_hosts 不足以判断无法连接：对应版本 strict callback 会先用 daemon 的控制平面公钥验证，再 fallback known_hosts。本机用户文件无目标记录，managed `99-netbird` 文件不存在，但没有因此直接跳过 daemon。实际一次 `--no-browser --strict-host-key-checking=true`、stdin关闭、40秒上限的只读 SSH 返回 exit 1 / host-key verification failed，没有取得任何远端 hostname 或 inventory。原始认证输出只保留在私有临时目录，本目录只保留长度、哈希与白名单结果。
4. `ssh detect peer 22` exit 2 本身不够，因为官方代码在探测失败时也返回 2。独立被动接收该 peer 22 端口首行，实际宣告 `OpenSSH_for_Windows_9.5`，没有 NetBird server 标识；未发送客户端 banner 或认证。banner 是未认证的服务宣告，不是宿主身份凭据。
5. 起初 Node 默认 pipe transport 未取得 gRPC 响应，不能把它当作 `found=false`。使用与官方一致的受保护 `ProtectedPrefix/Administrators/netbird` pipe 和 `TokenImpersonationLevel.Identification` 后，只读 `GetPeerSSHHostKey` RPC 成功：hostname-with-port 查询 `found=false`，remote-IP 查询 HTTP 200 / grpc-status 0 / `found=true`，peer IP/FQDN 正确，返回已绑定的 ssh-ed25519 公钥。原始公钥没有归档。
6. 同一 IP:22 的无认证 `ssh-keyscan -t ed25519` 公钥指纹与 daemon 已绑定公钥明确不同，见下表。**当前 22 端口 key mismatch，不是“控制平面没有公钥”。** 未接纳该 key、未修改 known_hosts、未尝试密码，也没有把验证绕过为成功。未取得原始 SSH 那一次握手所选择算法的 trace；指纹比较是本轮同端点公开 ed25519 key 的独立证据。
7. 官方代码的内部端口是 22022。对同一 peer 的该已知端口只做一次5秒预算的被动连接/读 banner，结果 TimeoutError、0字节；此记录没有分别记录连接阶段与读取阶段，不能断言是哪一阶段超时。没有 keyscan、认证或 metadata 查询，不能称为找到替代管理链路，也不能称为修好22端口。

| 公钥来源 | SHA-256 fingerprint |
| --- | --- |
| daemon 控制平面已绑定 ssh-ed25519 | `SHA256:GtHr4w7pEgUvE1OzeYHh3ttyZqXfnOyYYIl/9LemD50` |
| 当前 peer:22 宣告的 ssh-ed25519 | `SHA256:efthBc3q/F//OYMi+mFAOtIlYWKBCa2+m+JP6QMYmGA` |

## NIC 与睡眠验收范围

`target-readiness-historical.json` 是既有私有采样的白名单摘录（以其原始 UTC 为准），不是本轮刚查询目标 VM：只有承载 IPv4、默认路由与 RDP 管理的 vmxnet3 `Ethernet0`，没有证明独立的第二块 VMware NIC 或网络失效时仍可工作的管理路径。WAN Miniport、Teredo、内核调试器不能算真实双 NIC 切换验收。

`historical-suspend-summary.json` 保留既有实际 S1 结果：30秒 wake timer 被接受，但调用约7,506,030 ms后才返回，因此不能证明自动恢复。仍须遵守 `tests/vnc/windows-vm-suspend.ps1` 的同 UUID、近5分钟外部恢复证明保护；本轮没有绕过。

剩余必要条件是精确 UUID 的宿主映射、可信管理端点及可独立验证的外部恢复路线。NIC 切换还需要切换失效时不依赖该 NIC 的回滚路径。不能用 loopback、代理切换、杀 TCP 或仍依赖唯一 guest NIC 的管理脚本替代这些验收。

## 可审计资料

- `local-host-control.json`：新采集的本机 inventory、UUID 比对、进程/端口、路由、NetBird及失败状态。
- `netbird-strict-attempt.json`、`ssh-detect-22.json`、`ssh-banner-22.json`：真实连接/探测的白名单结果；无认证原始 stdout/stderr。
- `initial-rpc-no-response.json`：保留早期默认 pipe 请求无响应的失败证据。
- `identified-peer-key-rpc.json`、`server-key-comparison-22.json`、`documented-port-22022.json`：识别 pipe RPC、公开 key 比较及端口检查元数据；无原始公钥。
- `source/netbird-peer-key-transport.ps1`、`source/netbird-peer-key-rpc.js`：实际使用的只读适配源码，只调用公开 peer-key RPC，不调用 JWT、GetConfig、登录、设置、电源或网络 RPC，不安装依赖、不修改 daemon，不使用未保护 pipe/TCP 降级。
- `primary-sources.json`：已核对的 **v0.81.2** 官方源码精确链接，包含端口、探测、strict daemon-first 验证、RPC schema、受保护 pipe 与 Identification 约束。
- `summary.json`、`SHA256SUMS.txt`：机器可读结论及全部归档文件校验和。

重新执行源码会查询当时的 daemon 状态；本目录的时间与结果仅对应本次采集。源文件可审阅，尚未把一次性诊断桥作为产品或通用管理工具交付。
