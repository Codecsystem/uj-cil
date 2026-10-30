# Round 14 全栈性能审计与深度优化报告

## 1. 概述

本轮迭代（Round 14）对 `uj` 全部 7 个核心 Crate（`uj-core`、`uj-sandbox`、`uj-engine`、`uj-checker`、`uj-report`、`uj-tui`、`uj-cli`，约 40k 行 Rust 代码）开展了全栈性能审计与落地重构，完整覆盖：

- **3 项 P0 核心瓶颈优化**：Phase C 精确复测并行化与智能剪枝、Linux `clone3` + `CLONE_VFORK` 零页表复制派生、Linux PID 1 就地精确 CPU/墙钟采样；
- **19 项 P1 热路径架构优化**：边编译边评测流式任务池（`JudgePool::run_streaming`）、DAG 依赖就绪队列释放、全局 LPT 优先队列、`Min` 子任务硬失败提前终止、选手 `stdout` 零拷贝移交、Windows 进程内原生 Win32 ACL、Windows Job IOCP 事件驱动监控、Linux `open_tree`/`mount_setattr` 挂载树克隆与单线程 `poll` 监控、出题 `.mark` 轻量答案标记与输入/校验器持久化缓存、MCP 跨请求长生命周期 `CompileSession`、TUI 64 KiB 缓冲脏标记重绘与 O(1) 状态索引、Linux `systemd-run` 按 `boot_id` 探测缓存；
- **全部 P2 细节优化**（编译负缓存 `ce.txt`、锁外编译器探测与版本持久化、`#include` 传递闭包哈希、字节上限真 LRU 答案缓存、比较器字节相等快速短路与 `memchr` SIMD 行切分、`minijinja` 模板静态缓存等）；
- **7 项正确性与并发安全修复**（Windows `memory_bytes == 0` 误判 MLE、`AppContainerProfile` 并发名称冲突提前删除、Java `-cp <dir>` `.class` 拷入、长会话头文件修改与 Linux 粗粒度时钟 `racy-git` 窗口保护、符号链接防逃逸清理、`ans_state.json` 联动 `uj doctor`、`uj tl` 指纹纳入 `mtime`）。

所有改动严格遵守 `AGENTS.md` 四大硬性准则：**零 `tokio`/async 运行时**、**`stdout` 协议零污染**、**真实沙盒隔离层级不降级**、**Windows + WSL2/Linux 双平台全量测试（含 `evil_tests`）100% 通过**。

---

## 2. 核心架构优化详解

### 2.1 Linux 原生沙盒深度重构 (`crates/uj-sandbox/src/linux/`)

1. **`clone3` 一步入组 + `CLONE_VM | CLONE_VFORK` 消除三次完整页表复制**：
   - 使用 [`clone3`](../../crates/uj-sandbox/src/linux/runner.rs) 携带 `CLONE_INTO_CGROUP | CLONE_PIDFD | CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWNS | CLONE_NEWIPC | CLONE_NEWUTS | CLONE_NEWNET` 直接创建容器内 PID 1 并原子放入目标 cgroup v2（旧内核或受限环境自动优雅回退），彻底省去中间跳板进程与二次 `cgroup.procs` 迁移写入；
   - PID 1 派生选手进程 PID 2 以及宿主派生 `trusted` 辅助进程（校验器/比较器/生成器）改用 32 KiB 16 字节对齐栈上的 `clone(CLONE_VM | CLONE_VFORK | SIGCHLD)`，避免多线程高 RSS 评测进程在 `fork()` 时复制整棵页表与触发写时拷贝缺页中断。
2. **PID 1 就地采样剥离沙盒搭建开销 + Per-FD `memory.peak` 水位重置**：
   - PID 1 在 `vfork` PID 2 紧前与 `wait4(pid2)` 紧后直接读取预打开的 `cpu.stat`（`usage_usec` 差值）与 `CLOCK_MONOTONIC` 墙钟差值，连同原始 `wait4` 状态封装入 `SandboxStatusPacket` 回传宿主，彻底消除命名空间、`pivot_root` 与进程拆除被计入选手耗时的偏差；
   - Supervisor 以 `O_RDWR` 打开 `memory.peak` 并共享同一文件描述符给 PID 1，在派生 PID 2 前写入 `"0\n"` 重置该 FD 的局部峰值水位，兼容 Linux `>= 6.12` 的 per-fd 水位语义；
   - **架构取舍说明**：本轮保留“每轮新建独立子 cgroup + `clone3(CLONE_INTO_CGROUP)` 一步入组 + 100 µs 起始指数退避 `rmdir`”，而未采用跨测试点复用同一槽位 cgroup，以确保在不支持 `memory.peak` 重置的 `< 6.12` 内核上仍具备严格准确的单测试点峰值内存统计，并保持 `cgroup.kill` 的强隔离边界。
3. **`open_tree` + `mount_setattr` 递归只读挂载与单线程 `poll()` 监控**：
   - 优先使用 `open_tree(OPEN_TREE_CLONE | AT_RECURSIVE)` + `mount_setattr(AT_RECURSIVE, MOUNT_ATTR_RDONLY | MOUNT_ATTR_NOSUID)` + `move_mount` 克隆只读系统绑定目录，消除随宿主挂载表膨胀的遍历成本，同时修复传统 `MS_REMOUNT | MS_BIND | MS_REC` 忽略子挂载只读标志的内核陷阱；`/dev/{null,zero,urandom,random}` 保持读写绑定并将 `/dev` 目录重挂为只读；
   - Supervisor 改用单线程 `poll()` 同时监听 `pidfd`、状态管道与非阻塞 `stdout`/`stderr` 管道（0 额外 reader 线程），输出超限（OLE）时在单次 `read` 内立即截断并触发幂等 `cgroup.kill()`；
   - `seccomp-bpf` 过滤器通过 `OnceLock` 预编译缓存，`fork`/`vfork` 后零堆分配直接下发 `prctl(PR_SET_SECCOMP)`；进程级缓存 `which`、`/etc/ld.so.cache`、`/etc/alternatives`、`/dev/null` 与 `rootfs_base_dir`。

### 2.2 Windows 原生沙盒进程内 ACL 与 IOCP 事件驱动 (`crates/uj-sandbox/src/windows/`)

1. **进程内原生 Win32 安全 API 替代 `icacls.exe` 子进程**：
   - 在 [`windows/security.rs`](../../crates/uj-sandbox/src/windows/security.rs) 中直接调用 `GetNamedSecurityInfoW`、`SetEntriesInAclW`、`SetNamedSecurityInfoW` 与 `GetSecurityDescriptorControl` 完成槽位目录 `(OI)(CI)F` 授权、可执行文件 `RX` 授权与退出撤销，配合进程内 `ACL_LOCK` 串行化共享二进制 DACL 更新，且在遍历受保护子项时绝不跟随符号链接/重解析点。
2. **Job Object IOCP 事件驱动监控与死循环忙等修复**：
   - 在 [`windows/process.rs`](../../crates/uj-sandbox/src/windows/process.rs) 与 [`windows/job.rs`](../../crates/uj-sandbox/src/windows/job.rs) 中通过 `JobObjectAssociateCompletionPortInformation` 绑定完成端口，使用 `GetQueuedCompletionStatus` 事件驱动监听 `JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO`、`JOB_OBJECT_MSG_JOB_MEMORY_LIMIT`、`JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT` 与 `JOB_OBJECT_MSG_END_OF_JOB_TIME`；
   - 彻底修复当 `max_processes > 1` 且父进程先于子进程退出时，`WaitForSingleObject(h_process, 5ms)` 立即返回导致的 **100% CPU 空转忙等**；
   - 交互题/通信题 `execute_group` 改用 `WaitForMultipleObjects` 事件唤醒并读取 `GetProcessTimes` 内核 `ExitTime`（100ns 精度 `FILETIME`），消除 5 ms 轮询引入的墙钟与退出顺序量化误差；
   - `NoneSandbox` 同步升级为事件驱动等待（Windows `WaitForSingleObject`，Unix `pidfd_open` + `poll`）；进程级缓存 `SetErrorMode`、`WHICH_CACHE` 与基础环境块，并为 `prepare_binary_content` 引入带 50 ms `racy-git` 窗口的 `CONTENT_HASH_MEMO`。

### 2.3 全局评测池、流式编译调度与缓存引擎 (`crates/uj-engine/` & `crates/uj-core/`)

1. **Phase C 精确复测（`--precise`）并行化与智能剪枝 ([`pool.rs`](../../crates/uj-engine/src/pool.rs))**：
   - 选取互不共享物理核的非 SMT 主逻辑 P 核并行运行精确复测，每个复测槽位独占复用一个沙盒实例与 `box_dir`（彻底消除按提交重复创建 AppContainer 与 `icacls` 的开销）；
   - **提前收敛**：若首次干净重跑仍因 `CpuLimit` 超时且 `time_ms >= TL`，直接停止第 2 次重跑；
   - **提交级与子任务级剪枝**：若单份提交已有 `> 3` 个非边界确定性 TLE，跳过该提交的精确复测；若候选测试点所属 `Min` 子任务已被其它硬错误（WA/RE/MLE 等）归零，复测无法改变得分与总结论，直接跳过。
2. **流式边编译边评测 (`JudgePool::run_streaming`)**：
   - `JudgePool` 新增 `run_streaming(Receiver<PoolSubmission>, &CancelToken)`，在 `uj contest`、`uj judge a.cpp b.cpp ...` 及 MCP 多源码 `judge` 中均由并发编译工作池实时向全局评测池推送就绪提交，消灭“等待最慢编译完成才启动沙盒”的气泡。
3. **依赖就绪释放、队列深度准入与全局 LPT 优先队列**：
   - 子任务存在 `dependencies` 时不再入队阻塞 Worker 线程轮询，而是挂入延迟表，待前置子任务在 `complete_subtask_test` 中全部通过后即时推入就绪堆（前置失败则直接合成 `Skipped` 事件）；
   - 准入控制改为维持就绪堆深度 `< 2 × workers`，就绪队列采用 `BinaryHeap` 按 `(admission_epoch 升序, input_size 降序)` 全局 LPT（Longest Processing Time first）派发，压平长尾；
   - `Min` 计分子任务在常规竞赛模式下遇到 WA/RE/MLE 或 `>= 1.5 × TL` 明显超时导致子任务分值归零时，自动跳过该子任务剩余未跑测试点。
4. **零拷贝输出、真 LRU 答案缓存与编译加速 ([`judge.rs`](../../crates/uj-engine/src/judge.rs), [`compiler.rs`](../../crates/uj-engine/src/compiler.rs))**：
   - `TestCase` 直接固化 `input_size: u64` 字段（消除全局 `Mutex<HashMap>` 锁争用与整表清空抖动）；
   - 选手标准输出采用 `res.stdout.take()` 零拷贝移交比较器，单槽位大输出峰值内存减半；
   - 仅当比较器为 `Builtin` 时才预读并缓存 `.ans`（Testlib/Lemon SPJ 直接传文件路径），答案缓存升级为带字节预算上限的 `BTreeMap` 真 LRU；
   - `BoxLease` 基于 `CLEAN_SLOT_BOXES` 每测试点仅清理一次目录；交互/通信题 `TrustedDirLease` 采用线程局部目录池化复用；
   - `CompileSession` 在锁外并发执行编译器路径与 `--version` 探测并持久化至 `.uj/cache/compiler_versions.tsv`；支持编译失败（`ce.txt`）与 Python `py_compile` 双层缓存；C/C++ 通过 `#include` 传递闭包扫描替代盲扫两层目录全部头文件；仅在编译命令模板实际引用 `{memory_mb}`/`{stack_*}` 时才将对应限制纳入缓存键。

### 2.4 比较器、出题流水线、CLI、MCP 与 TUI (`uj-checker`, `uj-cli`, `uj-tui`, `uj-report`)

- **内置与外部比较器 (`uj-checker`)**：`check_wcmp`、`check_fcmp`、`check_lcmp` 增加去 BOM 后的字节相等快速短路（保留 `ncmp`/`hcmp`/`rcmp`/`yesno` 完整校验语义以防非法答案漏检）；`check_fcmp` 与 `check_lcmp` 采用基于 `memchr` SIMD 的 `SimdLines` 行迭代器；Testlib/Lemon SPJ 将 `stdout` 设为 `Stdio::Null` 省去空管道建立与回收。
- **出题缓存体系 ([`author_cache.rs`](../../crates/uj-cli/src/author_cache.rs))**：`uj ans` 缓存改为 `< 128 B` 的 `.mark` 摘要标记文件，消除 `.ans` 双份落盘；`InputHashCache` 将输入哈希与时间戳持久化至 `.uj/cache/input_hashes.json`，并下放至 Worker 线程并行计算；新增 Validator 成功标记缓存（`.uj/cache/val/<val_hash>/<in_hash>.<group>.ok`）；`uj stress` 每 Worker 复用固定临时文件名；`uj doctor` 先按文件字节大小分桶再并行哈希疑似重复输入。
- **CLI / MCP / TUI / Report**：
  - Linux CLI 启动时若无 `DBUS_SESSION_BUS_ADDRESS` 且无 `$XDG_RUNTIME_DIR/bus` 直接跳过 `systemd-run` 探测，并将成功探测按 `/proc/sys/kernel/random/boot_id` 缓存于 `$XDG_RUNTIME_DIR/uj-systemd-scope-ok`；
  - `uj mcp` 全局复用同一 `CompileSession`，采用 64 KiB `BufWriter` 直接流式序列化 JSON-RPC 响应，工具返回紧凑 JSON 文本并附带 MCP `2025-06-18` 原生未转义 `structuredContent` 对象；
  - 三套 TUI（单提交、多提交 Master-Detail、出题流水线）统一使用 64 KiB `BufWriter<Stderr>`、`dirty` 按需重绘（仅在事件/按键/窗口变化/250ms 时钟跳变时渲染）与 O(1) `HashMap` 索引；`SystemMonitor` 改为仅按评测机进程树 PID 增量刷新（每 3 个周期做一次子进程发现）。

---

## 3. 顺带修复的 7 项正确性与并发安全问题

| 编号 | 问题说明 | 修复方案与位置 |
| :---: | :--- | :--- |
| **FIX-01** | Windows `limits.memory_bytes == 0`（不限内存）时，`peak > limits.memory_bytes` 缺少 `> 0` 判断导致立即误判 `MLE` | 在 [`windows/process.rs`](../../crates/uj-sandbox/src/windows/process.rs) 单进程与进程组全部内存判定处补齐 `limits.memory_bytes > 0` 守卫，并新增 `test_memory_limit_zero_is_unlimited` 回归测试 |
| **FIX-02** | `AppContainerProfile::create_isolated` 与 `create_isolated_for_slot` 共用 `uj.sandbox.<pid>.<N>` 名称空间，并发 `Drop` 会提前 `DeleteAppContainerProfile` 导致活跃槽位 `CreateProcessW` 报 `0x80070002` | 在 [`windows/security.rs`](../../crates/uj-sandbox/src/windows/security.rs) 拆分为 `uj.sandbox.<pid>.iso.<N>` 与 `uj.sandbox.<pid>.slot.<slot>.<N>`，同时保持 `parts[2] == pid` 兼容僵尸 Profile 回收 |
| **FIX-03** | Java 编译产物含 `-cp <dir>` 时，`build_argv` 误将目录当作单文件 `fs::copy` 静默失败却改写为沙盒路径 | 在 [`judge.rs`](../../crates/uj-engine/src/judge.rs) 跳过 `-flag` 参数，遇到目录参数时将其中 `.class` 文件完整暂存至 `box_dir` 并重写为 `.`，新增 `test_build_argv_java_classpath_dir_stages_classes_into_box` |
| **FIX-04** | 长生命周期 `CompileSession`（如 MCP）`mtime_precheck` 仅检查主源文件且未防护 Linux 粗粒度时钟同 tick 覆写 | 在 [`compiler.rs`](../../crates/uj-engine/src/compiler.rs) 与 [`author_cache.rs`](../../crates/uj-cli/src/author_cache.rs) 记录完整依赖集时间戳，并统一实施 `racy-git` 窗口规则（子秒级 50 ms / 整秒级 2 s） |
| **FIX-05** | `make_writable_recursive` 与 `clean_box_dir_contents` 跟随符号链接，恶意程序可通过软链诱导宿主修改外部权限 | 在 [`uj-sandbox/src/lib.rs`](../../crates/uj-sandbox/src/lib.rs) 全程采用 `symlink_metadata()` / `DirEntry::file_type()`，遇到符号链接直接解除链接而不深入或 `chmod` 目标 |
| **FIX-06** | `uj doctor` 读取 `.uj/cache/ans_state.json` 检查过期答案，但流水线从未写入该文件 | 在 [`author_steps.rs`](../../crates/uj-cli/src/author_steps.rs) 与 [`author_cmd.rs`](../../crates/uj-cli/src/author_cmd.rs) 生成答案后原子合并更新 `ans_state.json` |
| **FIX-07** | `uj tl` 测速缓存指纹仅拼入 `name + subtask + size`，同字节长度重新生成输入会误复用旧耗时 | 在 [`author_cmd.rs`](../../crates/uj-cli/src/author_cmd.rs) `compute_tests_fingerprint` 中纳入输入文件 `mtime` 纳秒时间戳 |

---

## 4. 双平台 Release 实测性能基准 (Benchmarks)

测试环境：同一宿主机 **Windows 11 (MSVC `--release`)** 与 **WSL2 / Linux (GNU `--release`, `systemd-run --user --scope -p Delegate=yes`)**。`perf_bench` 中比较器基准均已修正为**独立堆内存缓冲区**（`assert_ne!(out.as_ptr(), ans.as_ptr())` + `black_box`），消除同切片指针短路偏差。

### 4.1 沙盒原语微基准 (`windows_perf_tests` & `linux_perf_tests`)

| 平台与测试项 | 优化前基线 | Round 14 实测结果 | 核心收益 |
| :--- | :--- | :--- | :--- |
| **Windows AppContainer 目录授权** `grant(OI\|CI F)` | 15 ~ 50 ms (`icacls.exe`) | **0.61 ~ 0.93 ms** | **20x ~ 80x 提速**（进程内 Win32 DACL API） |
| **Windows AppContainer 权限撤销** `revoke` | 15 ~ 50 ms (`icacls.exe`) | **0.42 ~ 0.72 ms** | **25x ~ 110x 提速**（消除槽位销毁进程风暴） |
| **Windows 可执行文件授权** `grant(RX)` | 15 ~ 50 ms (`icacls.exe`) | **0.18 ~ 0.34 ms** | **50x ~ 250x 提速** |
| **Windows `Tier::Full` 空原生程序单次运行** | ~25+ ms (5ms 轮询) | **14.50 ms** (30 次均值) | Job IOCP 事件驱动零轮询延迟 |
| **Linux `Tier::Full` 非信任 `/bin/true` 单次运行** | 多次 `fork` + 遍历重挂载 | **mean 3.45 ms / p50 3.42 ms** (200 次) | `clone3(CLONE_INTO_CGROUP)` + PID 1 `vfork` + `open_tree` |
| **Linux `Tier::Full` 空程序上报 CPU / 墙钟时间** | 混入 0.5~3 ms 沙盒开销 | **CPU 515 µs / Wall 450 µs** | PID 1 就地采样，彻底剥离沙盒搭建与拆除开销 |
| **Linux `trusted` 辅助进程 `/bin/true` 单次运行** | 完整 `fork` 页表复制 | **mean 0.457 ms / p50 0.440 ms** (200 次) | `CLONE_VM \| CLONE_VFORK` 亚毫秒级派生 |

### 4.2 全链路与组件基准 (`cargo run --release --example perf_bench`)

| 基准测试项目 | Round 11 基线 | Round 14 Windows 实测 | Round 14 WSL2/Linux 实测 | 提升幅度与说明 |
| :--- | :--- | :--- | :--- | :--- |
| **PatternSet 子任务匹配 (10k items)** | 328 µs | **362 µs** | **260 µs** | 相比未预编译 (508~848 ms) **1,957x ~ 2,344x** |
| **Checker `wcmp` 100k 词（独立同内容缓冲）** | 2,025 µs | **45 µs** | **26 µs** | **45.0x ~ 77.9x 提速**（字节相等快速短路） |
| **Checker `wcmp` 100k 词（空白差异 SWAR）** | 2,025 µs | **2,308 µs** (独立缓冲) | **2,124 µs** (独立缓冲) | 消除自别名 L1 假热后仍达 **47M 词/秒** |
| **Checker `fcmp` 100k 行（独立同内容缓冲）** | 3,242 µs | **168 µs** | **124 µs** | **19.3x ~ 26.1x 提速**（字节相等快速短路） |
| **Checker `fcmp` 100k 行（行尾异空白 `SimdLines`）** | 3,242 µs | **1,381 µs** | **1,019 µs** | **2.35x ~ 3.18x 提速**（`memchr` SIMD 换行扫描） |
| **Checker `ncmp` 100k 整数（独立异空白缓冲）** | 2,387 µs | **2,892 µs** | **2,721 µs** | 保留完整溢出与格式校验（无不安全字节短路） |
| **Checker `hcmp` 100k 大整数（带前导 `+000`）** | 3,540 µs | **3,826 µs** | **3,624 µs** | 保留完整高精度十进制语法校验 |
| **TUI `MultiApp` 40,000 事件分发 (`100×200`)** | $O(T^2)$ 线性扫描 | **7,646 µs** (7.65 ms) | **4,329 µs** (4.33 ms) | 单事件仅 **108 ~ 191 ns**（`HashMap` $O(1)$ 索引） |
| **端到端评测 100 测试点 (`-j 4`, `Tier::Full`)** | — | **526 ms** (5.26 ms/点) | **840 ms** (`/mnt/e` 9P 盘) | 100 点全 `Accepted` 且真实 `Tier::Full` 隔离 |
| **出题热缓存 `uj validate` / `uj ans`** | 每次全量重算哈希与校验 | **76 ms / 66 ms** | **185 ms / 215 ms** | `.mark` + `input_hashes.json` + Validator 标记命中 |
| **出题对拍 `uj stress -n 20` (60 次沙盒运行)** | 每轮建删临时文件 | **383 ms** | **589 ms** | Worker 固定槽位文件复用 + 会话级编译复用 |

---

## 5. 验证结果汇总

1. **格式与静态检查**：Windows 与 WSL2 双平台 `cargo fmt --check` 及 `cargo clippy --workspace --all-targets -- -D warnings` 全部 **0 警告**通过。
2. **Windows 全量测试**：`cargo test --workspace` 共 **248 项测试 100% 通过**（含 13/13 `evil_tests`、4/4 + 2/2 `windows_perf_tests`、26/26 `pipeline_tests`、17/17 `regression_tests`、8/8 `contest_tests`、9/9 MCP 集成测试）。
3. **WSL2 / Linux 全量测试**：`systemd-run --user --scope -p Delegate=yes cargo test --workspace` 共 **260 项测试 100% 通过**（含 13/13 `evil_tests`、7/7 + 1/1 `linux_perf_tests`、14/14 `sandbox_tests`、13/13 `uj-sandbox` 单元测试）。
