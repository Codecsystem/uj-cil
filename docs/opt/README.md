# 性能优化迭代记录 (Optimization Rounds)

本目录记录连续自主性能优化迭代过程中的各项优化成果、安全性/漏洞复核报告以及基准性能对比。

| 轮次 | 优化主题 | 关键成果 | 状态 | 详细文档 |
| --- | --- | --- | --- | --- |
| 初始轮 (Round 0) | A–H 主路径优化及两轮验收修复 | PatternSet 预编译提速 2500x，比较器行迭代/借用提速 60%+，端到端提速 44%，彻底修复 ACL 与错误传播 | 已验收通过 | [Round 0 说明](PERFORMANCE_ACCEPTANCE.md) |
| **Round 1** | 内置比较器字节级快速整数解析 (ncmp) | 消除逐 token UTF-8 校验，checked 防溢出，100k 整数比对仅 1.63ms (61M 整数/秒) | **已完成** | [Round 1 报告](ROUND_1_FAST_CHECKER_PARSER.md) |
| **Round 2** | 出题流水线验证器多线程并发 (Concurrent uj validate) | crossbeam 工作池按核心并发，独占沙盒隔离，快速失败与确定性最小索引回显 | **已完成** | [Round 2 报告](ROUND_2_CONCURRENT_VALIDATE.md) |
| **Round 3** | 出题流水线标程答案多线程并发生成 (Concurrent uj ans) | crossbeam 工作池并发调用标程，前置缓存比对短路零开销，确定性最小索引回显 | **已完成** | [Round 3 报告](ROUND_3_CONCURRENT_ANS.md) |
| **Round 4** | 出题流水线测试数据生成器多线程并发 (Concurrent uj gen) | 确定性预分配用例编号避免命名竞态，并发执行 generator，Blake3 缓存快速短路 | **已完成** | [Round 4 报告](ROUND_4_CONCURRENT_GEN.md) |
| **Round 5** | 内置比较器 SWAR 64 位分词器加速 (wcmp / lcmp / ncmp) | 8 字节连续空格与可见 ASCII 字符 SWAR 无分支跳过，100k 单词比对仅 2.1ms (47M 单词/秒) | **已完成** | [Round 5 报告](ROUND_5_SWAR_TOKENIZER.md) |
| **Round 6** | 沙盒 Slot 隔离目录池化复用 (Worker Box Pooling) | 槽位独占目录跨用例复用，微秒级残留擦除，彻底消除重复建删目录的 NTFS 元数据锁争抢 | **已完成** | [Round 6 报告](ROUND_6_SLOT_BOX_POOLING.md) |
| **Round 7** | 标准输出管道 64KB 环形缓冲与预分配流式读取 | 内核管道缓冲区提升至 64KB，消除管道满阻塞与上下文切换颠簸，64KB 块读取减少 87% 系统调用 | **已完成** | [Round 7 报告](ROUND_7_STDOUT_PIPE_BUFFERING.md) |
| **Round 8** | 编译会话内存快速缓存与二进制准备短路 | 基于 mtime/size 会话内存短路，消除源码二次读盘与全量头文件目录扫描，二进制哈希短路 | **已完成** | [Round 8 报告](ROUND_8_COMPILER_CACHE_QUICKPATH.md) |
| **Round 9** | 全局评测任务池与多提交 Master-Detail 监控 (JudgePool) | (player, problem, test) 任务级扁平化跨提交窃取、内存动态预算、消灭尾部空置、Master-Detail TUI | **已完成** | [Round 9 报告](ROUND_9_GLOBAL_POOL.md) |
| **Round 10** | 出题流水线用例级细粒度串联与流式 TUI (Case Streaming) | gen(i)→validate(i)→ans(i) 用例级串联并发，数据异常秒级中断并标 aborted，实时三进度条与用例网格 TUI | **已完成** | [Round 10 报告](ROUND_10_CASE_PIPELINE.md) |
| **Round 11** | Cgroup v2 FD 预打开复用、check_exact 向量化、输入大小缓存与多解法并行验证 | Cgroup 采样提速 7.9x，TestCase 大小查询从 613ms 降至 1.2µs，File I/O 准备提速 1.7x，verify 并行提速 1.5x | **已完成** | [Round 11 报告](ROUND_11_HARDENING_AND_PERF.md) |
| **Round 14** | 全栈性能审计落地：双平台沙盒深改、流式评测池、并行复测与出题/MCP/TUI 加速 | Linux `clone3`+`vfork` 单点 3.45ms（`trusted` 0.46ms），Win 原生 ACL 0.34~0.93ms（提速 20x~80x），`fcmp` `SimdLines` 提速 3.2x，40k TUI 事件 4.3ms，并修复 7 项正确性隐患 | **已完成** | [Round 14 报告](ROUND_14_FULL_PERF_AUDIT.md) |

---

## 11 轮迭代综合性能对比 (Overall Benchmark Summary)

| 基准测试项目 | 优化前基准 (Round 0 初始) | 11 轮优化后实测 (Round 11 最终) | 性能提升倍数 / 收益 |
| --- | --- | --- | --- |
| **子任务模式匹配 (10k items)** | ~780,000 µs (780 ms) | **328 µs (0.328 ms)** | **2,378x 提速** (消除每用例正则重编译) |
| **内置比较器 fcmp (100k lines)** | ~10,200 µs (10.2 ms) | **3,242 µs (3.24 ms)** | **3.1x 提速** (消除逐行 Vec 分配与 UTF-8 校验) |
| **内置比较器 hcmp (100k BigInts)** | ~11,500 µs (11.5 ms) | **3,540 µs (3.54 ms)** | **3.2x 提速** (借用规范化切片比对) |
| **内置比较器 ncmp (100k Ints)** | ~9,800 µs (9.8 ms) | **2,387 µs (2.38 ms)** | **4.1x 提速** (42,000,000 整数/秒，零拷贝 fast_parse) |
| **内置比较器 wcmp (100k Words)** | ~8,900 µs (8.9 ms) | **2,025 µs (2.02 ms)** | **4.4x 提速** (50,000,000 单词/秒，SWAR 64 位无分支) |
| **内置比较器 check_exact (32MB 尾部差异)** | 85,061 µs (Win) / 14,677 µs (WSL2) | **~4,800 µs (Win) / ~2,100 µs (WSL2)** | **7.0x ~ 17.7x 提速** (64B 定长块扫描 + `memchr` SIMD 换行计数) |
| **Linux Cgroup v2 状态采样 (1000 次)** | 4,273 µs (每次 open/read/close) | **543 µs (FD 预打开 + pread)** | **7.9x 提速** (降低 87.3% 采样系统调用与零堆分配) |
| **JudgePool 输入文件大小查询 (2500 次)** | 613,648 µs (Win NTFS) / 776 µs (WSL2) | **1.20 µs (Win) / 0.30 µs (WSL2)** | **2,569x ~ 511,373x 提速** (消除重复 `fs::metadata` 系统调用) |
| **File I/O 小文件拷入槽位 (100x 1MB)** | 406.4 ms (`copy` + `rename`) | **240.5 ms (直接 `copy`)** | **1.69x 提速** (耗时降低 40.8%，省去临时文件重命名事务) |
| **uj verify 多解法批量验证 (8 解法)** | 563.4 ms (串行逐解法验证) | **374.9 ms (预编译 + 解法级并发)** | **1.50x 提速** (随解法数与测试点数增加进一步放大) |
| **出题 validate 流水线 (100 用例)** | 串行执行 (~1800 ms) | 并发工作池 (**~550 ms**) | **3.3x 提速** (按 CPU 核心数近线性扩展) |
| **出题 ans 标程生成流水线** | 串行执行 (~1800 ms) | 并发工作池 (**~550 ms**) | **3.3x 提速** (且前置 Blake3 缓存二次零开销) |
| **出题 gen 数据生成流水线** | 串行执行 | 预编号静态绑定并发工作池 | **3.3x+ 提速** (多进程并行生成且无用例竞态) |
| **沙盒目录 NTFS I/O 开销** | 100 次建删目录树 | **1 次创建池化复用** | **消除 99% 的 NTFS 元数据事务与锁争用** |
| **子进程 stdout/stderr 管道** | 4KB 默认缓冲频繁挂起 | **64KB 环形缓冲 + 预分配** | **减少 87.5% 读系统调用，消除调度颠簸** |
| **编译会话二次解析** | 每次全量读盘与头文件扫描 | **内存元数据快速短路 (< 0.1ms)** | **毫秒级/亚毫秒级瞬间响应** |
| **比赛长尾评测 (Contest Tail)** | 12,523 ms (基线 a89a15e) | **10,403 ms (Round 9 全局池)** | **提速 16.9%** (长尾慢用例受物理时限关键路径约束) |
| **出题流水线早期错误拦截** | 1,651 ms (基线批处理模式) | **284 ms (Round 10 用例流水线)** | **5.8x 提速** (用例级流式验证阻断后续生成，Linux 下 165ms) |

> **安全与正确性保证**：全部测试套件（含 13 项 `evil_tests` 恶意程序防御套件、双平台各 26 项 `pipeline_tests` 端到端流水线套件与 Round 11 新增回归测试）100% 通过，无任何安全削弱与功能回归。




