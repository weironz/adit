# 原始 Windows 双客户端 VNC 两小时证据

[Run 39142722306](https://github.com/weironz/adit/actions/runs/39142722306) 已实际completed/success（2/2 jobs）；长测job 117721098430、artifact 12145846836。固定源提交为04df3170dc54ca182d7926fc4b9da0509eae09d6，不是当前main重新跑两小时。未取消、重启或替换原始运行。

实测soak **7240451ms（2小时00分40.451秒）**；probe总elapsed7243593ms。60个无合成输入窗口，各120000–120044ms；窗口后两生产客户端交替点击，各60次、合计120次真实Tk消费。sequence/clicks连续1..120，每次两端均验证新generation、变化RGB标记、完整opaque帧和静态canary。1445条progress，最大相邻间隔5113ms；两端disconnects=[0,0]、max_pending=[1,1]，stderr为空。两客户端实际对应vendor上的两个不同远端端口Established回环连接；不是两个原生Adit GUI进程。

关闭客户端后wrapper实际核验vendor Established=0；probe正常退出、session记录移除；自有vendor与目标进程退出，自有UDP规则从PersistentStore和ActiveStore删除，无超时/cleanup_error。原始probe字段os_socket_cleanup_verified=false仍保留：probe自身不宣称内核关闭ack，socket检查来自wrapper。清理absence来自固定wrapper运行时断言，没有另存独立系统原生快照。

资源是整个probe进程（含两个生产SessionManager）：Windows PeakWorkingSet29777920B（28.40MiB），累计CPU64312ms，≥1秒采样单核等效峰值38.749323%。以CPU增量/总probe elapsed得到约0.887847%单核仅为描述性近似，边界略有不同。vendor末次监督PeakWorkingSet36147200B（34.47MiB）、自启动CPU33218.75ms；Tk目标PeakWorkingSet49188864B（46.91MiB）。PeakWorkingSet是累计峰值，不是当前RSS序列，不能据此排除泄漏；不分摊每客户端，也没有新增资源合格阈值。

manifest.json记录源/run/job/artifact及11个允许文本的下载原始bytes/SHA256、CRLF→LF后Git存储bytes/SHA256。完整ZIP独立下载SHA256为169ec4887674d04e82d5f38bb6f688e4d04653ee86072821a7380ad6121a4feb，已与GitHub API digest一致。只转CRLF为LF，其他内容及空文件保留。fixture.ini、认证/vendor日志未归档。audit.json保存独立逐项审计、精确资源口径和固定源链接。

范围仅为UltraVNC1.8.3.0 portable、Windows Server2025 hosted CI、lossless/native loopback、公共合成桌面、两生产SessionManager的此轮长期共享稳定性。不证明物理输入、OS睡眠/恢复、UAC/锁屏、service模式、弱网、原生GPU呈现、其他产品或当前main的新一轮两小时验收；其他失败/未验证范围不变。
