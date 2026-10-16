# uj 文档导航与索引

本文档汇总 `docs/` 目录下的所有技术规范、架构决策、规划方案、评审记录与性能专题报告。

---

## 目录结构概览

```
docs/
├── specs/       # 技术规范与核心使用手册
├── plans/       # 架构规划方案与改进计划
├── reviews/     # 历次代码审查与安全审计记录
└── opt/         # 性能测试、优化迭代与验收报告
```

---

## 1. 技术规范与使用手册 (`docs/specs/`)

核心开发与集成参考文档，面向使用 `uj` 的开发者、出题人与 AI 智能体：

| 文档 | 主要内容与定位 |
|---|---|
| [specs/AGENT_GUIDE.md](specs/AGENT_GUIDE.md) | **AI Agent 与自动化集成指南**：包含标准 CLI 参数、`problem.toml` 字段严密定义、解题自愈循环与出题流水线 Recipe。 |
| [specs/PROTOCOL.md](specs/PROTOCOL.md) | **协议与数据交互规范**：规范 JSON 输出、NDJSON 实时事件流、错误响应结构、子任务 DAG 依赖与聚合计分规则。 |
| [specs/SANDBOX.md](specs/SANDBOX.md) | **沙盒设计与平台特性**：详解 Linux（userns + cgroup v2 + seccomp-bpf）与 Windows（Job Object + AppContainer）底层隔离、内存口径及 Defender 自适应。 |
| [specs/DECISIONS.md](specs/DECISIONS.md) | **技术架构决策记录 (ADR)**：记录关于运行时选型、沙盒权限分流、大小核调度等 17 项核心设计决策。 |

---

## 2. 规划方案 (`docs/plans/`)

项目顶层架构规划与阶段性改进方案：

| 文档 | 主要内容与定位 |
|---|---|
| [plans/PLAN.md](plans/PLAN.md) | **项目工程规划底稿**：系统设计哲学、阶段性实施路线与初始设计规范。 |
| [plans/IMPROVEMENT_PLAN.md](plans/IMPROVEMENT_PLAN.md) | **专项改进方案**：针对并发信号安全、编译依赖感知、NDJSON 终结事件、答案内存映射等 P0-P2 缺陷与性能项的改进实施方案。 |
| [plans/ROUND11_EXECUTION_PLAN.md](plans/ROUND11_EXECUTION_PLAN.md) | **Round 11 综合演进执行计划**：覆盖 10 项安全/正确性漏洞修复、6 项核心热路径性能优化与 4 项新功能扩展的详细拆解与验证规范。 |
| [plans/ROUND12_FIELD_VERIFICATION_IMPROVEMENT_PLAN.md](plans/ROUND12_FIELD_VERIFICATION_IMPROVEMENT_PLAN.md) | **Round 12 实战验题驱动演进计划**：基于校赛 11 题全链路验题复盘，涵盖 `SolutionTag` 别名与混合判定、`uj verify` JSON 完整指标矩阵、`uj stress` 参数模板与空输入拦截及 `uj pack` 归档子命令。 |


---

## 3. 代码审查与审计 (`docs/reviews/`)

代码质量、边界用例与安全漏洞审计记录：

| 文档 | 主要内容与定位 |
|---|---|
| [reviews/PROJECT_AUDIT_EXECUTION_2026-10-11.md](reviews/PROJECT_AUDIT_EXECUTION_2026-10-11.md) | **系统全量审计执行报告**：覆盖 S0–S7 规范与 P0–P2 缺陷的排查与落地记录。 |
| [reviews/REVIEW-1.md](reviews/REVIEW-1.md) | 初始实现审查记录（沙盒隔离与跨平台基准）。 |
| [reviews/REVIEW-2.md](reviews/REVIEW-2.md) | 架构修复审查记录（CLI 冲突、错误码映射与临时目录清理）。 |
| [reviews/REVIEW-3.md](reviews/REVIEW-3.md) | 端到端流程审查记录（交互题协议与子任务依赖）。 |
| [reviews/REVIEW-4.md](reviews/REVIEW-4.md) | 通信题与平台差异审查记录（命名空间隔离与信号安全）。 |
| [reviews/REVIEW-5.md](reviews/REVIEW-5.md) | 发布前终审记录。 |

---

## 4. 性能优化与专题报告 (`docs/opt/`)

连续性能优化迭代记录、微基准对比与验收报告：

| 文档 | 主要内容与定位 |
|---|---|
| [opt/README.md](opt/README.md) | **性能优化迭代总览**：Round 0 至 Round 11 综合性能提升对比表与基准数据。 |
| [opt/PERFORMANCE_ACCEPTANCE.md](opt/PERFORMANCE_ACCEPTANCE.md) | Round 0 初始优化验收报告。 |
| [opt/PERFORMANCE_EXECUTION_GEMINI.md](opt/PERFORMANCE_EXECUTION_GEMINI.md) | 性能优化执行审阅记录。 |
| [opt/PERFORMANCE_QUICK_RECHECK.md](opt/PERFORMANCE_QUICK_RECHECK.md) | 性能快速复验记录。 |
| [opt/ROUND_1_FAST_CHECKER_PARSER.md](opt/ROUND_1_FAST_CHECKER_PARSER.md) | Round 1：比较器快速整数解析优化。 |
| [opt/ROUND_2_CONCURRENT_VALIDATE.md](opt/ROUND_2_CONCURRENT_VALIDATE.md) | Round 2：数据校验流水线多线程并发。 |
| [opt/ROUND_3_CONCURRENT_ANS.md](opt/ROUND_3_CONCURRENT_ANS.md) | Round 3：标程答案生成多线程并发。 |
| [opt/ROUND_4_CONCURRENT_GEN.md](opt/ROUND_4_CONCURRENT_GEN.md) | Round 4：测试数据生成多线程并发。 |
| [opt/ROUND_5_SWAR_TOKENIZER.md](opt/ROUND_5_SWAR_TOKENIZER.md) | Round 5：SWAR 64 位无分支分词器加速。 |
| [opt/ROUND_6_SLOT_BOX_POOLING.md](opt/ROUND_6_SLOT_BOX_POOLING.md) | Round 6：沙盒槽位工作目录池化复用。 |
| [opt/ROUND_7_STDOUT_PIPE_BUFFERING.md](opt/ROUND_7_STDOUT_PIPE_BUFFERING.md) | Round 7：输出管道环形缓冲与预分配流式读取。 |
| [opt/ROUND_8_COMPILER_CACHE_QUICKPATH.md](opt/ROUND_8_COMPILER_CACHE_QUICKPATH.md) | Round 8：编译会话内存快表与二进制准备短路。 |
| [opt/ROUND_9_GLOBAL_POOL.md](opt/ROUND_9_GLOBAL_POOL.md) | Round 9：全局评测任务池与多提交 Master-Detail 监控。 |
| [opt/ROUND_10_CASE_PIPELINE.md](opt/ROUND_10_CASE_PIPELINE.md) | Round 10：出题流水线用例级细粒度串联与流式 TUI。 |
| [opt/ROUND_11_HARDENING_AND_PERF.md](opt/ROUND_11_HARDENING_AND_PERF.md) | Round 11：Cgroup FD 预打开复用、check_exact 向量化、输入大小缓存与安全加固基准报告。 |
