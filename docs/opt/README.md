# 性能优化迭代记录 (Optimization Rounds)

本目录记录连续自主性能优化迭代过程中的各项优化成果、安全性/漏洞复核报告以及基准性能对比。

| 轮次 | 优化主题 | 关键成果 | 状态 | 详细文档 |
| --- | --- | --- | --- | --- |
| 初始轮 (Round 0) | A–H 主路径优化及两轮验收修复 | PatternSet 预编译提速 2500x，比较器行迭代/借用提速 60%+，端到端提速 44%，彻底修复 ACL 与错误传播 | 已验收通过 | [Round 0 说明](../../docs/PERFORMANCE_ACCEPTANCE.md) |
| **Round 1** | 内置比较器字节级快速整数解析 (ncmp) | 消除逐 token UTF-8 校验，checked 防溢出，100k 整数比对仅 1.63ms (61M 整数/秒) | **已完成** | [Round 1 报告](ROUND_1_FAST_CHECKER_PARSER.md) |
| **Round 2** | 出题流水线验证器多线程并发 (Concurrent uj validate) | crossbeam 工作池按核心并发，独占沙盒隔离，快速失败与确定性最小索引回显 | **已完成** | [Round 2 报告](ROUND_2_CONCURRENT_VALIDATE.md) |
| **Round 3** | 出题流水线标程答案多线程并发生成 (Concurrent uj ans) | crossbeam 工作池并发调用标程，前置缓存比对短路零开销，确定性最小索引回显 | **已完成** | [Round 3 报告](ROUND_3_CONCURRENT_ANS.md) |
| **Round 4** | 出题流水线测试数据生成器多线程并发 (Concurrent uj gen) | 确定性预分配用例编号避免命名竞态，并发执行 generator，Blake3 缓存快速短路 | **已完成** | [Round 4 报告](ROUND_4_CONCURRENT_GEN.md) |




