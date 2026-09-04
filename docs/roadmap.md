# 路线图

**问题跟踪已迁移到 GitHub Issues**（为什么是这次反转，见
[decisions.md #21](decisions.md#21-issue-tracking-moved-to-github-issues--reversal-of-the-two-file-roadmap-20)）。
这份文件不再维护清单，只做导航。排序依据没变——"什么挡在 Adit 和别人能用上它
之间"，而不是哪件事更有意思。

## 待办在哪里

- 全部待办：[Issues](https://github.com/weironz/adit/issues)
- 卡在别人手上：[`blocked-external`](https://github.com/weironz/adit/issues?q=label%3Ablocked-external)
- 需要真机，而不是继续写代码：[`needs-hardware`](https://github.com/weironz/adit/issues?q=label%3Aneeds-hardware)
- 推广：[`growth`](https://github.com/weironz/adit/issues?q=label%3Agrowth)
- 值得补上的已知缺口：[`known-gap`](https://github.com/weironz/adit/issues?q=label%3Aknown-gap)
- 考虑过，不做：[`wontfix`](https://github.com/weironz/adit/issues?q=label%3Awontfix)（closed，为什么不做的推理在正文里）

已完成的工作按版本归档，见 [roadmap-done.md](roadmap-done.md)。

## 工作方式

- 新发现的问题或想法：`gh issue create`，按上面四类打 label。
- 做完：`gh issue close <n> --reason completed`；随版本发布时挂对应 milestone。
- 决定不做：`gh issue close <n> --reason "not planned"`——把为什么不做的推理
  写在正文或评论里再关。一个决定背后的推理，比决定本身活得更久。
