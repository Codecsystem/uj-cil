# 性能优化迭代记录 (Optimization Rounds)

本目录记录连续自主性能优化迭代过程中的各项优化成果、安全性/漏洞复核报告以及基准性能对比。

| 轮次 | 优化主题 | 关键成果 | 状态 | 详细文档 |
| --- | --- | --- | --- | --- |
| 初始轮 (Round 0) | A–H 主路径优化及两轮验收修复 | PatternSet 预编译提速 2500x，比较器行迭代/借用提速 60%+，端到端提速 44%，彻底修复 ACL 与错误传播 | 已验收通过 | [Round 0 说明](../../docs/PERFORMANCE_ACCEPTANCE.md) |
| **Round 1** | 内置比较器字节级快速整数解析 (ncmp) | 消除逐 token UTF-8 校验，checked 防溢出，100k 整数比对仅 1.63ms (61M 整数/秒) | **已完成** | [Round 1 报告](ROUND_1_FAST_CHECKER_PARSER.md) |

