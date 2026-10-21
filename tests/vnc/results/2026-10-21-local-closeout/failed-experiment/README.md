# 未完成的合成 GDI 图案实验

`clear-pattern-controller.json` 是已有原始失败记录，保持 `outcome: failed`、`process_identity: null`、`owned_process_stopped: false` 和 `codec_coverage_claimed: false`。

配置中的 60 秒时长与 350 ms 间隔只是请求参数。该记录没有有效的目标进程身份，不能证明合成图案成功运行 60 秒，不能证明取得了有效的新 RDP 流，也不能用来补充 ClearCodec 缓存命中或服务端实际发送的编解码覆盖证据。

本目录只保存失败 controller JSON、说明及独立 SHA-256 清单，没有截图、真实桌面、视频、二进制、网络捕获或密码。没有为补证新建环境或复跑 VM。主验收结果和已有 codec 回放证据应在各自记录中独立判断，不使用此失败实验替代。
