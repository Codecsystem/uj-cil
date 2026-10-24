# AGENTS.md — uj 系统 Agent 研发与维护指南

> 本文档专为在此代码库上工作的 **AI 编程助手（AI Coding Agents）**、自动化出题 Agent 及核心架构维护者编写。
> 当需要理解系统约束、修改代码、排查沙盒漏洞或扩展功能时，**必须首先通读本文档**。

---

## 1. 核心设计哲学与硬性架构约束 (Hard Invariants)

在对本项目做任何修改时，必须严格遵守以下硬性准则：

1. **绝对禁止引入 `tokio` 或其它 async 运行时**：
   - 进程监控、沙盒状态收集、管道流转、信号处理均为深度的阻塞式系统调用（如 `wait4`, `pidfd_open`, `WaitForMultipleObjects`）。
   - 全系统统一采用 `std::thread` + `crossbeam-channel` + `parking_lot` 线程池模型，更可控、更低系统开销、不污染调用栈，彻底杜绝 async cancel 遗留孤儿进程的隐患。
2. **标准输出（stdout）零污染原则**：
   - 凡是提供结构化结果的场景（`--format json`, `--format ndjson`, `uj mcp`），`stdout` 只能输出纯粹合法的协议数据。
   - 所有调试、诊断、告警、日志输出（包括 `eprintln!`, `tracing`）**必须严格流向 `stderr`**。
3. **真实隔离层级，严禁虚标 (No Fake Sandbox Tier)**：
   - 当请求的沙盒功能在当前环境不支持时，必须显式报错（若指定 `--require-tier full`）或降级为真实的 `Degraded` / `None`，并正确标记在 `RunResult.sandbox_tier` 中。严禁对外假冒更高层级的沙盒。
4. **双平台对等测试覆盖**：
   - 修改任何跨平台逻辑（特别是 `crates/uj-sandbox` 和 `crates/uj-engine`）后，**必须在 Windows（MSVC）与 WSL2/Linux 双环境下均通过 `cargo test --workspace`**。

---

## 2. 代码库模块划分与职责地图

```
uj-cil/
├── Cargo.toml                 # Workspace 根配置文件（Rust Edition 2024）
├── crates/
│   ├── uj-core/               # [核心模型] Verdict, Limits, RunSpec, RunResult, 题目配置, 计分
│   │   ├── src/config.rs      # problem.toml 解析与子任务拓扑解析
│   │   ├── src/run.rs         # 评测运行规格与结果结构体 (ExitStatus, KillReason, Tier)
│   │   └── src/verdict.rs     # 判题结论优先级与计分逻辑
│   ├── uj-sandbox/            # [沙盒实现] 平台隔离驱动与环境健康探测
│   │   ├── src/linux/         # Linux 原生沙盒
│   │   │   ├── cgroup.rs      # cgroup v2 目录创建、限制写入、peak/stat 采集与 cgroup.kill
│   │   │   ├── runner.rs      # 命名空间隔离、pivot_root、匿名状态管道、seccomp-bpf、init 进程
│   │   │   └── seccomp.rs     # 纯 Rust BPF 规则编译 (阻断 x32 ABI 与危险系统调用)
│   │   ├── src/windows/       # Windows 原生沙盒
│   │   │   ├── job.rs         # Job Object 限制与 Commit 内存采集
│   │   │   ├── process.rs     # 挂起派生、句柄列表限定 (PROC_THREAD_ATTRIBUTE_HANDLE_LIST)
│   │   │   └── security.rs    # AppContainer Profile 独占创建、ACL 目录赋权与清理
│   │   └── src/none.rs        # 无隔离回退实现 (Tier::None)
│   ├── uj-engine/             # [评测调度] 多核调度器、编译器、大小核绑定
│   │   ├── src/compiler.rs    # 多语言编译驱动与增量缓存 (支持按 hash 并发互斥)
│   │   ├── src/judge.rs       # 题目评测主循环 (标准题、交互题、通信题、提交答案题)
│   │   ├── src/scheduler.rs   # DAG 依赖拓扑驱动的线程池评测调度器
│   │   ├── src/topology.rs    # CPU 大小核识别与 P 核物理槽位独占绑定
│   │   ├── src/pool.rs        # [Round 9] 全局多提交评测工作池与任务窃取 (JudgePool)
│   │   ├── src/budget.rs      # [Round 9] 物理内存动态限额与预算管控 (MemoryBudget)
│   │   └── src/context.rs     # [Round 9] 跨提交共享的 ProblemContext 与 SubmissionContext
│   ├── uj-checker/            # [校验比较] 零拷贝比较器与第三方协议适配
│   │   ├── src/builtin.rs     # mmap + memchr 内置快速比较器 (exact, wcmp, fcmp 等)
│   │   ├── src/testlib.rs     # Testlib XML/Appes 协议适配与 SPJ 执行
│   │   └── src/lemon.rs       # Lemon/LemonLime 传统比较器适配
│   ├── uj-report/             # [报表生成] 格式化导出
│   │   ├── src/html.rs        # minijinja HTML 离线报告渲染
│   │   ├── src/csv.rs         # CSV 格式导出
│   │   └── src/schema.rs      # JSON Schema 定义与导出
│   ├── uj-tui/                # [终端交互] Ratatui 实时监控看板
│   │   ├── src/multi.rs       # [Round 9] 多提交 Master-Detail 监控看板
│   │   └── src/pipeline.rs    # [Round 10] 出题流式执行三进度条与用例网格看板
│   └── uj-cli/                # [命令行界面] 命令分发、流水线、MCP Server、Doctor 诊断
│       ├── src/cli.rs         # Clap 派生参数定义
│       ├── src/author_cmd.rs  # 出题自动化流水线 (build -> gen -> validate -> ans -> verify -> stress)
│       ├── src/author_steps.rs# [Round 10] 流水线用例级细粒度串联执行与步骤抽象
│       ├── src/contest_cmd.rs # [Round 9] 比赛批量多提交评测与全局池调度
│       ├── src/judge_cmd.rs   # 独立评测与批量评测命令调度
│       ├── src/mcp_server.rs  # Model Context Protocol stdio 交互服务端
│       └── src/doctor.rs      # 宿主系统环境健康检查
│       └── tests/             # 集成测试套件 (authoring, contest, evil, mcp, pipeline, regression, report)
├── tests/
│   ├── evil/                  # 恶意对抗测试源码 (防作弊、防逃逸、防死锁、资源耗尽)
│   ├── evil_seed/             # 种子数据与随机用例
│   ├── problems/              # 端到端测试题目套件 (传统、交互、通信、子任务依赖)
│   └── regression/            # 回归测试固化用例
└── docs/                      # 文档索引 (README.md) 与分类库 (specs/, plans/, reviews/, opt/)
```

---

## 3. 关键机制与历史踩坑备忘 (Gotchas & Lessons Learned)

在维护或修改沙盒和执行流时，以下是历次审查（Review 1~5）总结出的最高频隐患点：

### 3.1 Linux PID 1 信号免疫与状态管道通信
- **机制**：在独立的 Linux PID 命名空间中，容器内的首个进程（PID 1）由内核赋予信号免疫保护；如果选手进程（PID 2）崩溃引发信号（如 SIGSEGV / SIGABRT），PID 1 无法通过 `kill(getpid(), sig)` 杀死自身，否则信号会被内核忽略，被迫以退出码 `128+sig` 退出，导致外层误报为普通非零退出码。
- **规范**：PID 1 必须通过单向匿名管道（`pipe2` 附带 `O_CLOEXEC`）向宿主 supervisor 发送 `SandboxStatusPacket` 结构体，直接回传原始 `wait4` 的 `status` 以及 `rusage`；宿主据此解码出原生的 `ExitStatus::Signaled(sig)`。

### 3.2 Linux `/tmp` 循环 Bind Mount 错误 (`EINVAL` / os error 22)
- **机制**：如果将 `work_dir` 设在 `/tmp` 下（例如单元测试中的临时目录），沙盒容器如果在 `work_dir.join(".uj_rootfs")` 创建 rootfs，又试图将 `work_dir` bind mount 到 `rootfs/box`，会导致将祖先目录挂载至其自身后代，触发 Linux 内核返回 `EINVAL`。
- **规范**：当 `work_dir` 位于 `/tmp` 下时，rootfs 临时根目录必须重定向放置在 `/dev/shm/.uj_rootfs_<slot>` 下，确保挂载路径完全树状解耦。

### 3.3 Windows AppContainer 目录 ACL 与单例 SID
- **机制**：AppContainer 是基于 SID 的沙盒，绝对不能使用系统级通配的 `ALL APPLICATION PACKAGES`（否则并发槽位之间可互相读取私有输入数据与答案）。
- **规范**：每个评测槽位（slot）创建独立的 AppContainer Profile，仅为当前槽位的私有目录（`.uj/tmp/runs/slot-N/`）授权读写。评测结束后，彻底清理临时目录。`AppContainerProfile` 在多线程环境下需使用引用计数或单例共享机制，防止提前 drop 导致正在运行的 `CreateProcessW` 遭遇 `use-after-free`。

### 3.4 Windows Defender 冷启动扫描阻滞自适应
- **机制**：在 Windows 平台上，编译器新生成的可执行文件首次被 `CreateProcessW` 启动时，常被 Windows Defender 驱动同步扫描拦截 3~7 秒，导致原本仅需 0.1s 的程序因墙钟时间超时被误杀。
- **规范**：沙盒检测到因 `WallLimit` 被杀但实际 CPU 时间极小（`< 10% TL`）的首次运行用例，判定为 Defender 扫描阻滞，自动进行单次重跑并放宽墙钟上限（保持 CPU 限制不变），以保障评测结论准确。

### 3.5 systemd-run 探测静音与预判
- **机制**：在 Linux 无 systemd user bus 的环境（如容器或轻量 SSH 登录）下，盲目调用 `systemd-run` 会向 stderr 打印连接失败日志，干扰自动化测试。
- **规范**：先检查当前进程 `/proc/self/cgroup` 是否已处于有效 `.scope` 内；若已在 scope 内则直接跳过探测；若需探测，必须将探测子进程的 `stdout` 和 `stderr` 重定向至 `Stdio::null()`。

---

## 4. 测试与验证工作流 (Verification Protocol)

任何功能开发或 Bug 修复完成后，必须按顺序执行以下验证动作：

### Step 1: 格式与静态检查
```bash
# 格式化检验
cargo fmt --check

# 代码质量检查
cargo clippy --workspace --all-targets -- -D warnings
```

### Step 2: 本地平台完整测试
```bash
# Windows 环境 (PowerShell)
cargo test --workspace

# Linux / WSL2 环境 (Bash)
source ~/.cargo/env
cargo test --workspace
```

### Step 3: 对抗用例特别验证 (`crates/uj-cli/tests/evil_tests.rs`)
对抗测试套件覆盖了以下严苛攻击与边界场景，必须确保 100% 绿色通过：
- `test_evil_cheat_read_blocked`: 尝试跨目录偷读标准答案（必须阻断）。
- `test_evil_cheat_write_blocked`: 尝试篡改题目数据（必须阻断）。
- `test_evil_crashes_and_re`: 异常除零、段错误、abort（必须上报真实 signal/RE，不能卡死）。
- `test_evil_timeouts`: 真实死循环、睡眠偷时间（精确触发 TLE）。
- `test_evil_mle`: 静态大数组、动态耗尽内存（精确触发 MLE）。
- `test_evil_ole`: 海量输出（精确触发 OLE，不耗尽物理磁盘）。
- `test_evil_linux_seccomp`: 尝试提权、clone 命名空间、x32 越界系统调用（必须拦截）。

---

## 5. 面向 Agent 的 CLI 与 MCP 使用指南

AI Agent 在调度 `uj` 完成出题或批量评测任务时，推荐使用以下调用模式：

### 5.1 获取机器可读的评测结果
```bash
# 评测单个代码，输出 NDJSON 实时事件流
uj judge solution.cpp -p . --format ndjson

# 评测并获取完整 JSON 结果对象
uj judge solution.cpp -p . --format json
```

### 5.2 获取数据结构的 JSON Schema
```bash
# 查看评测输出结构的 JSON Schema，便于代码生成时对照
uj schema result
uj schema config
uj schema events
```

### 5.3 启动 MCP stdio 服务
```bash
# 启动 MCP 协议服务端，接收标准 JSON-RPC 2.0 请求
uj mcp
```
MCP 服务端对外暴露的工具包含：
- `doctor`: 诊断宿主环境与题目健康度。
- `init_problem`: 初始化题目目录骨架（支持 `type`、`name`、`title`、`force`）。
- `build`: 编译指定或全部题目组件。
- `verify`: 批量校验题目中声明的各类解法结论。
- `pipeline`: 一键执行出题全生命周期流水线。
- `judge`: 评测指定源码并回传评测指标。
- `stress`: 启动自动化对拍找反例。
- `show`: 深入查看某次运行或特定测试点的输入输出差异。
- `contest`: 批量运行比赛包评测并生成排行榜。

> 注：详细 Agent 工作流与参数说明请参阅 [docs/specs/AGENT_GUIDE.md](docs/specs/AGENT_GUIDE.md)。

