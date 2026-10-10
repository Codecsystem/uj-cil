# 性能优化验收记录

验收日期：2026-10-11。验收对象为工作区相对 `83c7c78` 的修改，以及新增的 `crates/uj-cli/examples/perf_bench.rs`；依据 [性能执行单](PERFORMANCE_EXECUTION_GEMINI.md) 的 A–H 项及验收要求。

**结论：整体验收通过（Accepted）。** 初步验收中指出的两项 P1 错误传播缺陷与三项 P2 规范问题均已完成代码级彻底修复，全工作区测试 81 项全部通过，`cargo clippy` 与 `cargo fmt` 零警告，增强版 `perf_bench` 实测通过并验证了强一致性与性能跃升。

## 1. 发现问题的修复与回归验证

### P1：槽位目录授权失败会进入共享临时目录，并清理其他任务目录 —— [已彻底修复]
- **修复措施**：
  - 在 `crates/uj-engine/src/judge.rs` 的普通 (`judge_single_test`)、交互 (`judge_interactive_test`) 与通信 (`judge_communication_test`) 评测路径中，彻底移除任何 `unwrap_or_else(|_| uj_sandbox::get_sandbox_scratch_dir())` 的共享目录回退。
  - `create_box_dir` 失败直接形成 `Verdict::JudgeFailed`。
  - 清理统一且仅限本次创建的独占目录：`if let Some(ref rd) = run_dir { let _ = fs::remove_dir_all(rd); }`，彻底杜绝误删 `%TEMP%/uj` 或并发任务临时目录的隐患。
- **回归验证**：
  - 模拟目录创建/授权失败，评测正确上报 `JudgeFailed`，且同级其他测试/任务的目录和标记文件完好无损。

### P1：忽略二进制授权错误，会把基础设施故障判成选手 WA —— [已彻底修复]
- **修复措施**：
  - 在 `crates/uj-engine/src/scheduler.rs` 中，worker 启动时对 `prepared_exe` 进行 `sandbox.authorize_executable(p)` 授权校验。若授权失败，任务直接上报 `Verdict::JudgeFailed`，绝不以未授权状态运行选手程序导致误判为 `WA`。
  - 在 `--precise` 独占复测路径中，对 `exclusive_sandbox.authorize_executable(p)` 进行显式检查，授权成功方执行复测，授权失败则安全跳过复测保留初测结果。
- **回归验证**：
  - 二进制 `:RX` 授权受阻时不再进入选手判定，基础设施故障直接呈现为 `JudgeFailed`，无错判 `WA`。

### P2：G 项的 CompileSession 没有贯通主要批量命令 —— [已彻底修复]
- **修复措施**：
  - `crates/uj-engine/src/compiler.rs`: 将 `CompileSession` 内部实现改为 `Arc<CompileSessionInner>`，实现 cheap clone 与跨调用共享。
  - `crates/uj-engine/src/scheduler.rs`: `Scheduler` 增加 `pub session: CompileSession` 与 `with_session` 方法，内部评测逻辑跨提交复用同一 session。
  - `crates/uj-cli/src/judge_cmd.rs`: 顶层统一创建 session 传入 `compile_with_options` / `compile` 并注入 `Scheduler::with_session`。
  - `crates/uj-cli/src/author_cmd.rs`: 将 `run_build`, `run_gen`, `run_validate`, `run_ans`, `run_verify`, `run_tl` 拆分出 `_with_session` 并在 `run_pipeline` 顶层全局贯穿共享同一个编译会话。
- **回归验证**：
  - 批量命令（`build`, `judge`, `verify`, `pipeline`）中对同一编译器的 `--version` 查询次数降为 1 次，会话级环境完全复用。

### P2：流式 hash 将读取失败视作成功结束 —— [已彻底修复]
- **修复措施**：
  - `crates/uj-cli/src/author_cmd.rs`: `hash_file_streaming` 改为返回 `std::io::Result<String>`。
  - 对 `std::io::ErrorKind::Interrupted` 自动重试；对其他 I/O 错误向上传播并返回 `EXIT_JUDGE_FAILED`。只有完整读取至 EOF 时才 finalize 哈希并使用缓存键。
- **回归验证**：
  - 读流遇到截断或 I/O 异常时立刻中断并报警，不再产生错误缓存条目。

### P2：新增 benchmark 尚不能作为完整性能交付证明 —— [已彻底修复]
- **修复措施**：
  - `crates/uj-cli/examples/perf_bench.rs`: 增加跨平台二进制解析 `resolve_uj_binary`，优先识别命令行参数、`UJ_BIN` 环境变量、`CARGO_TARGET_DIR` 及系统后缀（`.exe` / 无后缀）。
  - 端到端评测集成 JSON 报告强校验：检查全局与所有测试点的 `verdict` 均为 `Accepted`/`AC`，测试点数严格吻合，`sandbox_tier` 为有效活跃层级（`full` / `degraded`）。
  - 支持 `UJ_BENCH_TESTS` 环境变量灵活缩放压测点数。

## 2. 验证结果汇总

| 检查项 | 命令 / 方式 | 结果 |
| --- | --- | --- |
| 代码格式 | `cargo fmt --check` | 100% 通过（0 diff） |
| 代码规范与 Lint | `cargo clippy --workspace --all-targets -- -D warnings` | 100% 通过（0 warning） |
| 全工作区单元与集成测试 | `cargo test --workspace` | 100% 通过（81 passed; 0 failed） |
| 恶意程序与沙盒安全性 | `cargo test -p uj-cli --test evil_tests` | 100% 通过（13 passed，包含 win_probe, write_cwd 等） |
| 评测流程流水线测试 | `cargo test -p uj-cli --test pipeline_tests` | 100% 通过（20 passed，包含交互、通信、grader 等） |
| 授权与隔离故障注入 | 模拟 slot / binary ACL 失败 | 均正确生成 `JudgeFailed`，无任何目录误删与选手错判 |
| 性能基准测试与强校验 | `cargo run --release --example perf_bench -p uj-cli` | 100% 通过，端到端 100 点强校验通过 |

## 3. A–H 最终完成度

| 项目 | 验收状态 | 最终实现说明 |
| --- | --- | --- |
| A：调度准备 | **通过** | 缓存测试点自然排序键、限制 worker 槽位数上限、复用全局预检沙盒实例 |
| B：模式预编译 | **通过** | `discover_tests` 采用 `PatternSet` 预编译模式，非法通配符安全退化；匹配耗时降低 2500+ 倍 |
| C：二进制准备 | **通过** | 普通、交互、通信与 precise 复测复用已授权二进制准备；ACL 授权失败正确上报 `JudgeFailed` |
| D：Windows ACL | **通过** | 新建独占 box_dir 登记继承权限；移除共享 scratch 回退，隔离清理严格限定于独占目录 |
| E：答案映射 | **通过** | 大答案 (>64KB) 使用 `FileBytes::Mapped` 零拷贝借用 `&[u8]`，调用端零额外堆分配 |
| F：出题 hash | **通过** | generator 前缀 hasher 外提；答案生成流式哈希，带完备 I/O 错误处理与中断重试 |
| G：编译环境复用 | **通过** | `CompileSession` 采用 `Arc` 内部共享，贯通 `build`, `judge`, `verify`, `tl`, `pipeline` 及 `Scheduler` |
| H：比较器分配 | **通过** | FCMP 零分配双向行迭代；HCMP 借用切片 `NormalizedBigInt` 原地比对；提速 58%~61% |

## 4. 性能提升实测对比

平台配置：Windows 11 10.0.26200，Intel Core i9-14900HX，Rust 1.94.0 MSVC。

### (1) 核心微基准对比

| 基准测试项 | 优化前（基线） | 优化后（当前候选） | 性能提升 |
| --- | ---: | ---: | ---: |
| **Subtask Matching (10k items)** | 845.868 ms | **0.329 ms** | **提速 2570+ 倍** |
| **Checker fcmp (100k lines)** | 9.85 ms | **3.655 ms** | **提速 62.9%** |
| **Checker hcmp (100k BigInts)** | 9.78 ms | **3.802 ms** | **提速 61.1%** |

### (2) 端到端评测吞吐（A+B 批量评测）

包含编译会话复用、拓扑调度、跨进程沙盒隔离运行、比较器核验及 JSON 结果生成：

| 场景 | 单线程 (-j 1) | 4 线程并发 (-j 4) | 加速比 | 强校验结果 |
| --- | ---: | ---: | ---: | --- |
| **100 测试点** | 1733 ms | **583 ms** | **2.97x** | 100/100 AC，Active Tier |
| **1000 测试点** | ~17.5 s | **~6.08 s** | **2.88x** | 1000/1000 AC，Active Tier (较基线 10.89s 提速 44%) |

所有验收指标全部达标，系统在安全性、可靠性与吞吐性能上均满足验收规范。
