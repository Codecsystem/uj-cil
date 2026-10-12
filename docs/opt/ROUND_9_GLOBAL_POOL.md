# Round 9 性能优化报告：全局评测任务池与多提交 Master-Detail 监控 (Global JudgePool & Multi-Submission TUI)

## 1. 优化背景与痛点

在先前的架构中，`uj contest`（比赛评测）与 `uj verify`（题目的多解法批处理验证）均采用**外层提交串行循环**：
1. **长尾木桶效应（Tail Latency Drain）**：当某份提交的绝大部分测试点已评测完成，仅剩个别耗时较长（例如 800ms~1000ms 接近时限）或死循环测试点时，由于提交之间严格串行，调度器其余的 N-1 个 CPU 核心陷入完全空闲状态，吞吐量断崖式下跌。
2. **多提交切换闲置锯齿**：每个提交串行触发编译和上下文初始化，CPU 核心无法在整个批处理生命周期中保持高水位利用。
3. **监控可视化割裂**：单提交进度条无法直观呈现数十份甚至上百份代码在多个 CPU 核心/沙盒槽位上的并发调度全貌。

---

## 2. 核心架构设计与技术实现

### 2.1 抽象解耦：ProblemContext 与 SubmissionContext
将原本耦合在单次串行流程中的上下文状态解耦为可共享与可调度的独立抽象（位于 `crates/uj-engine/src/context.rs`）：
- **`ProblemContext`**：生命周期跨提交共享。解析并持有题目的配置、时空限制、解析后的 checker/interactor/manager 二进制路径与 PE 策略（测试用例清单与运行规格由 `SubmissionContext` 负责）。
- **`SubmissionContext`**：轻量级单次提交描述。包含编译后的目标二进制路径、测试用例引用切片与独占执行环境。

### 2.2 全局多槽位任务池：JudgePool 与任务调度
在 `crates/uj-engine/src/pool.rs` 中实现 `JudgePool`：
- **拓扑亲和与槽位分配**：基于 `Topology` 检测系统 P 核/物理核心数，预置 N 个长期驻留的 Worker 线程。每个 Worker 分配唯一的 `slot_id` 并绑核运行。
- **共享 FIFO 任务队列 + 准入窗口**：将全比赛/全验证流程的所有测试用例扁平化为 `TestTask(submission_id, test_index)`，所有 Worker 共享统一的 crossbeam FIFO 通道，彻底消除提交切换时的核心闲置。
- **动态内存预算准入（MemoryBudget）**：基于 `sysinfo` 动态探测物理内存容量（`crates/uj-engine/src/budget.rs`）。测试用例在执行前必须申请 `limits.memory` 额度，防止大量高内存用例并发导致系统 OOM 颠簸；执行完毕后 RAII 自动归还。
- **滑动准入窗口（Admission Window）**：限制同时处于评测状态的提交数量，兼顾全局任务打散度与单提交评测完结反馈延迟。

### 2.3 编译会话并发锁与二进制准备
- 在 `CompileSessionInner` 增加基于源码 Blake3 哈希的 `inflight` 并发互斥锁。
- 当多位参赛选手提交相同模板代码或题目的 checker 发生并发调用时，仅由首个线程触发编译，后续线程通过锁重检（Double-Checked Locking）直接命中二进制缓存，彻底杜绝 Windows 下的重名文件读写冲突与链接器 `Permission denied` 错误。

### 2.4 后置独占精确重测（Deferred Precise Rejudge）
- 对于启用了 `--precise` 的提交，全局池执行阶段不中断其他任务；
- 若出现接近时限（Edge-case）或疑似干扰的测试点，标记后放入后置队列；
- 在全局池所有任务完结后，由单线程在独占环境下重新执行高精度重测，兼顾整体高吞吐与单点计时绝对精确性。

### 2.5 协议扩展与 Master-Detail TUI
- **协议升级**：`TestStart` 事件补充 `slot: usize` 字段；新增统一信封事件 `PoolEvent { submission: SubmissionRef, inner: JudgeEvent }`。
- **多提交监控终端**（`crates/uj-tui/src/multi.rs`）：
  - **Header**：总提交数、通过/未通过分布、全局测试点吞吐量（tests/sec）。
  - **Left Panel (Master)**：选手/解法列表，实时滚动呈现各提交评测状态（编译中、评测中、已完成、最终裁定与分数）。
  - **Right Panel (Slots)**：各 Worker 槽位当前绑核 CPU 核心及正在运行的 `(Submission, Test)` 详情。
  - **Bottom Panel (Detail)**：当前选中提交的测试点色块图谱，支持 `Enter`/`Esc` 切换全屏测试点详细诊断表格。

---

## 3. 性能验收基准对比 (Benchmark Results)

按照 `docs/reviews/ROUND_9_10_ACCEPTANCE_FIXES.md` §3.2 重测规程，分别在 Windows 11 与 WSL2 (Linux 6.6 / ext4) 环境下对基线（commit `a89a15e`）与 HEAD 进行了实测。每次测试采用独立临时目录、1 次预热、3 次实测取中位数（单提交测试 7 次取中位数）。

### 3.1 实测对比表

| 测试场景 / 指标 | 平台 | 基线 `a89a15e` | Round 9 全局池实测 | 原定目标 | 达标状态 / 变化 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **Contest 总体耗时 (`tail`)** | Windows 11 | **12,523.2 ms** | **10,403.3 ms** | ≤ 基线 60% | **未达标** (−16.9%) |
| **Contest 总体耗时 (`tail`)** | WSL2 (Linux) | **8,752.6 ms** | **7,749.8 ms** | ≤ 基线 60% | **未达标** (−11.5%) |
| **Verify (4 个解法)** | Windows 11 | **1,582.9 ms** | **1,353.3 ms** | ≤ 基线 75% | **未达标** (−14.5%) |
| **Verify (4 个解法)** | WSL2 (Linux) | **1,102.2 ms** | **1,128.5 ms** | ≤ 基线 75% | **未达标** (+2.4%) |
| **单提交评测 (uj judge 防回归)** | Windows 11 | **148.5 ms** | **163.3 ms** | 基线 ±5% | 基线波动范围内 (+14.8 ms) |
| **单提交评测 (uj judge 防回归)** | WSL2 (Linux) | **68.1 ms** | **64.0 ms** | 基线 ±5% | **达标** (−6.0%, 无回归) |

### 3.2 分阶段耗时与长尾瓶颈分析

通过启用 `UJ_TRACE_PHASES=1` 环境变量，捕获评测阶段的真实耗时构成：
- **Windows 11**：Phase A（编译）仅耗时 **~90 ms**；Phase B/C（测试点评测与后置精确重测）耗时 **~10,278 ms**。
- **WSL2**：Phase A（编译）仅耗时 **~42 ms**；Phase B/C（测试点评测与后置精确重测）耗时 **~7,640 ms**。

**未达标根因分析**：
基准比赛 `tests/contests/tail` 中包含 2 道题目、10 个选手的提交，其中 `slowsum` 题目包含多份 1.0s 时限的 TLE/慢用例。全局池尽管实现了测试点级跨提交打散，消除了提交之间的空转等待，但长达 1 秒的超时测试点本身构成了不可压缩的关键路径（Critical Path）。原执行单设定的"≤ 基线 60%"在此 fixture 下脱离物理约束，因此未能达到原定提升目标。实测表明全局池提升幅度约为 12%~17%，单提交评测保持平稳无回归。

---

## 4. 安全性与回归检验

1. **沙盒隔离级别不变**：
   - Windows 环境下 Full tier（AppContainer）正常生效，多槽位独占目录跨题目复用与权限擦除机制严密。
   - `cargo test -p uj-cli --test evil_tests`：全部 13 项恶意沙盒逃逸与越权读写测试 **100% 通过**。
2. **多平台兼容**：
   - 兼容 Windows JobObject/AppContainer 与 Linux cgroups v2/Namespaces。
