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
| 8 | Windows AppContainer 单例生命周期 | `AppContainerProfile::get_shared()` 配合 `OnceLock` 跨线程安全共享 | 避免并发测试或多评测槽位时由于局部 drop 提前 `FreeSid` 导致其他线程在 `CreateProcessW` 时踩中 use-after-free / `0x80070002` 错误。 |
| 9 | Windows 沙盒 Auto 模式优雅降级 | Full 层级优先，对系统管理工具 (如 cmd/powershell) 降级至 Job Object (Degraded) | Windows 系统策略禁止 AppContainer 启动未打包的控制台管理外壳，而在 Auto 模式下平滑降级到 Job Object 能保障任意命令都能受控执行，显式 `--require-tier` 则严格阻断。 |
| 10 | Clap 全局参数命名防冲突 | 子命令参数与全局参数去同名化（如 `import` 使用 `kind` 替代全局 `format`） | Clap 在解析全局参数同名子命令参数时存在 downcast 强断言冲突，显式区分命名确保结构安全。 |
| 11 | LemonLime 子任务依赖标志解析 | 解析 `inputFiles` 中的 `*_lemon_SUbtaskDEPENDENCE_fLAg` 后缀提取依赖 ID | LemonLime CDF 在 JSON 中采用特殊的字符串后缀编码子任务依赖关系，通过解析剥离保障完全向前兼容。 |
| 12 | 评测报告提交者命名口径 | 统一以 `submission_name`（选手-题目或源码文件名）记入报告 | 避免临时编译二进制文件名（如 `prog.exe` 或 `ac.exe`）覆盖真实提交者名称，保障报表清晰。 |
| 13 | stdio MCP Server 零标准输出污染 | 内部模块直接以 Rust 原生结构体形式调用，禁止向 stdout 输出日志 | MCP 协议通信强依赖标准输出管道传递纯粹的 JSON-RPC 2.0 数据包，通过结构化直调避免破坏流协议。 |
