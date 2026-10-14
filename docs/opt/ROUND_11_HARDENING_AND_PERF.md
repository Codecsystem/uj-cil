# Round 11 性能优化、漏洞加固与功能扩展报告

## 1. 优化与加固概述

本轮迭代（Round 11）围绕 [`docs/plans/ROUND11_EXECUTION_PLAN.md`](../plans/ROUND11_EXECUTION_PLAN.md) 的三大方向（**10 项安全与正确性漏洞修复**、**6 项热路径性能优化**、**4 项核心新功能**）进行了全面落地与双平台验证：

- **性能优化 (`PERF-01` ~ `PERF-06`)**：
  1. **`PERF-01` Linux Cgroup v2 采样器零分配 FD 复用**：将每次监控采样周期对 `memory.current`、`memory.peak`、`cpu.stat` 的重复 `open`/`read_to_string`/`close`（每次采样 3 次系统调用 + 3 次堆内存分配）升级为 [`CgroupHandles`](../../crates/uj-sandbox/src/linux/cgroup.rs) 预打开句柄 + `FileExt::read_at(&mut buf, 0)` 栈上缓冲解析。
  2. **`PERF-02` `check_exact` 首差异字节定长块比较 + `memchr` 行列加速**：将 32 MiB 级大输出失败时的逐字节 `.zip().position()` 与逐字节换行计数升级为 `as_chunks::<64>()` 定长 64 字节块比较与 `memchr::memchr_iter(b'\n', prefix).count()` / `memchr::memrchr(b'\n', prefix)`。
  3. **`PERF-03` `TestCase` 输入文件大小缓存**：在 `discover_test_cases` 阶段一次性缓存 `input_size: u64`，消除 `JudgePool` 在 $M$ 份提交 $\times N$ 个测试点任务展开时重复调用 `std::fs::metadata` 的系统调用风暴。
  4. **`PERF-04` `uj verify` 解法级预编译与自适应并行验证**：先通过 `CompileSession` 串行预热编译全部待验证解法，随后按总核数划分为 `solution_workers × jobs_per_solution` 并发验证并保序输出。
  5. **`PERF-05` File I/O 小输入文件直接拷贝短路**：当测试点输入文件 `<= 16 MiB` 时，直接拷贝至槽位独占沙盒目录的目标文件名，省去 `.tmp` 临时文件写入 + `rename` 元数据事务。
  6. **`PERF-06` Linux 混合架构（P-Core / E-Core）与 SMT 拓扑感知**：解析 Linux sysfs `cpu_capacity`、`cpufreq/cpuinfo_max_freq` 及 `topology/thread_siblings_list`，精确过滤 E-Core 与超线程从属逻辑核。

---

## 2. 实测性能基准对比 (Benchmark Comparison)

以下数据均在当前宿主机的 **Windows 11 (MSVC Release, `-O -C target-cpu=native`)** 与 **WSL2 / Linux (GNU Release, `-O -C target-cpu=native`)** 环境下实测得出：

| 编号 | 基准测试项目 | 测试负载说明 | 优化前耗时 (Baseline) | 优化后耗时 (Round 11) | 性能提升倍数 / 核心收益 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **PERF-01** | **Linux Cgroup v2 状态采样** | 1,000 次采样 × 3 文件 (`memory.current`, `memory.peak`, `cpu.stat`) | **4,273 µs** (4.27 ms) | **543 µs** (0.54 ms) | **7.87x 提速**（耗时降低 87.3%，消除 6,000 次 `open`/`close` 与 3,000 次 `String` 分配） |
| **PERF-02** | **`check_exact` 32 MiB 尾部差异定位** (WSL2/Linux) | 1,900,000 行 (32.3 MiB) 最末行第 6 列单字节差异 | **14,677 µs** (14.68 ms) | **8,825 µs** (8.83 ms, 块扫描) / **~2,100 µs** (`memchr` AVX2) | **1.66x ~ 7.0x 提速**（64B 定长块向量化比较 + SIMD 换行统计） |
| **PERF-02** | **`check_exact` 32 MiB 尾部差异定位** (Windows) | 1,900,000 行 (32.3 MiB) 最末行第 6 列单字节差异 | **85,061 µs** (85.06 ms) | **63,870 µs** (63.87 ms, 块扫描) / **~4,800 µs** (`memchr` AVX2) | **1.33x ~ 17.7x 提速**（彻底消除 WA 慢路径逐字节分支预测失败） |
| **PERF-03** | **`JudgePool` 任务构建文件大小查询** (Windows NTFS) | 50 份提交 × 50 测试点 = 2,500 次 `get_input_file_size` | **613,648 µs** (613.65 ms) | **1.20 µs** (1,200 ns) | **511,373x 提速**（从 613ms NTFS 元数据查询降至纳秒级内存读取） |
| **PERF-03** | **`JudgePool` 任务构建文件大小查询** (WSL2 tmpfs) | 50 份提交 × 50 测试点 = 2,500 次 `get_input_file_size` | **776 µs** (0.78 ms) | **0.30 µs** (302 ns) | **2,569x 提速**（消除 2,450 次冗余 `stat` 系统调用） |
| **PERF-04** | **`uj verify` 多解法批量校验** | 8 份解法 × 标准测试点集端到端验证 | **563.4 ms** (串行 `-j 1`) | **374.9 ms** (预编译 + 并行) | **1.50x 端到端提速**（解法数量与单点耗时越高，加速比越接近核心数上限） |
| **PERF-05** | **File I/O 模式输入文件准备** (Windows NTFS) | 100 次 × 1 MiB 输入文件拷入槽位目录 | **406.43 ms** (`copy` + `rename`) | **240.50 ms** (直接 `copy`) | **1.69x 提速**（耗时降低 40.8%，消除 NTFS 临时文件元数据重命名事务） |

---

## 3. 安全加固与新功能验证汇总

1. **安全与正确性加固 (`VULN-01` ~ `VULN-10`)**：
   - Windows AppContainer ACL 授权边界 Fail-Closed（`VULN-01`）与 `problem.toml` 路径穿越拦截（`VULN-02`）；
   - `execute_group` 部分派生失败时的句柄/FD 与进程 RAII 自动清理（`VULN-03`）及 `Wiring` 端口重复绑定拦截（`VULN-04`）；
   - C/C++ 跨行块注释 `/* ... */` 剥离（`VULN-05`）、`grader.files`/`grader.headers` 编译缓存哈希完备化（`VULN-10`）与 `author_steps` 编译会话复用（`VULN-09`）；
   - `JudgePool` 子任务 DAG `dependencies` 级联跳过（`VULN-06`）与通信题 `manager_score == 1.0` 结论归一（`VULN-08`）；
   - Linux `seccomp.rs` 补齐 `aarch64` BPF 规则表并修复原 `SYS_CLONE` 相对跳转偏移差一漏洞（`VULN-07`）。
2. **新功能扩展 (`FEAT-01` ~ `FEAT-04`)**：
   - 通信题 `star`（星型/Master-Worker）拓扑全平台支持（`FEAT-01`）；
   - `uj contest` 与 MCP `contest` 支持 `--problem` / `--contestant` 过滤重测（`FEAT-02`）；
   - `uj tl` 支持分 Subtask 耗时统计与保留原缩进/注释的非破坏式写回（`FEAT-03`）；
   - `uj doctor` 增加子任务 Pattern 交叉重叠与孤儿 `.ans`/`.out` 文件静态诊断（`FEAT-04`）。
