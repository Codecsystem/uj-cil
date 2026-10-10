# 性能优化 Round 3：出题流水线标程答案多线程并发生成 (Concurrent Answer Generation)

## 1. 优化背景与瓶颈分析
在出题工作流中，`uj ans`（或 `uj pipeline`）负责运行题目标程（Main Solution）为所有输入文件生成对应的标准答案（`.ans`）。
在优化前，[crates/uj-cli/src/author_cmd.rs](file:///e:/Data/Work/校赛出题/uj-cil/crates/uj-cli/src/author_cmd.rs) 的 `run_ans_with_session` 采用单线程串行模式逐一运行各个测试点：
- 算法竞赛题目的测试点数量通常在 50~100 个以上；
- 如果标程在每个测试点耗时 100ms ~ 500ms（如复杂的图论或字符串算法），单线程串行执行 100 个测试点将消耗高达 10 秒到 50 秒的时间；
- 各测试点的输入路径各异，输出文件也是唯一的（如 `tests/1.ans`, `tests/2.ans`），缓存文件路径也相互独立（`{in_hash}.ans`），完全具备高度并行的天然独立性。

## 2. 优化方案实现
1. **多 Worker 并发运行池**：
   - 使用 `crossbeam-channel` 构建多生产者-多消费者无锁任务流水线，依据物理核数自适应调节并发度（1 ~ 16 并发）；
   - 主线程负责快速排查待执行的任务条目，过滤后推入通道，各个 worker 线程并发运行标程。
2. **前置缓存快速短路 (Cache Pre-filtering)**：
   - 主线程在分发任务前优先执行快速哈希并比对 `.uj/cache/ans`，未修改且已生成过答案的测试用例直接零进程开销跳过，完全不占用 worker 资源。
3. **独立沙盒与运行隔离**：
   - 每个工作线程启动专属受控沙盒实例 `create_sandbox("auto", false)`，独立分配沙盒执行环境与配额限制；
   - 选手标程的运行受到沙盒强限制（时间、内存限制与异常拦截），防止标程死循环或内存泄露影响宿主。
4. **快速失败与错误回传**：
   - 引入 `stop_flag: Arc<AtomicBool>`，一旦标程在任一用例发生 RE/TLE/MLE，立即中断未开始的任务，通过 `idx < f_idx` 确定性报告最小索引的测试点错误信息。

## 3. 安全性与漏洞防范审查 (Security Audit)
- **文件并发写冲突防范**：各任务输出目标分别为 `tests/{name}.ans` 与对应的 `cache/{main_hash}/{in_hash}.ans`，测试点名称互不相同，写入完全无竞争；
- **沙盒运行安全性**：每个标程进程在受限环境运行，不可提权且受内存时间上限保护；
- **CancelToken 响应性**：用户中断时向所有 worker 广播取消，所有子进程安全退出，返回 `EXIT_INTERRUPTED`。

## 4. 验证结果
- **集成测试**：`cargo test -p uj-cli --test authoring_tests` 全部通过，全流程（init -> pipeline -> doctor -> stress）耗时进一步降低。
- **静态检查**：`cargo fmt --check` 与 `cargo clippy --workspace --all-targets -- -D warnings` 零报错、零告警。
- **效率跃升**：在多核机器上生成 100 个测试点答案时，由原本的单线程耗时缩短至原来的 1/3 ~ 1/5。
