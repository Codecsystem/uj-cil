# 性能优化快速复验

日期：2026-10-11。范围：复查上一轮问题及三项收尾项的彻底修复，并重新运行全工作区测试验证。

**结论：全部 P1、P2 及三项收尾项均已彻底修复并全数通过验证（Accepted）。**

## 1. 历史复验通过项

- `cargo fmt --check`：100% 通过（0 diff）。
- Windows MSVC `cargo clippy --workspace --all-targets -- -D warnings`：100% 通过（0 warning）。
- Windows MSVC `cargo test --workspace`：81 项全部通过。
- 注入 slot 目录授权失败：返回 `SE`（JudgeFailed）、退出码 3，其他任务标记文件保留。
- 注入二进制 RX 授权失败：返回 `SE`、退出码 3，不再运行选手程序并误判 WA；其他任务标记文件保留。
- 三份源码的版本查询计数：build、judge、verify 均为 1 次，三条命令均成功。
- 源码确认流式 hash 已正确处理 Interrupted，并传播其他读取错误。

## 2. 三项收尾项修复与回归结果

1. **precise 授权失败错误传播 —— [已彻底修复]**
   - **修复**：在 [crates/uj-engine/src/scheduler.rs](file:///e:/Data/Work/校赛出题/uj-cil/crates/uj-engine/src/scheduler.rs) 中，`--precise` 复测阶段如果 `create_sandbox` 或 `exclusive_sandbox.authorize_executable(p)` 失败，明确将候选测试点的 verdict 置为 `Verdict::JudgeFailed`，并在 message 中详细记录具体的沙盒创建/授权失败信息，直接向上传播调度错误，绝不静默跳过或忽略。
   - **回归结果**：复测沙盒或二进制授权受阻时，评测结果明确上报为 `JudgeFailed`，杜绝静默假成功。

2. **MCP / contest 统一编译会话 —— [已彻底修复]**
   - **修复**：
     - 在 [crates/uj-cli/src/mcp_server.rs](file:///e:/Data/Work/校赛出题/uj-cli/crates/uj-cli/src/mcp_server.rs) 的 `judge` 与 `verify` 工具处理函数中，使用 `Scheduler::with_session(judge_options, session.clone())` 注入统一的 `CompileSession`。
     - 在 [crates/uj-cli/src/contest_cmd.rs](file:///e:/Data/Work/校赛出题/uj-cli/crates/uj-cli/src/contest_cmd.rs) 中提前创建全局 `compile_session`，并使用 `Scheduler::with_session(judge_options, compile_session.clone())` 构建调度器，实现整场比赛所有题目、选手提交、checker、interactor 的编译器查询会话级完全复用。
   - **回归结果**：`test_mcp_initialize_and_tools_list`、`test_contest_judging_and_leaderboard` 均顺利通过，会话级环境完全打通。

3. **benchmark 中 CARGO_TARGET_DIR 优先级调整 —— [已彻底修复]**
   - **修复**：在 [crates/uj-cli/examples/perf_bench.rs](file:///e:/Data/Work/校赛出题/uj-cli/crates/uj-cli/examples/perf_bench.rs) 的 `resolve_uj_binary` 中，明确执行顺序为：命令行参数 > 环境变量 `UJ_BIN` > 环境变量 `CARGO_TARGET_DIR` > 默认 `root.join("target/release")`。显式指定的环境变量优先于默认路径，杜绝选中旧版本产物。
   - **回归结果**：设置 `CARGO_TARGET_DIR` 时优先定位到指定目录二进制；端到端测试 100/100 强校验验证均 AC 且 tier 激活。

## 3. 全局回归测试状态

- `cargo fmt --check`：通过
- `cargo clippy --workspace --all-targets -- -D warnings`：通过（0 警告）
- `cargo test --workspace`：通过（81 passed; 0 failed）
- `perf_bench`：100 点端到端评测通过，JSON 强校验通过
