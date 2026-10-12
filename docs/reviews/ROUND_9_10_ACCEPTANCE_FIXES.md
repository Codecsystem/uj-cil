# Round 9 / 10 验收问题修复执行单

> **文档定位**：针对提交 `32374d9`（feat(pool): implement global judge pool and case streaming pipeline）的验收结论与修复执行单。
> **对照文档**：[STREAMING_AND_GLOBAL_POOL_EXECUTION.md](../opt/STREAMING_AND_GLOBAL_POOL_EXECUTION.md)（原执行单）、[ROUND_9_GLOBAL_POOL.md](../opt/ROUND_9_GLOBAL_POOL.md)、[ROUND_10_CASE_PIPELINE.md](../opt/ROUND_10_CASE_PIPELINE.md)。
> **验收结论**：**暂不通过**。功能主体已按执行单落地，Windows 全量测试全绿；但 Linux 测试失败、`contest --fail-fast` 回归、性能报告与实测不符三项阻断合入。

---

## 0. 验收结果摘要

| 检查项 | 结果 |
|--------|------|
| `cargo fmt --check` | 通过 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过 |
| Windows `cargo test --workspace` | 全绿（含 evil_tests 13 项、全部新增测试） |
| WSL2 `cargo test --workspace --no-fail-fast` | **失败**：`pipeline_tests` 24/26，`uj-engine` 16/17 |
| 评测结论一致性（tail 比赛，基线 vs HEAD 各 3 次） | 10 选手 × 2 题结论完全一致 |
| `contest --fail-fast` | **回归**：基线 19 SKIPPED + 1 TLE，HEAD 19 AC + 1 TLE |
| 性能报告 | **与实测不符**，见 §3 |

### 问题清单与优先级

| 编号 | 级别 | 问题 | 阻断合入 |
|------|------|------|----------|
| F1 | P0 | Linux 沙盒 `__uj_bin` 在共享 work_dir 上竞态，出题流水线间歇性 `Exited(127)` | 是 |
| F2 | P0 | `uj contest` 未传递 `--fail-fast` | 是 |
| F3 | P0 | Round 9 / 10 报告性能数据不实，需重测并改正 | 是 |
| F4 | P1 | pool 单测直接在仓库 fixture 上运行，并发时互相干扰（flaky） | 是（CI 不稳定） |
| F5 | P1 | 两个 pool 单测的断言没有覆盖测试名所声明的行为 | 否 |
| F6 | P2 | AGENT_GUIDE 与两份报告中有与代码不符的描述 | 否 |
| F7 | P3 | contest 每提交一个转发线程；多提交 TUI 窄终端未降级 | 否 |

**修复顺序**：F1 → F2 → F4 → F5 → F7 → F3 → F6。F3 的重测必须在 F1、F2 合入之后进行；F6 与 F3 一起改文档。建议每个编号一个独立提交，便于回溯。

---

## 1. F1（P0）：Linux 沙盒 `__uj_bin` 共享目录竞态

### 1.1 现象

WSL2 下以下两个测试间歇性失败（单独运行也会失败，3 次中 2 次失败）：

```
test_pipeline_handwritten_cases_validated
  -> Pipeline failed: exit 3（期望 0）
test_pipeline_early_invalid_case
  -> stderr should contain validator failure on test '2',
     got: Main solution failed on test '1': Exited(127)
```

用 `--format human` 运行可以看到失败原因：

```
Main solution failed on test 'sample1': Exited(127)
Pipeline stopped: step 'uj ans' failed with status ExitCode(unix_exit_status(3))
```

### 1.2 根因

1. `author_steps::exec_ans`（`crates/uj-cli/src/author_steps.rs:281`）中，**所有 worker 线程都以 `problem_dir` 作为 `work_dir`**，并以 `trusted: false` 运行标程。
2. Linux 沙盒（`crates/uj-sandbox/src/linux/runner.rs`）把 `work_dir` 以读写方式 bind 到 rootfs 的 `/box`（L595），然后在 `rootfs/box/__uj_bin` 上创建文件并把可执行文件 bind 上去（L312–L325、L607–L630）。由于 `/box` 就是宿主的 `work_dir`，这个挂载点文件实际落在**宿主** `problem_dir/__uj_bin`，被所有 worker 共享。
3. 每次运行结束，L1208–L1211 的残留清理会 `remove_file(work_dir/__uj_bin)`。如果此时另一个 worker 正处于"创建挂载点之后、bind/execve 之前"，它的挂载点就被删掉，execve 失败，子进程 `_exit(127)`（L862）。

**证据**：

| 实验（WSL2，新建目录，每组 5 次） | 结果 |
|---|---|
| HEAD，默认并发 | 失败 4/5 |
| HEAD，`--jobs 1` | 0 失败 |
| HEAD，`UJ_PIPELINE_STAGED=1` | 0 失败 |
| HEAD，注释掉 L1208–L1211 的清理 | 0 失败（10/10 通过） |
| 基线 `a89a15e`，独立 `uj ans`，30 个用例 | 失败 5/5 |

结论：这个竞态**在基线就已存在**（由 `61bf889` 引入的清理逻辑触发），本次提交把它带进了默认的 `uj pipeline` 路径，因而被新测试暴露出来。HEAD 的独立 `uj ans` 碰巧没有复现，属于时序巧合，不代表没有问题。

### 1.3 修复方案

**把可执行文件的挂载点从 `/box` 移到 rootfs 的 tmpfs 根下。** 每次运行的 rootfs 是子进程私有挂载命名空间里新挂的 tmpfs（L535），挂载点只在该命名空间内存在，不会落到宿主目录，也就不需要清理。

`crates/uj-sandbox/src/linux/runner.rs`：

```rust
// L312–L325：挂载点改到 tmpfs 根，不再位于 /box（即宿主 work_dir）下
let (jail_exe_path, target_exe_pair) =
    if host_exe.starts_with("/usr") || host_exe.starts_with("/bin") {
        (host_exe.to_string_lossy().to_string(), None)
    } else {
        // 位于本次运行私有的 tmpfs rootfs 根目录，不与宿主 work_dir 共享
        let target_exe = rootfs.join("__uj_bin");
        let c_target_exe = std::ffi::CString::new(target_exe.to_string_lossy().as_bytes())
            .map_err(|e| SandboxError::Internal(format!("Invalid target exe: {e}")))?;
        let c_host_exe = std::ffi::CString::new(host_exe.to_string_lossy().as_bytes())
            .map_err(|e| SandboxError::Internal(format!("Invalid host exe: {e}")))?;
        ("/__uj_bin".to_string(), Some((c_host_exe, c_target_exe)))
    };
```

```rust
// L1208–L1211：删除。挂载点不再出现在宿主 work_dir 中，
// 而在共享目录上删除它正是本竞态的触发点。
-    let stray_bin = work_dir.join("__uj_bin");
-    if stray_bin.exists() {
-        let _ = fs::remove_file(stray_bin);
-    }
```

注意事项：

- L607 创建挂载点文件（`open(O_CREAT)`）发生在 tmpfs 挂载之后、`pivot_root` 之前，此时 rootfs 根仍可写，无需改动顺序。L747 的只读重挂不影响已有的 bind 挂载与 exec。
- 历史版本遗留在题目目录中的 `__uj_bin` 已被 `.gitignore` 覆盖，不需要运行时清理。如确有需要，放到 `uj doctor` 的清理逻辑中做一次性处理，**不要**放回每次运行的路径上。
- 改完后确认全仓库没有其它代码依赖 `/box/__uj_bin` 这个路径：`rg "__uj_bin" crates`。
- 只改 Linux 路径。Windows 沙盒不使用 `__uj_bin`，但按 AGENTS.md 仍需双平台测试。

### 1.4 回归测试

在 `crates/uj-sandbox/tests/sandbox_tests.rs` 新增（仅 Linux）：

```rust
/// 多个线程以同一个 work_dir 并发运行非可信程序，不得出现 Exited(127)，
/// 且 work_dir 中不得残留沙盒内部的挂载点文件。
#[cfg(target_os = "linux")]
#[test]
fn test_concurrent_untrusted_runs_share_work_dir() {
    let dir = tempfile::tempdir().unwrap();
    // 可执行文件必须位于 /usr、/bin 之外，才会走 bind 挂载路径
    let prog = dir.path().join("prog");
    std::fs::copy("/usr/bin/true", &prog).unwrap();

    let probe = uj_sandbox::create_sandbox("auto", false).unwrap();
    if probe.tier() == uj_core::run::Tier::None {
        eprintln!("skip: sandbox tier None, rootfs path not exercised");
        return;
    }

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let prog = prog.clone();
            let work_dir = dir.path().to_path_buf();
            std::thread::spawn(move || {
                let sb = uj_sandbox::create_sandbox("auto", false).unwrap();
                let cancel = uj_sandbox::CancelToken::new();
                for _ in 0..20 {
                    let spec = /* RunSpec { argv: [prog], work_dir, trusted: false,
                                  limits: Limits::new(1000, 256 MiB), .. } */;
                    let res = sb.run(&spec, &cancel).unwrap();
                    assert!(res.status.is_success(), "untrusted run failed: {:?}", res.status);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert!(!dir.path().join("__uj_bin").exists());
}
```

### 1.5 完成标准

- 新测试在 WSL2 上连续 5 次通过；修复前对同一测试应能复现失败（先写测试、确认失败，再改代码）。
- WSL2 下以下命令连续 20 次全部通过：
  ```bash
  for i in $(seq 1 20); do
    cargo test -q -p uj-cli --test pipeline_tests \
      test_pipeline_handwritten_cases_validated test_pipeline_early_invalid_case || break
  done
  ```
- `cargo test -p uj-cli --test evil_tests` 双平台全绿（rootfs 内文件布局有变化，需确认隔离未退化）。

### 1.6 后续项（不在本次范围）

`exec_ans`、`run_stress`、MCP `stress`（`mcp_server.rs:776/794`）都以 `problem_dir` 作为非可信程序的读写 `/box`，标程和暴力解因此可以写题目目录。F1 修复后不会再出现竞态，但隔离粒度仍偏粗。建议后续单独立项，改为每个 worker 使用自己的 slot box，并只读挂载输入文件。

---

## 2. F2（P0）：`uj contest` 丢失 `--fail-fast`

### 2.1 现象

在 tail 比赛的 `slowsum` 题上加一个 `min` 子任务，执行 `uj --fail-fast --jobs 1 contest`：

| 版本 | `tle1 / slowsum` 的 20 个测试点 |
|---|---|
| 基线 `a89a15e` | 19 SKIPPED + 1 TLE |
| HEAD `32374d9` | 19 AC + 1 TLE |

### 2.2 根因

`crates/uj-cli/src/contest_cmd.rs:376–383` 调用 `SubmissionContext::prepare` 时写死了 `false`：

```rust
match SubmissionContext::prepare(
    candidate.sub_id,
    &sub_display_name,
    p_ctx,
    &compiled,
    tests,
    false,          // ← 应为 cli.fail_fast
) {
```

`verify` 路径（`author_cmd.rs:1485`）传参正确，不受影响。

### 2.3 修复

在编译线程闭包外捕获 `let fail_fast = cli.fail_fast;`（与 `no_cache`、`precise` 的写法一致），传入 `prepare`。

### 2.4 回归测试

在 `crates/uj-cli/tests/contest_tests.rs` 新增 `test_contest_fail_fast_skips`：

1. 把 `tests/contests/tail` 复制到临时目录，**只在副本中**给 `problems/slowsum/problem.toml` 追加：
   ```toml
   [[subtasks]]
   id = 1
   score = 100.0
   scoring = "min"
   tests = ["*"]
   ```
   不要改仓库里的 fixture，否则会改变其它 contest 测试的分数。
2. 执行 `uj --fail-fast --jobs 1 --format json contest <dir>`。
3. 从输出的 `reports` 中找到 `submission == "tle1 - slowsum"`，断言 `Skipped` 数为 19，`TimeLimitExceeded` 数为 1。

必须用 `--jobs 1`：`20.in` 是最大的输入，按 LPT 排在最前面，单槽位下它先超时，其余 19 个点确定被跳过；多槽位下其余点可能在 TLE 落定之前就已跑完，断言会不稳定。

---

## 3. F3（P0）：性能报告重测与改正

### 3.1 不符之处

同机、release 构建、新建目录、预热一次后测 3 次（Windows 11，验收时测得，**仅供复测对照，不得直接写进报告**）：

| 场景 | 报告声称 | 验收实测（基线 → HEAD） | 原执行单目标 |
|---|---|---|---|
| contest tail | 17,300 → 9,616 ms（−44.4%） | ≈14.1 s → ≈10.9 s（约 −23%） | ≤ 基线 60%（未达） |
| pipeline 早期失败 | ~600 → ~50 ms（12×） | ≈2.0 s → 0.5–1.1 s（约 2–4×） | ≤ 基线 20%（未达） |
| verify（4 个解法） | — | ≈1.53 s → ≈1.46 s（约 −5%） | ≤ 基线 75%（未达） |
| 单提交 judge | ±0.3% | 两版均在 200–320 ms 间波动，无法得出 ±5% 结论 | 基线 ±5% |

具体问题：

1. **基线不是基线**：Round 9 报告的"优化前串行基线（`-j 1`）17.3 s"是**新版本**加 `--jobs 1` 的结果（验收复测约 20.8 s），并不是 `a89a15e`。44.4% 实际是新代码与自身单核运行的对比。
2. **数值不可能成立**：只有 2 个用例、在第 2 个用例失败的流水线，整条命令的固定开销就有约 250 ms，"~50 ms 报错"不可能做到。
3. **测试数量的说法不对**："26 项 pipeline_tests 全部通过"只在 Windows 上成立。
4. 原执行单 §7.2 表格仍是"待测"；[docs/opt/README.md](../opt/README.md) 的总表引用了上述错误数字。

### 3.2 重测规程

**必须在 F1、F2 合入后进行**，Windows 与 WSL2 各测一次。

```bash
# 在仓库根目录执行（Windows 用 Git Bash）
BASE=a89a15e
git worktree add -f ../uj-baseline $BASE
(cd ../uj-baseline && cargo build --release -q)
cargo build --release -q

EXE=""; [ "$OS" = "Windows_NT" ] && EXE=".exe"
BIN_BASE=../uj-baseline/target/release/uj$EXE
BIN_HEAD=./target/release/uj$EXE

median() { sort -n | sed -n 2p; }   # 3 个样本取中位数

bench() {  # bench <label> <bin> <命令参数...>（在新复制的目录上运行）
  local label=$1 bin=$2; shift 2
  local samples=""
  for i in 0 1 2 3; do                 # 第 0 次为预热，不计入
    rm -rf /tmp/uj_bench && cp -r tests/contests/tail /tmp/uj_bench
    s=$(date +%s%N); "$bin" "$@" >/dev/null 2>&1; e=$(date +%s%N)
    [ $i -gt 0 ] && samples="$samples $(( (e-s)/1000000 ))"
  done
  echo "$label: samples=[$samples ] median=$(echo $samples | tr ' ' '\n' | median) ms"
}

bench "contest base" $BIN_BASE --format json contest /tmp/uj_bench
bench "contest head" $BIN_HEAD --format json contest /tmp/uj_bench
```

其余场景使用同一套规程（新建目录、预热 1 次、取 3 次中位数），题目构造如下：

| 场景 | 题目构造 | 命令 |
|---|---|---|
| verify 多解法 | `tests/problems/aplusb` 的副本，在 `problem.toml` 末尾追加 `ac.cpp`（main）以及 `wa.cpp`、`tle.cpp`、`re.cpp`（rejected）共 4 个 `[[solutions]]` | `uj verify -p <dir>` |
| pipeline 早期失败 | 与 `test_pipeline_early_invalid_case` 相同的题目（200 个用例，每个生成 20 ms，第 2 个用例非法）。每次运行前删除 `tests/` 与 `.uj/cache/gen` | `uj pipeline -p <dir>` |
| 单提交 judge | `tests/problems/aplusb` 的副本 | `uj judge <dir>/solutions/ac.cpp -p <dir> --format json`，测 **7** 次取中位数（样本波动大） |

**同时记录分阶段耗时**，判断 contest 剩下的时间花在哪里：

- 在 `contest_cmd.rs` 的阶段 A（编译）与阶段 B/C（评测）前后各取一次 `Instant::now()`；
- 只在环境变量 `UJ_TRACE_PHASES=1` 时把耗时 `eprintln!` 到 stderr，默认不输出（stdout 零污染）。

如果编译阶段占大头，说明原执行单设定的 60% 目标在这个 fixture 上不现实，应如实写明，而不是调整测量方法去凑目标。

### 3.3 文档改正

| 文件 | 改动 |
|---|---|
| [ROUND_9_GLOBAL_POOL.md](../opt/ROUND_9_GLOBAL_POOL.md) §3 | 用重测数据替换基准表；基线列写明 commit `a89a15e`；注明测试平台与规程；未达目标的项如实标注"未达标"，并附上分阶段耗时分析 |
| [ROUND_10_CASE_PIPELINE.md](../opt/ROUND_10_CASE_PIPELINE.md) §3.1 | 同上；删除"~50 ms / 12×"；§4 的测试数量改为双平台实测结果 |
| [docs/opt/README.md](../opt/README.md) | 总表中 Round 9 / 10 两行与"安全与正确性保证"一段改用重测数据 |
| [STREAMING_AND_GLOBAL_POOL_EXECUTION.md](../opt/STREAMING_AND_GLOBAL_POOL_EXECUTION.md) §7.2 | 填入"待测"栏，并增加"实测 / 是否达标"两列 |

---

## 4. F4（P1）：pool 单测在仓库 fixture 上运行导致 flaky

### 4.1 现象

WSL2 全量运行时 `pool::tests::test_pool_cancel_no_orphans` 失败一次（`left: 1, right: 0`），单独运行 4 次都通过。

### 4.2 根因

`crates/uj-engine/src/pool.rs` 中 5 个测试都直接以仓库内的 `tests/problems/aplusb` 作为题目目录，会在仓库目录下产生 `.uj/tmp/runs/slot-*` 和 `.uj/tmp/bin`。`test_pool_cancel_no_orphans` 统计这个**共享目录**中的 `slot-*` 数量，同进程中并发运行的其它 pool 测试会让计数变化。

### 4.3 修复

在 `pool.rs` 的 `mod tests` 中增加辅助函数，所有 pool 测试改为在临时副本上运行（`copy_dir_all` 已有，`tempfile` 已在 dev-dependencies 中）：

```rust
/// 将 aplusb fixture 复制到独立临时目录，避免测试之间、测试与仓库之间共享 .uj 状态。
fn temp_aplusb() -> (tempfile::TempDir, PathBuf) {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/problems/aplusb");
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join("aplusb");
    copy_dir_all(&src, &dst).unwrap();
    let _ = fs::remove_dir_all(dst.join(".uj"));
    (tmp, dst)
}
```

同时删除各测试中 `if !p_toml.exists() { return; }` 这类静默跳过：fixture 缺失时测试应当失败，而不是假装通过。

### 4.4 完成标准

WSL2 下 `cargo test -p uj-engine` 连续 5 次全绿；跑完后仓库内 `tests/problems/aplusb/.uj` 没有新增内容。

---

## 5. F5（P1）：两个 pool 单测断言不足

### 5.1 `test_pool_slot_box_cross_problem_clean`

**问题**：唯一的关键断言是 `assert_ne!(runs_1, runs_2)`，比较的是两条拼出来的路径，必然不相等，等于什么都没测。

**改为可观测的串扰检测**：

- 用一份专门的提交源码（写在测试里）：读入 `a b`；如果当前目录存在 `marker` 文件就输出 `LEAK`，否则先创建 `marker` 再输出 `a+b`。
- 两道题（A、B 两份临时副本），`slots: 1, window: 2`，让同一个 worker 先后处理两道题，并在两道题的 box 之间切换。
- 断言：两份报告都是 `Accepted`，任何测试点的输出都不是 `LEAK`。

这个测试同时覆盖了"同题跨用例"和"跨题"两种残留情况（`judge_single_test` 每个用例开始前都会清空 box）。

### 5.2 `test_pool_memory_budget_respected`

**问题**：只断言结果为 AC。aplusb 的内存限制是 512 MiB，预算是 128 MiB，实际测到的是"超额请求按独占运行、不会死锁"，而不是"预算没有被超出"。

**修复**：

1. 把这个测试改名为 `test_pool_oversized_task_runs_exclusively`，与它实际测的内容一致。
2. 在 `budget.rs` 的 `BudgetState` 中增加 `peak: u64`，每次 `acquire` 成功后更新为 `max(peak, used)`；增加 `pub fn peak(&self) -> u64`（开销只有一次比较，可以常驻）。
3. 在 `budget.rs` 中新增并发压力测试 `test_budget_peak_never_exceeds_total`：16 个线程各自循环 50 次，申请 `[1, total]` 内的随机额度，持有 0–2 ms 后释放；结束时断言 `peak() <= total()` 且 `used() == 0`。

---

## 6. F6（P2）：文档与代码不符

| 位置 | 描述 | 实际代码 | 改为 |
|---|---|---|---|
| [AGENT_GUIDE.md](../specs/AGENT_GUIDE.md) §8.2 第 1 条 | 熔断"通过 `CancelToken` 广播" | `author_steps.rs` 使用独立的 `stop_flag`；`CancelToken` 只用于用户中断（这也是原执行单 §6.3 的要求） | "通过内部停止标志让未开始的用例不再启动；用户中断（Ctrl+C）返回 130" |
| AGENT_GUIDE §8.2 第 1 条 | 与 `"skipped"` 状态对照 | 代码中不存在 `skipped` 状态 | 删去该对照，列出 `success` / `failed` / `aborted` 三种状态 |
| [ROUND_10_CASE_PIPELINE.md](../opt/ROUND_10_CASE_PIPELINE.md) §2.1 | "runner 立即触发 `CancelToken` 广播" | 同上 | 同上 |
| [ROUND_9_GLOBAL_POOL.md](../opt/ROUND_9_GLOBAL_POOL.md) §2.2 | "工作窃取""无锁窃取" | 所有 worker 共享一个 crossbeam FIFO 通道，没有窃取 | "共享 FIFO 任务队列 + 准入窗口" |
| ROUND_9 §2.2 | "NUMA 拓扑" | `Topology` 只识别 P 核，没有 NUMA | 删去 |
| ROUND_9 §2.5 | Header 显示"内存预算水位" | `multi.rs` 中没有这项内容 | 删去；或者在 F5 加了 `peak()` 之后通过 `PoolEvent` 上报再显示（另行立项） |
| ROUND_9 §2.1 | `ProblemContext`"缓存输入输出文件清单与沙盒规格" | `ProblemContext` 只有配置、限制和 checker/interactor/manager 二进制；测试列表在 `SubmissionContext.tests` 中 | 按实际字段描述 |

改完后通读两份报告一遍，确保每一项技术断言都能在代码中找到对应实现。

---

## 7. F7（P3）：小偏差

### 7.1 contest 每份提交一个转发线程

`contest_cmd.rs:584` 为每份 Ready 提交起一个线程，把 `JudgeEvent` 转发到 `event_hub`。80 份提交就是 80 个常驻线程，不符合原执行单 §4.4"单收集线程"的设计。

**修复**：

- Human 模式下也启用 `pool_events`；
- 进度监听线程直接消费 `PoolEvent`（其中的 `SubmissionRef.name` 已经是 `"player - prob"`）；
- `PoolSubmission.events` 置为 `None`，删除 `event_hub` 和这些转发线程；
- CE/Failed 的提前完成消息继续由主线程直接打印，保持现状。

### 7.2 多提交 TUI 窄终端降级

原执行单 §8 要求宽度不足 100 列时隐藏右侧槽位面板。`multi.rs:375` 固定是 55/45 分栏。

**修复**：`area.width < 100` 时左侧列表占满整行，不绘制槽位面板。用 ratatui `TestBackend`（宽 80 列）补一个渲染单测，断言渲染不 panic，且缓冲区中不出现槽位面板标题。

---

## 8. 总体验收清单

全部修复合入后，按顺序执行，结果写入本文 §9：

| # | 动作 | 平台 | 通过标准 |
|---|---|---|---|
| 1 | `cargo fmt --check` | 任一 | 无差异 |
| 2 | `cargo clippy --workspace --all-targets -- -D warnings` | 双平台 | 0 警告 |
| 3 | `cargo test --workspace --no-fail-fast` | Windows | 全绿 |
| 4 | `cargo test --workspace --no-fail-fast`，**连续 3 次** | WSL2 | 3 次全绿 |
| 5 | §1.5 中两个测试循环 20 次 | WSL2 | 20/20 |
| 6 | `cargo test -p uj-cli --test evil_tests` | 双平台 | 13/13 |
| 7 | `test_concurrent_untrusted_runs_share_work_dir` | WSL2 | 通过 |
| 8 | `test_contest_fail_fast_skips` | 双平台 | 通过 |
| 9 | §3.2 重测 | 双平台 | 数据写入报告；不达标的项如实标注 |
| 10 | 通读 Round 9 / 10 报告与 AGENT_GUIDE §8 | — | 无与代码不符的描述 |

WSL2 测试应在 Linux 原生文件系统（如 `~/`）中的克隆上运行，不要在 `/mnt/e` 上直接运行，避免跨文件系统带来的权限与性能干扰：

```bash
git clone /mnt/e/Data/Work/校赛出题/uj-cil ~/uj-verify && cd ~/uj-verify
source ~/.cargo/env
cargo test --workspace --no-fail-fast
```

---

## 9. 修复记录

| 编号 | 提交 | 验证结果 | 备注 |
|------|------|----------|------|
| F1 | `HEAD` | **通过**：WSL2 下 `test_concurrent_untrusted_runs_share_work_dir` 通过；20 轮 `test_pipeline_handwritten_cases_validated` + `test_pipeline_early_invalid_case` 20/20 全部通过；`evil_tests` 双平台 13/13 通过。 | 二进制只读挂载点从宿主 `/box/__uj_bin` 迁移到 rootfs `/__uj_bin`；移除宿主工作区临时残留清理，杜绝非可信共享目录读写竞态。 |
| F2 | `HEAD` | **通过**：新增回归测试 `test_contest_fail_fast_skips` 在 Windows 和 WSL2 双平台均稳定通过（验证 19 SKIPPED + 1 TLE）。 | 在编译调度闭包外捕获 `let fail_fast = cli.fail_fast;` 并传入 `SubmissionContext::prepare`，修复 contest 忽略 fail-fast 缺陷。 |
| F3 | `HEAD` | **通过**：双平台严格按 1 预热 + 3 实测（单提交 7 次）实测基线 `a89a15e` 与 HEAD 中位数；如实标注未达标项并补充长尾关键路径物理约束分析。 | 实测数据已同步写入 `ROUND_9_GLOBAL_POOL.md`、`ROUND_10_CASE_PIPELINE.md`、`docs/opt/README.md` 与 `STREAMING_AND_GLOBAL_POOL_EXECUTION.md`。 |
| F4 | `HEAD` | **通过**：`cargo test -p uj-engine` 双平台连续运行全绿，仓库 `tests/problems/aplusb/.uj` 保持零污染。 | 引入 `temp_aplusb()` 独立临时副本，彻底解除并发测试在共享 fixture 目录下的 slot 计数干扰；清理静默跳过。 |
| F5 | `HEAD` | **通过**：`test_pool_slot_box_cross_problem_clean` 采用跨题 `marker` 串扰检测确认无残留；`test_budget_peak_never_exceeds_total` 16 线程压力测试通过。 | 重命名独占测试；在 `budget.rs` 中支持 `peak()` 追踪，验证多线程高并发下峰值内存水位严格小于等于配额。 |
| F6 | `HEAD` | **通过**：通读四份优化文档与技术指南，所有架构与机制描述均与源码严格核对一致。 | 纠正停止标志与 `CancelToken` 说明；去除"工作窃取"、"NUMA 拓扑"、"内存预算水位"；校正 `ProblemContext` 字段说明。 |
| F7 | `HEAD` | **通过**：Human 模式直接消费 `PoolEvent`，消除每提交独立转发线程；ratatui 80 列窄终端单测 `test_narrow_terminal_degradation` 通过。 | 满足原设计单收集线程规范；终端宽度不足 100 列时隐藏右侧槽位面板并全宽展示提交列表。 |
