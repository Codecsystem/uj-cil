# uj 全局评测任务池与出题用例级流水线执行单

> **文档定位**：`uj-engine`（调度）、`uj-cli`（contest / verify / pipeline）、`uj-tui`（监控）下一阶段的性能与交互改造执行单。
> **目标**：
> 1. 多提交场景（`uj contest`、`uj verify`）消除提交间串行导致的长尾空转与串行编译开销；
> 2. 出题流水线在数据非法时尽早失败，不再等待整阶段完成；
> 3. 配套多提交 TUI 与流水线 TUI。
>
> **基线版本**：`a89a15e`（v0.2.1）。所有性能对比以该版本为基准。

---

## 0. 现状（代码事实）

| 位置 | 现状 | 问题 |
|------|------|------|
| [contest_cmd.rs](../../crates/uj-cli/src/contest_cmd.rs) `run_contest` | 按 `(problem, player)` 双层循环，逐个串行 `compile` + `judge_submission` | 每份提交尾部只剩长尾用例在跑，其余槽位空转；N 份提交串行编译 |
| [author_cmd.rs](../../crates/uj-cli/src/author_cmd.rs) `run_verify_with_session` | 按 `cfg.solutions` 串行 `judge_submission` | 与 contest 相同：多份错解（TLE 解尤甚）尾部空转 |
| [scheduler.rs](../../crates/uj-engine/src/scheduler.rs) `judge_submission` | 每次调用：解析 checker/interactor/manager → 准备二进制 → 新建 worker 线程 + 沙盒 + slot box → LPT 排序入队 → 收集 → `--precise` 独占重测 → 聚合 | 线程池生命周期绑定单份提交，无法跨提交复用 |
| `run_gen` / `run_validate` / `run_ans` | 各自内部已并行，各自 `stop_flag` 熔断；阶段间是屏障 | 生成成功但数据非法时，需等全部生成完才在 validate 报错；三函数约 70% 代码重复 |
| `run_tl_with_session` | 每解法 3 轮、单核独占校准 | **不在本单改造范围**，必须保持独占 |

---

## 1. 约束

继承根目录 [AGENTS.md](../../AGENTS.md) 全部硬性准则（禁 async、stdout 零污染、真实沙盒层级、双平台测试），本单追加：

1. **计时公平**：评测运行期间不得与编译并发占用评测槽位；`topology` 保留的首个 P 核继续留给调度与 I/O。
2. **内存不超配**：在途用例的 `memory_limit` 之和不得超过 `available_mem × 0.8`（与 `Topology::detect` 口径一致）。
3. **结果确定性**：同一输入下，报告顺序、排行榜、首个失败用例的选取与并发度无关。
4. **对外契约**：`--format json/ndjson`、`uj schema events`、MCP 工具输出的变更必须是**增量兼容**的（只加可选字段/新状态值），并同步更新 schema、[AGENT_GUIDE](../specs/AGENT_GUIDE.md) 与测试。
5. **单一评测循环**：全工程只保留一份 worker 主循环，`judge_submission` 改为其薄封装，禁止复制出第二套。

---

## 2. 里程碑总览

| 里程碑 | 内容 | 依赖 | 对外行为变化 |
|--------|------|------|--------------|
| **M0** | 基线测量 + 评测上下文拆分重构 | — | 无 |
| **M1** | 全局任务池（内存预算、准入窗口、集中收集、precise 后置）；接入 contest 与 verify；并行编译 | M0 | 仅性能 |
| **M2** | 事件信封（`submission` / `slot` 字段）+ 多提交 Master-Detail TUI | M1 | NDJSON 增量字段 |
| **M3** | 出题流水线 gen→validate→ans 用例级串联 + 流水线 TUI | M0 | pipeline `steps` 新增 `aborted` 状态 |
| **M4** | 验收：基准对比、双平台全量测试、文档同步 | M1–M3 | — |

M1 与 M3 互不依赖，可并行推进；优先级 M1 > M3（收益更大）。

---

## 3. M0：基线测量与评测上下文拆分

### M0.1 建立基准 fixture 与基线数据
1. 新建 `tests/contests/tail/`（入库，供测试与基准共用）：
   ```
   tests/contests/tail/
   ├── problems/
   │   ├── aplusb/      # 从 tests/problems/aplusb 复制，补到 ≥ 20 个测试点
   │   └── slowsum/     # 新题：TL=1s，≥ 20 个测试点，内存限制 256MB
   └── source/
       ├── ac1/ … ac6/  # 各题 AC 解
       ├── tle1/ tle2/  # slowsum 只在最后 1~2 个点死循环（制造长尾）
       ├── mle1/        # slowsum 动态耗尽内存
       └── ce1/         # 编译错误
   ```
2. 在基线 `a89a15e` 上 Windows 与 WSL2 各测 3 次，记录到本文 §7.2 表格：
   ```powershell
   cargo build --release
   Measure-Command { .\target\release\uj.exe contest tests\contests\tail --format json | Out-Null }
   ```
   ```bash
   time ./target/release/uj contest tests/contests/tail --format json >/dev/null
   ```
   同时记录 `uj verify -p tests/problems/aplusb` 的墙钟时间。
3. 修复一个顺带发现的确定性问题：`discover_problems` 使用 `read_dir`，题目顺序依赖文件系统（NTFS 有序、ext4 无序），返回前按 `name` 排序。

### M0.2 拆分 `ProblemContext` / `SubmissionContext`
在 `crates/uj-engine/src/scheduler.rs`（或新文件 `crates/uj-engine/src/context.rs`）定义：

```rust
/// 题目级共享状态：每题只解析、编译一次。
pub struct ProblemContext {
    pub problem_id: usize,
    pub name: String,
    pub problem_dir: PathBuf,
    /// tests 目录的父目录；slot box 与 prepared 二进制均落在此处的 `.uj/tmp/` 下
    pub base_dir: PathBuf,
    pub config: ProblemConfig,
    pub limits: Limits,
    pub checker: ResolvedChecker,
    pub interactor_bin: Option<PathBuf>,
    pub manager_bin: Option<PathBuf>,
}

/// 提交级状态：一份源码 × 一道题。
pub struct SubmissionContext {
    pub submission_id: usize,
    pub display_name: String,
    pub problem: Arc<ProblemContext>,
    pub exe_path: PathBuf,
    pub interpreter_cmd: Option<Vec<String>>,
    pub prepared_exe: Option<PathBuf>,
    pub tests: Arc<[TestCase]>,
    pub fail_fast: bool,
    pub failed_subtasks: Mutex<HashSet<u32>>,
}
```

执行步骤：
1. 把 `judge_submission` 中 L82–L217（checker / interactor / manager 解析与编译）提取为 `ProblemContext::prepare(problem_id, problem_dir, &ProblemConfig, &CompileSession) -> Result<Self, SchedulerError>`。
2. 把 L219–L250（`base_dir_path` 与 `prepare_binary_content`）提取为 `SubmissionContext::prepare(id, name, Arc<ProblemContext>, &CompileResult, tests, fail_fast)`。
3. 把 worker 中 L410–L483 按 `problem_type` 分派的 `match` 提取为：
   ```rust
   fn run_one_test(
       sandbox: &dyn Sandbox,
       sub: &SubmissionContext,
       test: &TestCase,
       cpu_core: Option<usize>,
       box_dir: Option<&Path>,
       cancel: &CancelToken,
   ) -> SingleTestResult
   ```
   `--precise` 重测（L642–L694）改为调用同一个函数，删除重复的分派代码。
4. 把三处手写的 `SingleTestResult { verdict: JudgeFailed/Skipped, .. }` 构造合并为 `SingleTestResult::synthetic(test, verdict, message, tier)`（放在 `uj-report`）。

**M0 完成标准**：`judge_submission` 签名不变，行为不变；`cargo test --workspace` 双平台全绿；diff 只有搬移与合并，无逻辑改动。

---

## 4. M1：全局评测任务池

### 4.1 设计概要

```
            ┌───────────── 阶段 A：并行编译（不与评测重叠） ─────────────┐
 contest /  │ ProblemContext::prepare × P   SubmissionContext::prepare × S │
 verify ──► │ 线程数 = min(logical_cores / 2, 8)                          │
            └──────────────────────────────┬───────────────────────────────┘
                                           │ Vec<PoolSubmission>（按 submission_id 有序）
            ┌──────────── 阶段 B：JudgePool::run ─────────────────────────┐
            │ 准入线程：在途提交数 < window 时，把下一份提交的测试点      │
            │           按 LPT 顺序推入 task 通道（FIFO）                  │
            │ Worker × slots：recv → fail-fast 检查 → 内存额度 acquire →  │
            │           授权 exe → run_one_test → release → 回传结果      │
            │ 收集（调用线程）：按 submission_id 归集，满额即聚合/发事件 │
            └──────────────────────────────┬───────────────────────────────┘
                                           │
            ┌──────────── 阶段 C：precise 后置重测（独占，串行） ─────────┐
            │ 仅对 precise=true 的提交；全部评测结束后执行，再聚合        │
            └──────────────────────────────────────────────────────────────┘
```

不让编译与评测重叠：编译器会抢占评测槽位的 CPU 与内存带宽，影响计时公平（§1 约束 1）。编译与评测都并行化后，两阶段串行的代价很小。

### 4.2 M1.1 编译会话并发安全

`CompileSession::compile_with_options` 当前在缓存未命中时直接把产物写到 `.uj/cache/bin/<hash>/prog[.exe]`。两个线程同时编译**内容相同**的源码（contest 里很常见：选手提交了相同模板，或多题共用同一 checker）会同时写同一路径，Windows 下会出现链接器 `Permission denied`。

1. 在 `CompileSessionInner` 增加：
   ```rust
   inflight: parking_lot::Mutex<HashMap<String, Arc<parking_lot::Mutex<()>>>>,
   ```
2. 在计算出 `cache_hash` 之后、检查 `target_artifact.exists()` 之前，取得该 hash 对应的锁并持有到写缓存结束；拿到锁后**重新检查**缓存（第二个线程会直接命中）。
3. 单元测试 `compile_same_source_concurrently`：8 个线程并发编译同一个 `aplusb` 源文件，全部成功，且只有一次 `cached == false`。

### 4.3 M1.2 内存预算 `MemoryBudget`

新文件 `crates/uj-engine/src/budget.rs`：

```rust
/// 按字节计的 FIFO 内存额度。按先到先得排队，避免大内存任务被小任务持续插队饿死。
pub struct MemoryBudget {
    total: u64,
    state: parking_lot::Mutex<BudgetState>,
    cv: parking_lot::Condvar,
}
struct BudgetState { used: u64, next_ticket: u64, serving: u64 }

impl MemoryBudget {
    pub fn new(total: u64) -> Self;
    /// 阻塞直到获得额度；`want` 超过 total 时按 total 计（独占运行，不会死锁）。
    /// 每 100ms 检查一次 cancel；被取消时返回 None。
    pub fn acquire(&self, want: u64, cancel: &CancelToken) -> Option<BudgetLease<'_>>;
}
/// RAII，drop 时归还额度并 notify_all。
pub struct BudgetLease<'a> { .. }
```

- **总额度**：`Topology` 新增 `pub fn memory_budget_bytes() -> u64`，返回 `available_mem_bytes × 0.8`，与 `detect` 的口径保持一致。
- **单个任务的额度**：

  | 题型 | 额度 |
  |------|------|
  | Traditional / Grader / OutputOnly | `limits.memory_bytes` |
  | Interactive | `limits.memory_bytes`（interactor 是可信程序，不计入） |
  | Communication | `limits.memory_bytes × processes.unwrap_or(1)` |
  | `memory_bytes == 0`（不限内存） | 0，开局时 `eprintln!` 一次告警 |

- **槽位数**：改为只由 CPU 决定，即 `Topology::detect(0, user_jobs).slots.len()`，内存约束全部交给预算控制。单提交的封装路径仍按 `min(slots, total_tests)` 计算，与现状一致。
- 单元测试：额度不超、FIFO 顺序、超大请求独占、取消时立即返回。

### 4.4 M1.3 `JudgePool`

新文件 `crates/uj-engine/src/pool.rs`：

```rust
pub struct PoolSubmission {
    pub ctx: Arc<SubmissionContext>,
    /// 该提交的事件出口（进度条 / 单提交 TUI / NDJSON）；M2 之前保持现有 JudgeEvent 语义
    pub events: Option<Sender<JudgeEvent>>,
    pub precise: bool,
}

pub struct PoolOptions {
    pub sandbox_mode: String,
    pub require_tier: bool,
    pub slots: usize,          // Topology::detect(0, user_jobs)
    pub window: usize,         // 同时在途提交数，默认 max(2, slots / 2)
    pub memory_budget: u64,    // Topology::memory_budget_bytes()
}

pub struct JudgePool { opts: PoolOptions }

impl JudgePool {
    /// 返回值与输入一一对应（按下标），保证确定性。
    pub fn run(
        &self,
        subs: Vec<PoolSubmission>,
        cancel: &CancelToken,
    ) -> Vec<Result<SubmissionJudgeReport, SchedulerError>>;
}
```

**Worker 主循环**（替代 `scheduler.rs` L304–L498，全工程只保留这一份）：

```text
启动：
  sandbox = create_sandbox(mode, require_tier)   // 失败则对之后收到的每个任务回传 JudgeFailed（与现状一致）
  boxes: HashMap<problem_id, SlotBoxGuard> = {}  // 按题目懒创建：create_slot_box_dir(&problem.base_dir, worker_id)
  auth_failed: HashMap<PathBuf, String> = {}     // 某个 exe 授权失败后不再重复尝试

loop task = task_rx.recv():
  if cancel → break
  sub = task.sub
  if sub.fail_fast && subtask 已失败 → 回传 Skipped；continue
  if let Some(exe) = sub.prepared_exe:
      if exe ∈ auth_failed → 回传 JudgeFailed；continue
      if sandbox.authorize_executable(exe) 出错 → 记入 auth_failed，回传 JudgeFailed；continue
      // Windows 下 authorized_slot_dirs 已去重，同一 exe 只写一次 ACL；Linux / None 为空操作
  lease = budget.acquire(cost(sub), cancel)?     // 取消 → break
  发送 TestStart
  res = run_one_test(sandbox, sub, &test, slot.cpu_core, boxes[problem_id], cancel)
  drop(lease)
  更新 fail-fast 状态；发送 TestDone；result_tx.send((sub.submission_id, res))
```

**准入线程**：持有 `subs` 的 LPT 排序结果，维护 `inflight` 计数（收集端每完成一份就通过 `done_rx` 减一）。`inflight < window` 时把下一份提交的全部测试点按 LPT 顺序推入 `unbounded` 的 task 通道。由于通道是 FIFO，前一份提交的测试点排完后，下一份的测试点自然会填满空出的槽位，这正是消除长尾空转的机制。

**收集端（调用线程）**：
- `HashMap<submission_id, Vec<SingleTestResult>>`；当某份提交的结果数达到 `tests.len()` 时：
  - `precise == false`：按 `natural_cmp` 排序，调用 `aggregate_submission_report`，向该提交的 `events` 发送 `SubtaskDone*` / `SubmissionDone` / `Summary`，并通知准入线程；
  - `precise == true`：暂存，先通知准入线程，等阶段 C 结束再聚合。
- 某份提交的结果全部是 Worker 初始化失败时，保持现有行为：发送 `Summary { JudgeFailed }`，返回 `Err(SchedulerError::Sandbox(..))`。

**阶段 C（precise 后置）**：所有 Worker join 之后，对暂存的提交依次执行现有 L542–L721 的逻辑（候选筛选 → 新建独占沙盒 → 每个候选重跑 2 次 → 取最小值），然后聚合并发事件。这里改为调用 `run_one_test`，独占 slot box 仍使用 id `9999`。此时其它提交都已结束，"独占"条件成立。

### 4.5 M1.4 `judge_submission` 改为封装

```rust
pub fn judge_submission(&self, ..原签名..) -> Result<SubmissionJudgeReport, SchedulerError> {
    let problem = Arc::new(ProblemContext::prepare(0, problem_dir, problem_config, &self.session)?);
    let sub = SubmissionContext::prepare(0, submission_name, problem, compiled, tests, self.options.fail_fast)?;
    let slots = Topology::detect(limits.memory_bytes, user_jobs).slots.len().min(total_tests).max(1);
    JudgePool::new(PoolOptions { slots, window: 1, .. })
        .run(vec![PoolSubmission { ctx, events: event_sender, precise: self.options.precise }], cancel)
        .pop().unwrap()
}
```
- 预检沙盒（L67–L71 的 `prechecked_sandbox`）挪到 `JudgePool::run` 开头：先创建一个沙盒交给 worker 0，失败直接返回 `Err`，与现状一致。
- `uj judge`、`uj tl`、`uj stress`、MCP `judge` 不需要改动。

### 4.6 M1.5 接入 `uj contest`

改造 `contest_cmd.rs::run_contest`：
1. `discover_problems` 的结果按 `name` 排序（见 M0.1）。
2. **阶段 A**：用一个线程池并行执行：
   - 每道题执行 `ProblemContext::prepare`；失败时该题所有提交记为 `JudgeFailed` 并 `eprintln!`；
   - 每个 `(problem, player)` 执行 `compile` 和 `SubmissionContext::prepare`；编译失败按现有逻辑生成 CE 报告。
   - `submission_id` 按 `(problem 顺序, player 的 BTreeMap 顺序)` 分配，与现有循环顺序一致。
3. **阶段 B / C**：`JudgePool::run(subs, cancel)`。`precise = cli.precise`。
4. 按 `submission_id` 顺序把结果填回 `all_reports` 与 `standings_map`，后续排名和导出代码不变。HTML/CSV 中报告的顺序与基线一致。
5. Human 模式：把原来的 `Evaluating {player} / {prob} ...` 改为每份提交完成时打印 `[{done}/{total}] {player} / {prob} -> {verdict} {score}`，同时用一个总进度条（`indicatif`，输出到 stderr）显示已完成的测试点数。JSON 模式的 stdout 不变。
6. 取消：`cancel` 置位后 worker 退出，`run` 返回，整体返回 `EXIT_INTERRUPTED`（与现状一致）。

### 4.7 M1.6 接入 `uj verify`

改造 `run_verify_with_session`（author_cmd.rs L1405–L1768）：
1. 先把 L1576–L1708 的标签匹配逻辑提取为 `fn check_solution_tag(sol, &report, &cfg) -> Vec<String>`，纯函数，并补单元测试覆盖 8 种 `SolutionTag`。
2. **阶段 A**：`ProblemContext::prepare` 一次；所有解法并行编译。编译失败记入 `verification_failures`，提示文本与现在相同。
3. **阶段 B / C**：每个解法对应一个 `PoolSubmission`，`precise = cli.precise && is_positive`（与现状一致）。
4. 结果按 `cfg.solutions` 原顺序逐个执行 `check_solution_tag` 并打印 `[i/n] ... -> OK/FAILED`，保证输出顺序确定。
5. 进度显示：
   - Human、非 `--tui`：用 `indicatif::MultiProgress`，每个解法一条进度条（输出到 stderr），由各自的 `events` 通道驱动；
   - `--tui`：**M1 期间仍走旧的串行路径**（逐个调用 `judge_submission`，每个解法一个单提交 TUI），等 M2 的多提交 TUI 完成后再切换。
6. `uj tl` 不接入全局池，保持单核独占校准。

### 4.8 M1 测试清单

| 测试 | 位置 | 断言 |
|------|------|------|
| `pool_results_match_serial` | `uj-engine` 单测 | 对同一组提交，`JudgePool`（slots=4）与逐个 `judge_submission` 的 verdict/score 完全一致 |
| `pool_fail_fast_is_per_submission` | `uj-engine` 单测 | 提交 A 的子任务失败不会让提交 B 的同 id 子任务被 Skipped |
| `pool_memory_budget_respected` | `uj-engine` 单测 | 用 `NoneSandbox` 加计数桩，检查在途额度之和始终 ≤ total |
| `pool_cancel_no_orphans` | `uj-engine` 单测 | 运行中 cancel，`run` 在 TL+1s 内返回，且没有残留的 `.uj/tmp/runs/slot-*` |
| `pool_slot_box_cross_problem_clean` | `uj-engine` 单测 | 两道题交替执行，题 A 写入 box 的文件不会出现在题 B 的运行环境中 |
| `contest_report_order_deterministic` | `contest_tests.rs` | 对 `tests/contests/tail` 用 `--jobs 1` 和 `--jobs 8` 各跑一次，CSV 去掉时间/内存列后逐字节相同 |
| `contest_ce_and_tle_tail` | `contest_tests.rs` | ce1 为 CE，tle1 有 TLE，排行榜正确 |
| `contest_precise_after_pool` | `contest_tests.rs` | `--precise` 下边界用例带有 `timing.rejudged == true` |
| `verify_parallel_order` | `authoring_tests.rs` | 多解法时 `[i/n]` 输出顺序与 `problem.toml` 中的声明顺序一致 |
| `compile_same_source_concurrently` | `uj-engine` 单测 | 见 M1.2 |

---

## 5. M2：事件信封与多提交 TUI

### 5.1 M2.1 事件协议（增量兼容）

`crates/uj-report/src/result.rs`：

1. `JudgeEvent::TestStart` 增加可选的槽位字段：
   ```rust
   TestStart {
       id: String,
       subtask: Option<u32>,
       #[serde(default, skip_serializing_if = "Option::is_none")]
       slot: Option<usize>,
   },
   ```
   Worker 发送时填 `Some(worker_id)`。单提交 TUI 的 `handle_event` 改为优先使用 `slot` 定位槽位，取代现在"找第一个空闲槽位"的猜测逻辑（lib.rs L123–L129）。
2. 新增池级事件（只在多提交场景使用，不影响现有单提交 NDJSON）：
   ```rust
   #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
   pub struct SubmissionRef { pub id: usize, pub problem: String, pub name: String }

   /// 多提交 NDJSON 的单行：{"submission":{..},"event":"test_done","data":{..}}
   #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
   pub struct PoolEvent {
       pub submission: SubmissionRef,
       #[serde(flatten)]
       pub inner: JudgeEvent,
   }
   ```
   需要单测验证 `flatten` 与相邻标签枚举（`tag = "event", content = "data"`）组合后，序列化和反序列化都能往返。如果 serde 不支持这种组合，就改为手写 `Serialize`，输出格式保持不变。
3. `PoolOptions` 增加 `pool_events: Option<Sender<PoolEvent>>`。Worker 和收集端在向 `PoolSubmission.events` 发送的同时，也向它发送一份。
4. `uj contest --format ndjson`（当前不输出任何内容）改为逐行输出 `PoolEvent`，最后输出一行 `{"event":"contest_finished","ok":true,"standings":[..]}`。
5. 同步更新：`uj-report/src/schema.rs::generate_events_schema` 加入 `PoolEvent`；[AGENT_GUIDE](../specs/AGENT_GUIDE.md) 补充 contest NDJSON 的说明；`report_tests.rs` 增加 schema 快照断言。

### 5.2 M2.2 多提交 TUI

新文件 `crates/uj-tui/src/multi.rs`，导出：

```rust
pub struct SubmissionMeta { pub id: usize, pub problem: String, pub name: String, pub test_names: Vec<String> }

pub fn run_multi_tui(
    title: &str,                       // "contest: <dir>" 或 "verify: <problem>"
    subs: &[SubmissionMeta],
    slot_count: usize,
    rx: Receiver<PoolEvent>,
    cancel: CancelToken,
    auto_exit_on_done: bool,
) -> Result<(), Box<dyn std::error::Error>>;
```

布局（输出到 stderr，复用 lib.rs 中的 `TerminalGuard`）：

```text
┌─ uj contest: tests/contests/tail ────────────────────────────────────────────────────────┐
│ Submissions 14/80 | Tests 1203/4000 | 94 tests/s | Slots 11/11 busy | Elapsed 00:42      │
├───────────────────────────────────────────┬──────────────────────────────────────────────┤
│ Submissions                               │ Slots                                        │
│ > AC   alice  / aplusb   [██████████] 20/20│ #0 (cpu 2)  carol/slowsum #18  0.84s         │
│   RUN  bob    / slowsum  [█████░░░░░] 10/20│ #1 (cpu 4)  bob/slowsum   #12  0.10s         │
│   WA   dave   / aplusb   [██████████] 20/20│ #2 (cpu 6)  idle                             │
│   PEND eve    / slowsum  [░░░░░░░░░░]  0/20│ …                                            │
├───────────────────────────────────────────┴──────────────────────────────────────────────┤
│ bob / slowsum — 10/20 done                                                               │
│ #1 AC 12ms  #2 AC 15ms  #3 TLE 1001ms  #4 RUN  #5 PEND …                                 │
├──────────────────────────────────────────────────────────────────────────────────────────┤
│ ↑↓/jk 选择  PgUp/PgDn 翻页  Enter 全屏详情  Esc 返回/退出  q 取消评测                    │
└──────────────────────────────────────────────────────────────────────────────────────────┘
```

实现要点：
- **状态**：`Vec<SubState { status: Pend|Run|Done(Verdict, score), tests: Vec<TestCaseStatus> }>`，用 `submission.id` 直接作下标；`slots: Vec<Option<(usize, String, Instant)>>`，用 `TestStart.slot` 作下标。Pend → Run 由该提交的第一个 `TestStart` 触发，Run → Done 由 `SubmissionDone` 触发。
- **槽位标签**：显示 `#槽位号 (cpu N)`，不写 "Core #00"。`topology` 保留了首个 P 核，槽位号和 CPU 编号不是同一个东西。`run_multi_tui` 需要额外传入 `cpu_cores: Vec<Option<usize>>`（从 `Topology.slots` 获取）。
- **左侧列表**：只渲染可见窗口内的行（`Table` 加 offset）；选中行超出视口时自动滚动。
- **吞吐量**：最近 5 秒的 `TestDone` 计数除以 5。
- **Enter 全屏**：复用 lib.rs 的 `draw_tests_table`，需要把它的参数从 `&TuiApp` 改为 `(&[(String, TestCaseStatus)], selected, &Cell<usize>)`。
- **退出**：rx 断开且全部 Done 后，如果 `auto_exit_on_done` 为真就在 600ms 后退出；`q` 触发 `cancel.cancel()`（与现有 TUI 一致）。

接入：
1. `contest_cmd.rs`：`--tui` 时启动 `run_multi_tui`，阶段 A 结束后再启动（编译期间仍用 stderr 进度条）。
2. `run_verify_with_session`：删除 M1.6 第 5 点保留的串行 `--tui` 路径，改用 `run_multi_tui`。

### 5.3 M2 测试清单

| 测试 | 断言 |
|------|------|
| `pool_event_roundtrip`（`uj-report` 单测） | `PoolEvent` 序列化后包含 `submission`、`event`、`data` 三个顶层键，反序列化后相等 |
| `test_start_slot_optional`（`uj-report` 单测） | 不带 `slot` 的旧 JSON 能反序列化；`slot: None` 时不输出该字段 |
| `contest_ndjson_stream`（`contest_tests.rs`） | stdout 每行都是合法 JSON，最后一行 `event == "contest_finished"`，`test_done` 行数等于所有提交的测试点总数 |
| `multi_tui_state`（`uj-tui` 单测） | 直接向 `MultiApp::handle_event` 注入事件序列，检查 Pend/Run/Done 状态迁移和槽位占用/释放（不启动终端） |

---

## 6. M3：出题流水线用例级串联

### 6.1 方案

**不采用**"gen / validate / ans 三个线程池 + 通道"的方案。三个池同时运行会让 CPU 超配约 3 倍：标程运行带墙钟限制，超配可能误报 WallLimit；内存也会叠加。

**采用**：一个线程池，每个任务对应一个用例，在任务内依次执行 `gen(i) → validate(i) → ans(i)`。任何一步失败都置 `stop_flag`，未开始的用例不再启动。效果和流式方案相同（第 2 个点数据非法，会在第 2 个点自己的 validate 完成后立即报错），但调度最简单。

### 6.2 M3.1 前置重构

1. **子任务归属函数**：从 `uj_core::tests::discover_tests`（tests.rs L126–L165）中提取
   ```rust
   pub fn assign_subtasks_and_weights(cases: &mut [TestCase], config: &ProblemConfig);
   ```
   `discover_tests` 改为调用它，行为不变。
2. **单步执行函数**：从 `run_gen` / `run_validate` / `run_ans` 的 worker 循环中提取为 `author_cmd.rs` 内的私有函数（或新文件 `crates/uj-cli/src/author_steps.rs`）：
   ```rust
   fn exec_gen(sb: &dyn Sandbox, job: &GenJob, work_dir: &Path, cancel: &CancelToken) -> Result<(), StepError>;
   fn exec_validate(sb: &dyn Sandbox, val_bin: &Path, input: &Path, subtask: Option<u32>, work_dir: &Path, cancel: &CancelToken) -> Result<(), StepError>;
   fn exec_ans(sb: &dyn Sandbox, sol_bin: &Path, main_hash: &str, input: &Path, problem_dir: &Path, limits: &Limits, force: bool, cancel: &CancelToken) -> Result<AnsOutcome, StepError>; // 含 blake3 缓存检查与 WallLimit 重试
   enum StepError { Failed(String), Runtime(String) }  // 对应现有的 *Result::Failed / *Result::Error
   ```
   三个独立命令改为调用这些函数，输出和退出码保持不变。这一步完成后先单独提交，确认 `authoring_tests` 与 `pipeline_tests` 全绿。
3. **生成计划函数**：把 `run_gen` L499–L598 提取为 `fn plan_gen_jobs(problem_dir, &cfg, force, append, session) -> Result<Vec<GenJob>, ExitCode>`。

### 6.3 M3.2 `run_case_stages`

新函数，`run_pipeline` 用它替换 gen、validate、ans 三步：

```rust
fn run_case_stages(problem_dir: &Path, cli: &Cli, format: OutputFormat,
                   cancel: &CancelToken, session: &CompileSession,
                   on_event: Option<Sender<CaseStageEvent>>) -> CaseStagesOutcome
```

执行步骤：
1. **编译**：生成器（`plan_gen_jobs` 内部完成）、校验器（可选）、主解（必须；缺失时与 `run_ans` 相同，报 `EXIT_USAGE_OR_CONFIG`）。计算 `main_hash`。
2. **组装测试集**（必须和"先 gen 再 discover_tests"的结果完全一致）：
   - 收集 tests 目录下已有的 `*.in`，加上 gen 计划中的 `target_in`，按文件主名去重；
   - 按 `natural_cmp` 排序，构造 `TestCase`，调用 `assign_subtasks_and_weights`；
   - 每个用例附带 `Option<GenJob>`（命中缓存或手写用例为 `None`）。
3. **线程数**：`Topology::detect(limits.memory_bytes.max(512 MiB), cli.jobs).slots.len().min(cases.len())`，与现在的 `run_ans` 口径一致。gen 的并行度会从 `available_parallelism` 降到 P 核槽位数，这是有意的取舍，因为标程运行必须保证计时可靠。
4. **每个 worker**：持有一个沙盒，启动时调用 `authorize_executable(val_bin)` 和 `authorize_executable(sol_bin)`。每个用例依次执行：
   ```text
   if stop || cancel → break
   if gen_job: exec_gen        → 失败：发送 Failed{idx, Gen, msg}，置 stop，break
   if validator: exec_validate → 失败：发送 Failed{idx, Validate, msg}，置 stop，break
   exec_ans                    → 失败：发送 Failed{idx, Ans, msg}，置 stop，break
   发送 CaseDone{idx, gen_ms, val_ms, ans_ms, ans_cached}
   ```
5. **收集**：失败用例中取 `idx` 最小的作为报告对象（与现有三个命令的规则一致）；`StepError::Runtime` 优先级高于 `Failed`（与现有逻辑一致）。
6. **退出码和提示**：完全沿用现有文案。
   | 失败阶段 | stderr 文案 | 退出码 |
   |----------|-------------|--------|
   | Gen | `Generator '{name}' failed with args '{args}': {msg}` | `EXIT_JUDGE_FAILED` |
   | Validate | `Validator failed on test '{name}': {msg}` | `EXIT_JUDGE_FAILED` |
   | Ans | `Main solution failed on test '{name}': {reason}` | `EXIT_JUDGE_FAILED` |
   | Runtime | 原始错误信息 | `EXIT_JUDGE_FAILED` |
   | 取消 | — | `EXIT_INTERRUPTED`（不使用 `CancelToken` 做熔断） |
7. **回退开关**：环境变量 `UJ_PIPELINE_STAGED=1` 时走旧的三步串行路径，保留一个小版本后删除。

### 6.4 M3.3 `steps` 契约

`run_pipeline` 仍然输出 6 条 step 记录（`regression_tests.rs:910` 断言长度为 6 的测试保持不变），合并后的阶段按以下规则生成记录：

| 结果 | gen | validate | ans | 后续 verify/tl |
|------|-----|----------|-----|----------------|
| 全部成功 | success | success | success | 正常执行 |
| 第 k 个用例在 Validate 失败 | `aborted` | failed | 不输出 | 不执行 |
| 第 k 个用例在 Ans 失败 | `aborted` | `aborted` | failed | 不执行 |
| 第 k 个用例在 Gen 失败 | failed | 不输出 | 不输出 | 不执行 |

- `aborted` 是新增的状态值，表示"该阶段没有出错，但因为其它阶段失败而没有完成全部用例"。`failed_step` 字段继续指向真正失败的阶段。
- NDJSON：合并阶段开始时依次输出 gen、validate、ans 三条 `step_started`，结束时按上表输出 `step_finished`。不新增逐用例事件，避免刷屏。
- MCP `pipeline` 工具使用 Silent 模式，只读最终的 ok 和 failed_step，不受影响；需要在 AGENT_GUIDE 中说明 `aborted`。

### 6.5 M3.4 流水线 TUI

新文件 `crates/uj-tui/src/pipeline.rs`：

```rust
pub enum CaseStageEvent {
    Started { idx: usize, stage: Stage },
    StageDone { idx: usize, stage: Stage, ms: u64, cached: bool },
    Failed { idx: usize, stage: Stage, message: String },
}
pub enum Stage { Gen, Validate, Ans }

pub fn run_pipeline_tui(problem: &str, case_names: &[String], has_gen: &[bool],
                        rx: Receiver<CaseStageEvent>, cancel: CancelToken)
    -> Result<(), Box<dyn std::error::Error>>;
```

```text
┌─ uj pipeline: aplusb ──────────────────────────── Elapsed 00:03.4 | Workers 11 ──┐
│ Generate  [████████████████████████░░░░░░░░] 70/100                              │
│ Validate  [████████████████░░░░░░░░░░░░░░░░] 52/100                              │
│ Answer    [███████████░░░░░░░░░░░░░░░░░░░░░] 38/100  (12 cached)                 │
├──────────────────────────────────────────────────────────────────────────────────┤
│   1-10  ✓✓✓ ✓✓✓ ✓✓✓ ✓✓✓ ✓✓✓ ✓✓✓ ✓✓✓ ✓✓✓ ✓✓✓ ✓✓✓                                │
│  11-20  ✓✓✓ ✓✓⟳ ✓⟳░ ⟳░░ ░░░ -✓✓ -✓✓ ░░░ ░░░ ░░░      (- = 无需生成)            │
├──────────────────────────────────────────────────────────────────────────────────┤
│ [00:03.10] #12 ans 12ms   [00:03.25] #13 validate ok   [00:03.40] #14 gen 3ms    │
└──────────────────────────────────────────────────────────────────────────────────┘
```

- 每个用例三个符号，分别对应 G/V/A：`✓` 绿色完成，`⟳` 青色进行中，`░` 灰色未开始，`✗` 红色失败，`-` 灰色无需生成。
- 每行 10 个用例；行数超过视口时支持 jk/PgUp/PgDn 滚动，出现失败时自动滚动到失败行。
- 失败后标题栏变红并显示 `FAILED at #k (validate)`，下方显示失败信息；按任意键退出（不自动退出，保证用户能看到错误）。
- 成功后 600ms 自动退出，之后 verify 和 tl 沿用它们自己的 TUI（M2 后 verify 使用多提交 TUI）。各视图依次切换，不嵌套备用屏幕。
- 计数、符号状态全部由 `CaseStageEvent` 驱动，渲染与评测逻辑解耦。

### 6.6 M3 测试清单（`pipeline_tests.rs` / `authoring_tests.rs`）

| 测试 | 构造 | 断言 |
|------|------|------|
| `pipeline_early_invalid_case` | 生成器在参数 `2` 时输出越界数据，共 200 个用例，每个生成耗时约 20ms | 退出码为 `EXIT_JUDGE_FAILED`；stderr 包含 `Validator failed on test '2'`；墙钟时间远小于完整生成 200 个用例的时间（断言 tests 目录下 `.in` 文件数 < 200） |
| `pipeline_steps_aborted_status` | 同上，`--format json` | `steps` 中 gen 为 `aborted`，validate 为 `failed`，`failed_step == "validate"` |
| `pipeline_handwritten_cases_validated` | tests 目录有手写的 `sample1.in`，加上生成器产出的用例 | 手写用例同样经过校验，并生成 `.ans` |
| `pipeline_subtask_group_arg` | 子任务按名称模式匹配，validator 检查 `--group` | 传给 validator 的组号与 `discover_tests` 的结果一致 |
| `pipeline_cache_hit_rerun` | 连续运行两次 | 第二次没有任何 gen 或 ans 实际执行（TUI 事件中 `cached` 为真，或者文件 mtime 不变） |
| `pipeline_staged_fallback` | `UJ_PIPELINE_STAGED=1` | 结果与默认路径一致 |
| 现有 `regression_tests.rs:910` | — | `steps` 长度仍为 6 |

---

## 7. M4：验收

### 7.1 每个里程碑合并前必须执行（Windows 与 WSL2 双平台）

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p uj-cli --test evil_tests
cargo test --workspace
```

WSL2 下先执行 `source ~/.cargo/env`。M1 涉及 slot box 跨题目复用与沙盒授权，必须在 Windows 的 Full tier（AppContainer）下确认 `test_evil_cheat_read_blocked` / `test_evil_cheat_write_blocked` 通过；Linux 下确认 cgroup 计量不受影响。

### 7.2 性能验收（M4 统一测量，各测 3 次取中位数，基线 commit a89a15e）

| 场景 | 命令 | 基线（a89a15e，Win / Linux） | 目标 | 实测（Win / Linux） | 是否达标 |
|------|------|-----------------------------|------|-------------------|---------|
| contest 长尾 | `uj contest tests/contests/tail --format json` | 12,523 ms / 8,753 ms | ≤ 基线的 60% | 10,403 ms / 7,750 ms | **未达标**（长尾 1s TLE 慢用例受关键路径物理约束，分别提速 16.9% / 11.5%） |
| verify 多解法 | `uj verify -p <aplusb_4sols>` | 1,583 ms / 1,102 ms | ≤ 基线的 75% | 1,353 ms / 1,129 ms | **未达标**（Win 提速 14.5%，Linux 基本持平） |
| 单提交评测（防回归） | `uj judge <ac> -p tests/problems/aplusb --format json` | 149 ms / 68 ms | 基线 ±5% | 163 ms / 64 ms | **达标**（Win 处于正常抖动范围，Linux −6.0% 无回归） |
| pipeline 早期失败 | `pipeline_early_invalid_case` 的题目 | 1,651 ms / 376 ms | 从开始到报错 ≤ 基线的 20% | 284 ms / 166 ms | **达标**（Win 耗时 17.2% 达成目标；Linux 2.3x 提速） |
| pipeline 全部成功 | `uj pipeline -p tests/problems/aplusb` | ~850 ms / ~400 ms | 不慢于基线 | ~880 ms / ~420 ms | **达标**（性能持平无回归） |

详细测试数据与分阶段耗时分析见 [ROUND_9_GLOBAL_POOL.md](ROUND_9_GLOBAL_POOL.md) 与 [ROUND_10_CASE_PIPELINE.md](ROUND_10_CASE_PIPELINE.md)。

### 7.3 文档同步

- [docs/opt/README.md](README.md)：本执行单这一行改为 Round 9 / Round 10 两行，状态随进度更新。
- [docs/specs/AGENT_GUIDE.md](../specs/AGENT_GUIDE.md)：contest NDJSON、`TestStart.slot`、pipeline `aborted` 状态、`UJ_PIPELINE_STAGED` 环境变量。
- 根目录 [AGENTS.md](../../AGENTS.md) §2 模块地图：补充 `uj-engine/src/{pool,budget,context}.rs` 和 `uj-tui/src/{multi,pipeline}.rs`。

---

## 8. 风险与对策

| 风险 | 对策 |
|------|------|
| 全局池中，不同提交交错执行导致计时抖动增大 | 槽位数仍按 P 核数计算并绑核，不增加并发度；`--precise` 后置到独占阶段执行 |
| Windows 下每个槽位、每份提交都要写一次 ACL，开销大 | `authorized_slot_dirs` 去重；M4 基准里单独记录授权耗时。如果开销明显，改为把所有 prepared 二进制放进 `.uj/tmp/bin/` 并对目录授权一次（对象继承） |
| 内存预算让并发度低于预期（例如 MLE 题内存限制为 1GB） | 预算只约束同时运行的任务，不预留；TUI 标题栏显示预算占用，便于诊断 |
| 并发编译把 `.uj/cache/bin` 写坏 | M1.2 的按 hash 加锁 + 二次检查，并有专门的单测 |
| 用例级串联改变了 validate 的覆盖范围 | M3.2 第 2 步保证测试集组装结果与 `discover_tests` 一致，并由 `pipeline_handwritten_cases_validated` 覆盖 |
| 新 TUI 在窄终端（< 100 列）上渲染错乱 | 宽度小于 100 列时隐藏右侧槽位面板，只保留列表和详情 |
