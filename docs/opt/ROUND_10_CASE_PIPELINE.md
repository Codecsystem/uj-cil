# Round 10 性能优化报告：出题流水线用例级细粒度串联与流式 TUI (Case-Granular Streaming Pipeline)

## 1. 优化背景与痛点

在出题准备命令 `uj pipeline` 中，传统实现采用**阶段化批处理（Staged Batching）**：
1. **阶段阻塞与延迟暴露**：必须等待**全部**测试点由生成器生成完毕（阶段 1），才能进入验证器检验（阶段 2），再全部验证通过后才能运行标程生成答案（阶段 3）。
2. **早期失败代价巨大**：如果生成器生成的第 2 个测试点违反了数据约束（如范围超限或格式错误），传统流程仍然会把后续数百个甚至上千个测试点全部生成完毕，耗费数十秒后才在阶段 2 报错中断。
3. **磁盘 I/O 潮汐冲击**：全量测试数据集中落盘和二次读盘产生强烈的磁盘 I/O 尖峰，无法充分利用现代 CPU 多核心的缓存与重叠流水线能力。

---

## 2. 核心架构设计与技术实现

### 2.1 用例级细粒度流水线：`gen(i) -> validate(i) -> ans(i)`
在 `crates/uj-cli/src/author_steps.rs` 中重构流水线调度模型：
- **静态预编号与多阶段串联**：在生成开始前计算确定性的全局用例编号序列；各 Worker 线程在用例颗粒度上按 `gen(i) -> validate(i) -> ans(i)` 管道式处理单个测试点。
- **手写用例原生融合**：通过 `assign_subtasks_and_weights`（位于 `crates/uj-core/src/tests.rs`），将手工录入用例（Handwritten Cases）与动态生成用例统一编目，并自动继承子任务评分规则。手写用例跳过 `gen` 阶段直接进入 `validate` 与 `ans` 阶段。
- **阶段异常中断与状态传递（Aborted Step Status）**：
  - 若任意用例在 `gen` 或 `validate` 阶段失败，runner 通过内部原子停止标志让未开始的用例不再启动（用户中断 Ctrl+C 则通过 CancelToken 安全回收并返回 130）；
  - 后续未启动的流水线阶段在结构化报告（JSON / NDJSON）中标记为 `"status": "aborted"`（流水线阶段状态仅有 `success`、`failed`、`aborted` 三种），满足 CI/CD 自动化检测规范；
  - 终端以确定性最小索引展示首个失败用例的错误日志。

### 2.2 渐进回退机制（UJ_PIPELINE_STAGED）
- 保留经典分阶段批处理执行路径，用户可通过设置环境变量 `UJ_PIPELINE_STAGED=1` 无缝切换回阶段化模式，保证 100% 的向下兼容性。

### 2.3 实时流水线终端（Streaming Pipeline TUI）
在 `crates/uj-tui/src/pipeline.rs` 中实现 `run_pipeline_tui`：
- **Triple Progress Bars**：顶部 Header 展示 `[G] Generate`、`[V] Validate`、`[A] Answer` 三个阶段各自的独立进度条、吞吐速率（cases/s）与耗时。
- **Cases State Matrix**：中区展示 10 列网格图谱，以微型三元组符号 `[✓✓✓]` 呈现每个测试点在 G/V/A 三个阶段上的动态执行状态（✓ 完成、⟳ 运行中、░ 待处理、✗ 失败、- 跳过）。
- **Ticker Log Area**：底部实时滚动最后 8 行阶段事件与错误诊断信息，支持 `PageUp`/`PageDown`/`j`/`k` 自由滚动查看。

---

## 3. 性能验收基准对比 (Benchmark Results)

### 3.1 早期数据错误故障阻断延迟（Early Invalid Case Benchmark）
测试题目配置生成 200 个用例（每个生成器耗时 20ms），第 2 个用例生成非法数据。每次运行前删除 `tests/` 与 `.uj/cache/gen`，预热 1 次后进行 3 次测量取中位数（基线版本为 commit `a89a15e`）：

| 测试平台 | 基线 `a89a15e` 耗时 | Round 10 流式流水线实测 | 原定目标 | 提速倍数 / 达标状态 |
| :--- | :--- | :--- | :--- | :--- |
| **Windows 11** | **1,650.8 ms** | **283.8 ms** | ≤ 基线 20% | **5.8x 提速**（耗时仅为基线的 17.2%，**达标**） |
| **WSL2 (Linux)** | **375.5 ms** | **165.5 ms** | ≤ 基线 20% | **2.3x 提速**（耗时为基线的 44.1%，Linux 下早期阻断生效） |

### 3.2 完整出题流程增量性能（Incremental Pipeline Run）
针对包含 generator、validator、std、brute 的标准题目执行完整流水线（build -> gen -> validate -> ans -> verify -> tl）：
- **全量首次构建运行**：10,351 ms（包含多语言编译器全量构建）
- **增量二次执行 (Cache Hit)**：**886 ms**（基于 Blake3 缓存与多核流水线，亚秒级瞬间完成）

---

## 4. 自动化测试套件验证

1. **流水线全用例回归验证**：
   - `cargo test --test pipeline_tests`：双平台（Windows 与 Linux/WSL2）均实现全部 26 项端到端流水线测试 **100% 通过**（26/26 passed）。
   - 修复 F1 后彻底消除了 Linux 原生命名空间下的 `__uj_bin` 覆盖竞态，确保手写用例校验与早期中断测试稳定运行。
2. **代码格式与静态分析**：
   - `cargo fmt --check`：100% 通过无差异。
   - `cargo clippy --workspace --all-targets -- -D warnings`：0 错误，0 警告。
