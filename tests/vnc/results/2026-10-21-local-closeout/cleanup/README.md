# 本轮最后清理与控制连接确认

`result.json` 是主控保存的实际现场元数据，保留两个阶段，不将租约执行等同于目录已经不存在：

1. `closeout_readonly`：准确目录 `C:\ProgramData\AditVncKeys-c11d36edff21422f8fd01acb1b637803` 仍存在，但 lease 已使 share/tasks/processes 均为 0。
2. `closeout_directory_removed`：主控确认检查 owner 归属及 reparse point 后，精确删除此目录；同一路径的最终 `absent=true`。

JSON 保存存在/资源计数和移除后 absent 的实际观察，未包含 owner 的原始字节或 reparse 检查全文；这些操作前置检查由主控确认，不冒充本目录能够独立重放的 VM 操作。没有修改原服务、账户或网络。

`control-final.json` 单独记录最后一次控制 driver **PID 54084、exit 0、helper exit 0**。来源是主控确认：发出 CLOSE 后，新连接对应后一个 helper_exit=0。累计 cap/stderr 含更早的 `probe_deadline expired`，因此没有归档混合日志，也不把旧 probe 提示归为这次新连接出错。

两份 JSON 均纳入上级 `SHA256SUMS.txt` 与 `provenance.json`。本轮归档没有连接 VM、重建夹具或重新执行清理。
