# uj 架构与技术决策记录 (ADR / Decisions)

本文档记录 `uj` 开发过程中的核心设计决策与权衡考量。

| # | 决策项 | 结论 | 理由/权衡 |
|---|---|---|---|
| 1 | 项目名与二进制名 | `uj` | 简短好记，命令行输入极速，工作区命名为 `uj-cil`。 |
| 2 | Linux 默认运行方式 | 免 root + systemd 委派；root 作为 Full(root) 可选 | 竞赛选手和 Agent 出题环境通常为非 root 普通用户，通过 cgroup 委派配合 userns 即可实现完整的强隔离。 |
| 3 | Windows 内存口径 | 提交内存 commit (`PeakJobMemoryUsed`) | Windows 竞赛传统以 commit 为主，可有效防范内存预分配后突发写入造成的 OOM。可在配置中切换。 |
| 4 | PE 是否合并为 WA | 默认合并 (`pe_as_wa = true`) | 符合现代竞赛（Codeforces/AtCoder/IOI）主流规范，保留配置选项供 Lemon 等传统场景使用。 |
| 5 | 异步运行时选型 | **禁止引入 tokio**，全采用 std 线程 + crossbeam-channel | 子进程监控是阻塞式的系统调用，线程池模型更可控、更低开销、不污染调用栈，无 async cancel 安全隐患。 |
| 6 | Linux 最低内核版本 | **严格要求 ≥ 5.19** | 强依赖 `memory.peak` (5.19)、`cgroup.kill` (5.14)、`subset=pid` (5.8) 及 pidfd。拒绝维护繁琐脆弱的旧内核兼容分支。 |
| 7 | 输出流重定向策略 | 必须通过 pipe 异步并发读取 stdout/stderr | 若直接重定向至 tmpfs/文件，文件页缓存 (page cache) 会全额计入内存 cgroup 导致内存虚高；同时避免单向缓冲区写满死锁。 |
