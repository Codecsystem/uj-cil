# 性能优化 Round 4：出题流水线测试数据生成器多线程并发 (Concurrent Test Data Generation)

## 1. 优化背景与瓶颈分析
在出题工作流中，`uj gen` 依据 `problem.toml` 中的 `[[tests.generators]]` 规则，调用编译好的生成器产生一组组测试数据（`.in`）。
在优化前，[crates/uj-cli/src/author_cmd.rs](file:///e:/Data/Work/校赛出题/uj-cil/crates/uj-cli/src/author_cmd.rs) 的 `run_gen_with_session` 采用单线程双层循环：
- 外层循环遍历不同的生成器规则，内层循环根据 `gen_spec.count` 串行执行数十至上百次生成器进程；
- 复杂题目（如大规模随机树、随机稠密图生成）的 generator 每次调用可能需要几十至几百毫秒，单线程串行生成 100 组数据往往需要消耗 10 秒以上；
- 各测试用例的生成过程完全独立（不同编号与参数），具有完全并发执行的潜力。

## 2. 优化方案实现
1. **确定性编号先行分配 (Deterministic Job Indexing)**：
   - 主线程在排查时预先计算 `max_idx += 1`，将测试点编号（如 `1.in`, `2.in`）与参数在入队时静态绑定；
   - 彻底避免在多工作线程并发时发生文件名冲突或编号错位。
2. **并发 Worker 任务队列**：
   - 采用 `crossbeam-channel` 构建无锁任务通道，根据 CPU 物理/逻辑拓扑自动调整 worker 数量；
   - 各 worker 从通道中竞争获取 `GenJob`，使用专属沙盒实例并发执行 generator 进程。
3. **缓存命中零开销短路**：
   - 在主线程入队前即校验 `Blake3` 缓存条目，已生成的有效测试点直接跳过，零子进程与调度开销。
4. **错误快速熔断与确定性回显**：
   - 任何一个 generator 运行失败（非 0 退出或沙盒超限），原子标志 `stop_flag` 立刻熔断后续任务，主线程按照测试点索引先后确定性展示最早失败的错误。

## 3. 安全性与漏洞防范审查 (Security Audit)
- **并发文件名碰撞防范**：测试用例编号完全由主线程单调递增分配，各工作线程写入的文件互不干扰；
- **沙盒运行安全性**：Generator 以受信任模式运行但依然受到时间上限与内存配额约束，防止有缺陷的生成器死循环阻塞系统；
- **CancelToken 响应性**：用户按下 Ctrl+C 时立即中断执行，各线程安全退出，返回 `EXIT_INTERRUPTED`。

## 4. 验证结果
- **集成测试**：`cargo test -p uj-cli --test authoring_tests` 全部通过。
- **静态检查**：`cargo fmt --check` 与 `cargo clippy --workspace --all-targets -- -D warnings` 零报错、零告警。
- **效率跃升**：至此，出题全流水线三大耗时支柱（`gen`, `validate`, `ans`）已全部实现多核并行化，大规模出题效率实现数倍飞跃。
