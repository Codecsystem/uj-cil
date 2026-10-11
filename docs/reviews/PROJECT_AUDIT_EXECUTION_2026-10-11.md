# uj-cil 项目审查与执行计划

**审查日期：** 2026-10-11<br>
**基线提交：** <code>7caf5a310a8a6c9d3069c55992706f20216ddd05</code><br>
**审查范围：** 工作区中的七个 Rust crate、CLI/MCP/TUI、Windows 与 Linux 沙盒实现、配置与语言定义、集成测试、CI/安装脚本及现有设计文档。
**产出目的：** 给维护者一份可以分阶段实施、验收和发布的工程计划。

> 本文不是网络安全审计报告，也不以渗透测试为目标。文中涉及目录隔离、进程组和结果文件的内容，是在线评测系统保证“分数正确、资源限制有效、评测不挂起”的工程前提，按可靠性和判题完整性处理。

## 1. 结论与发布建议

项目的基础分层较清楚：核心模型、执行沙盒、调度器、检查器、报告器与 CLI 均已具备雏形；标准题的基本编译、运行、比较和报表链路可以工作。现有测试在两端均大面积通过，说明当前代码不是不可用状态。

但在特殊题型、文件输出、缓存可信度、跨进程生命周期和机器可读协议上存在会直接造成错误判分、假成功或长时间卡住的问题。其中两个最高优先级问题已在本机受控复现：

1. 交互题的选手进程可改写裁判结果 XML，原本 WA 可以变成 AC。
2. Windows 上主进程退出后，派生子进程若继续持有标准输出/错误管道，评测会在墙钟限制之后继续等待；受控样例的 200 ms 限制实际等待约 3 秒，结果还显示正常退出。

因此，建议在完成 S1 阶段前：

- 不将交互题、通信题和依赖其结果的正式比赛评测作为可发布能力。
- 对正式评测临时关闭会造成错误接受的文件输出回退路径，并要求答案文件存在。
- 生产评测关闭有问题的内存快速编译缓存，或默认等效使用 <code>--no-cache</code>，直到依赖清单缓存落地。
- 将 Windows 管道后代复现加入阻断发布的回归用例。
- 任何修复均保持项目既有约束：不用 Tokio 或其他 async 运行时；JSON/NDJSON/MCP 的 stdout 只输出协议；如实上报沙盒 tier；跨平台代码在 Windows MSVC 与 WSL2/Linux 双端验证。

## 2. 审查方法、证据等级与限制

### 2.1 阅读和验证范围

已审阅的重点包括：

- <code>uj-core</code> 的题目配置、限制、评分、运行结果模型；
- <code>uj-engine</code> 的编译缓存、判题主循环、调度与拓扑；
- <code>uj-sandbox</code> 的 Windows Job/AppContainer 路径、Linux namespace/cgroup/seccomp 路径和 none 回退；
- <code>uj-checker</code> 的 builtin、testlib、Lemon 适配；
- <code>uj-cli</code> 的 judge、authoring、contest、doctor、MCP、导入及输出格式；
- <code>uj-report</code> 和 <code>uj-tui</code> 的渲染、持久化与事件消费；
- 现有集成测试、示例题、语言定义、README、AGENTS、计划/性能文档和 CI/安装入口。

未逐行审阅第三方大体积 <code>testlib.h</code>；相关结论仅针对本项目对其协议、退出码和输出的适配层。

### 2.2 证据等级

| 标记 | 含义 | 使用方式 |
|---|---|---|
| R | 已用受控 fixture/harness 在本机复现 | 作为修复阻断项，必须增加回归测试 |
| S | 从当前源码可直接确认的确定性缺陷 | 修复前先补单元或集成测试 |
| C | 条件性风险，尚未人为注入底层故障复现 | 设计修复时覆盖，并在目标平台补验证 |
| D | 文档、能力声明或设计债务 | 排入版本计划，不作为现状功能承诺 |

“条件性风险”不表示已经在用户环境中发生，也不应按已复现故障宣传。

### 2.3 可追溯原始证据

本次临时复现文件保存在本机临时目录，便于维护期追查：

- <code>C:\Users\Lenovo\AppData\Local\Temp\uj-audit-20261011\repro\evidence.json</code>
- <code>C:\Users\Lenovo\AppData\Local\Temp\uj-audit-20261011\repro\special-evidence.json</code>
- <code>C:\Users\Lenovo\AppData\Local\Temp\uj-audit-20261011\rust-harness\harness-warm.log</code>
- Windows 验证日志：<code>C:\Users\Lenovo\AppData\Local\Temp\uj-audit-20261011-clippy-windows.log</code>、<code>C:\Users\Lenovo\AppData\Local\Temp\uj-audit-20261011-test-windows.log</code>
- WSL 验证日志：<code>C:\Users\Lenovo\AppData\Local\Temp\uj-audit-20261011\linux-*.log</code> 及对应 <code>.exit</code> 文件。

这些路径是审查证据，不是仓库交付物；下面每项关键复现都保留了足够的结果说明，使文档脱离临时目录后仍可理解。

## 3. 当前验证基线

| 检查 | Windows MSVC / Rust 1.94.0 | WSL2 Ubuntu / Rust 1.99.0 | 结论 |
|---|---:|---:|---|
| <code>cargo fmt --check</code> | 通过 | 通过 | 可作为后续门槛 |
| <code>cargo clippy --workspace --all-targets -- -D warnings</code> | 通过 | 失败 | Linux 在 <code>crates/uj-core/src/config.rs:474</code> 报 <code>clippy::collapsible_match</code> |
| <code>cargo test --workspace</code> | 88 passed, 0 failed | 87 passed, 0 failed | 均通过；数量差异来自平台条件测试 |
| evil 集成测试 | 13 passed | 13 passed | 通过 |
| pipeline 集成测试 | 20 passed | 20 passed | 通过 |

WSL 内核为 <code>6.6.87.2-microsoft-standard-WSL2</code>。本次没有运行 release 性能基准；后文的性能项是测量计划，不是已获得的性能提升。

还确认了两个工程基线问题：

- README 声称 Rust 1.85+，但锁定依赖与使用的 let-chain 至少要求 Rust 1.88；应将 MSRV 统一为经过 CI 验证的版本。
- AGENTS 架构图写有 <code>crates/uj-engine/src/pipeline.rs</code>，实际不存在该文件，当前流水线仍集中在 <code>uj-cli/src/author_cmd.rs</code>。

## 4. 优先级定义

| 优先级 | 判定标准 | 发布处理 |
|---|---|---|
| P0 | 可错误接受、错误得分、无界卡住、严重资源失控，或使“Full/Degraded”结果失真 | 修复和双端回归后才允许对应能力发布 |
| P1 | 常规工作流可能假成功、协议不可靠、特殊题型不完整、跨平台行为不一致 | 在下一个稳定版本前完成 |
| P2 | 可维护性、可观测性、性能、文档和长期架构债务 | 排入后续版本并以指标驱动 |

## 5. P0：先修复的判题完整性问题

### P0-01 交互裁判结果文件位于选手可写目录

**证据：R**<br>
**位置：** <code>crates/uj-engine/src/judge.rs:697-699</code>，交互结果 XML 和 transcript 文件创建在 <code>box_dir</code>；选手程序与交互器共享该目录。

受控复现中，交互器先写入 WA XML、休眠 500 ms 后退出。控制组的空选手程序得到 WA、0 分、<code>full</code> tier。替换为只扫描当前目录并改写同名 <code>interact_res_*.xml</code> 的选手程序后，最终报告变为 AC、100 分、<code>checker = "spoofed AC"</code>，tier 仍显示 full。该复现说明的是结果文件信任边界错误；它利用了人为设置的窗口，不代表每个交互器都必然存在稳定窗口。

**影响：** 交互题结果不可信。即使底层沙盒正确隔离宿主，选手和裁判共享可写评测目录也会破坏判题完整性。

**修复设计：**

1. 定义 <code>TrustedRunLayout</code>，至少分为：选手可写 box、裁判/答案只读目录、宿主专用结果目录。
2. 结果 XML、分数文件、裁判日志和 transcript 由宿主创建在专用目录；Windows 不授予选手 AppContainer SID，Linux 不 bind mount 到选手 namespace。
3. 更稳妥的长期方案是让交互器通过宿主持有的匿名管道或预打开 FD 回传结构化状态，避免以可替换路径传递最终结果。
4. 在解析 XML 前验证来源、唯一性、有限分数、退出状态和文件完整性；缺失或无效结果必须是 JudgeFailed，而非默认 WA/AC。
5. 将交互器、选手、后处理器的权限和生命周期显式建模，不能靠同一 <code>work_dir</code> 的约定。

**验收：**

- Windows Full 和 Linux Full 各增加一个“选手尝试创建/替换/删除结果文件”的集成用例，最终必须不改变裁判结论。
- 正常 AC、WA、交互器 RE、超时、无 XML、格式错误 XML 都有明确 verdict。
- 结果目录的 ACL/mount 布局在诊断模式可打印到 stderr，不污染 JSON stdout。

### P0-02 Windows 后代持管道导致墙钟限制失效

**证据：R**<br>
**位置：** <code>crates/uj-sandbox/src/windows/process.rs:185</code> 的 <code>execute_in_job</code>，以及 <code>:329-343</code> 对 reader thread 的 join。

受控 C++ 程序由父进程启动继承 stdout/stderr 的子进程；父进程立即成功退出，子进程仅睡眠 3 秒后退出。预热后，在 <code>max_processes=2</code>、墙钟 200 ms 下，得到：

~~~text
elapsed_ms = 3048
reported_wall_ms = 19
killed_by = None
status = Exited(0)
~~~

说明监控在主进程退出时结束，但后续 join 等待仍被后代占用的管道阻塞。若子进程无限持有句柄，评测可无限等待；本次没有运行无限子进程。

**影响：** 评测线程、批量队列和 TUI 可能卡住；报告把超时外的实际等待伪装成正常运行，时间数据不可信。

**修复设计：**

1. 将“主进程退出”改为“Job/进程组完全终止或明确 killed”才完成运行。
2. 运行超时、OLe、取消、reader 异常和 join 超时都必须终止 Job，再等待 reader 收尾。
3. reader 应有独立停止路径：关闭宿主读取端、记录截断/诊断状态、有限时 join；不能把无限 join 放在主评测路径。
4. <code>AssignProcessToJobObject</code> 失败后，立即终止已挂起进程并释放所有句柄；不能只关闭句柄后返回错误。
5. 使用统一的 <code>GroupSupervisor</code> 管理单进程、交互、通信和编译器子进程，避免每种模式独写 wait/join 逻辑。

**验收：**

- Windows 回归：后代继承管道并睡眠 3 秒，200 ms wall limit 的总调用耗时应低于约 1 秒，结果为 wall timeout 或受控 JudgeFailed。
- Windows 回归：后代无限睡眠用例在测试完成后无残留进程。
- 正常短命子进程、无 stdout/stderr、达到输出上限、取消时 reader 均能结束。

### P0-03 缺失答案和文件输出错误可产生 AC

**证据：R + S**<br>
**位置：** <code>crates/uj-engine/src/judge.rs:319-323</code>、<code>:934-936</code>、<code>:1081-1083</code>。

目前多个路径将 <code>fs::read</code> 或答案读取错误通过 <code>unwrap_or_default()</code>/<code>.ok()</code> 变成空字节串。已复现：

- 标准题中删除答案文件，选手输出为空，结果 AC、exit 0；
- 配置文件 I/O 输出为 <code>output.txt</code>，选手只向 stdout 写正确答案、未创建文件，结果 AC；
- 输出限制为 64 B，选手向 <code>output.txt</code> 写 1024 B，答案也是 1024 B，结果 AC。

**影响：** 缺数据、数据读取失败、未按题面约定输出，以及超过输出上限都可能被报告为正确。文件输出未受限还会带来磁盘与宿主内存压力。

**修复设计：**

1. 新增显式的 <code>ReadArtifactError</code>：答案缺失、I/O 失败、路径非法、超过大小限制分别保留原因。
2. 答案文件缺失或不可读应在评测开始前成为配置/系统错误；合法空答案只在文件成功读取且长度为零时成立。
3. file I/O 模式下只读取配置的输出文件；是否允许 stdout 回退必须是显式、默认关闭的兼容选项。
4. 用受限读取函数读取 stdout、file output、交互 transcript、checker 输出；检查元数据后按上限流式读取，超过上限立即标记 OLE 并终止/清理。
5. 为 Windows 和 Linux 补充文件大小限制或目录配额策略；不能只依赖 stdout pipe cap。

**验收：**

- 缺答案、权限拒绝、输出文件缺失、输出文件目录替换、空答案、空输出各有独立期望 verdict。
- 64 B 限制下 65 B stdout 与 65 B file output 都得到 OLE，且测试后临时目录无大文件残留。
- JSON 报告提供可区分的 <code>message</code>，不把系统错误伪装成 WA。

### P0-04 子任务依赖按声明顺序计算，系统错误被部分分掩盖

**证据：R + S**<br>
**位置：** <code>crates/uj-engine/src/judge.rs:461</code> 的 <code>aggregate_submission_report</code>。

已复现两个独立问题：

- 子任务 1 失败、子任务 2 依赖 1 且自身 AC。声明顺序为 1、2 时总分 0；交换配置声明为 2、1 后总分 50。配置只检查 DAG 无环，却没有拓扑顺序求值。
- 一个子任务 AC 25 分、另一个子任务 SE 0 分时，整体和子任务 verdict 变成 PC，原始 test verdict 仍是 AC/SE。系统错误被“部分得分”覆盖。

**影响：** 同一题、同一提交仅因 TOML 顺序不同得到不同分数；真实系统错误会被误导为普通部分正确，影响重测和赛后申诉。

**修复设计：**

1. 在配置校验阶段生成稳定的拓扑序，使用子任务 ID 作为并列节点的稳定 tie-break；不得依赖 TOML 声明顺序或 HashMap 遍历顺序。
2. 聚合分为两层：先收集每个 test 的运行/检查异常，再计算各子任务的得分与依赖门控。
3. 定义 verdict 优先级：JudgeFailed/CompileError/RuntimeError/超限等不可被 PC 覆盖。若产品希望保留得分，可同时给出 <code>score</code> 和明确的总体失败 verdict。
4. 将评分规则写入 <code>docs/PROTOCOL.md</code>，为 sum/min/group 等模型补表格示例。

**验收：**

- 对每个 DAG 的所有合法声明排列，报告分数和 verdict 完全一致。
- AC+SE、AC+TLE、AC+JudgeFailed、WA+AC 的聚合结果有固定断言。
- 使用 property test 生成小型 DAG 和测试 verdict 组合，验证拓扑顺序与结果稳定性。

### P0-05 编译和已授权二进制缓存可复用过期或损坏产物

**证据：R + S**<br>
**位置：** <code>crates/uj-engine/src/compiler.rs:236-246</code>、<code>:500-518</code>；<code>crates/uj-sandbox/src/lib.rs:357-418</code>。

受控 harness 在同一 <code>CompileSession</code> 中将头文件从 <code>VALUE=1</code> 改为 <code>VALUE=2</code>，第二次编译返回同一个 binary，程序仍返回 1。快速缓存只使用顶层源/extra source 的最大 mtime 与总大小，遗漏头文件、嵌套 include、语言配置等依赖；同秒等长修改也可能漏检。

另一个 harness 在 <code>prepare_binary_content</code> 后直接损坏缓存目标，再次准备仍复用了损坏文件。缓存只检查来源 mtime/size 和目标存在；rename fallback 的写入错误也被忽略，产物发布没有跨进程 singleflight/原子提交保证。

**影响：** 开发者修改代码却运行旧程序，或随机运行损坏产物；这是出题流水线、验证和本地调试的高风险错误来源。

**修复设计：**

1. 移除当前以内存 stamp 为真值的 fast path，至少先退化为内容哈希或只作为“候选”后再次验证完整依赖清单。
2. 编译缓存键包含：所有转译单元、递归 include/module 依赖、编译模板、语言配置内容、编译器绝对路径和版本、目标平台、相关 limits。
3. 由编译器生成依赖文件时优先读取依赖文件；不支持时采用可排序的递归扫描，并明确其保守失效策略。
4. 用 <code>tmp + fsync + atomic rename</code> 发布缓存文件；对同一 hash 使用进程间锁或 lockfile singleflight；发布前重新 hash 成品。
5. 已授权二进制以内容 hash 为 key，命中时验证目标 hash；所有 I/O 错误向上返回，不能静默跳过。
6. 设计缓存版本号和 LRU/大小上限，避免长期积累。

**验收：**

- 修改间接头文件、Rust module、同大小同时间戳的源文件、语言配置、编译器版本标识后均重新编译。
- 20 个并发相同编译请求只产生一个有效缓存产物，其余等待或读取完整成品。
- 人为截断缓存 binary 后下一次调用自动重建，不执行损坏文件。

### P0-06 Windows 预热路径可能在前置隔离失败时继续启动

**证据：C**<br>
**位置：** <code>crates/uj-sandbox/src/windows/process.rs:985</code> 起的 <code>prewarm_binary</code>。

AppContainer 获取、Job 创建、限制设置和关联中的多个错误被 <code>.ok()</code> 或忽略，后续仍可能恢复线程。正常执行路径与预热路径的约束不一致。此项没有通过人为注入 AppContainer/Job 创建失败进行动态复现，因此标为条件风险。

**影响：** 预热本应只用于处理 Windows Defender 首次启动扫描，却可能在资源与目录权限未成功建立时执行不可信程序，且失败难以诊断。

**修复设计：**

1. 预热应复用正常执行的受限启动构造器，不能维护一套“尽力而为”的实现。
2. 任一隔离前置条件失败时不启动进程；记录到 stderr/诊断事件并安全跳过预热。
3. 预热只可在受限 Job、独立 AppContainer 和最小权限目录都成功建立后执行。
4. 将预热结果建模为 <code>Skipped / Succeeded / Failed</code>，不影响正常判题的真实 tier。

**验收：**

- 通过可注入的 Win32 adapter 模拟 Job/AppContainer/ACL 任一失败，断言没有调用 ResumeThread。
- 正常 Defender 冷启动模拟仍只重跑一次，CPU 限制保持不变。

## 6. P1：下一稳定版本必须完成的功能与可靠性问题

下表先给出全量索引，后续小节解释实现方向。位置是当前基线附近的定位点，重构后应保留对应测试 ID 而非依赖行号。

| ID | 证据 | 问题 | 主要位置 | 建议版本 |
|---|---|---|---|---|
| P1-01 | R/S | 通信题固定仅 manager+1 participant，计分管道与 stdout 冲突，tier 标签错误 | <code>judge.rs:1168, 1330</code> | S5 |
| P1-02 | S | 交互/通信退出、超时、精确重跑路径不一致 | <code>judge.rs</code>, <code>scheduler.rs:610</code> | S5 |
| P1-03 | S | 编译器先轮询 child、后读管道，大量诊断可堵住 pipe；超时不完整回收 | <code>compiler.rs:444-465</code> | S3 |
| P1-04 | S | 语言配置字段未完整传入 RunSpec；Java、Go、Python 支持与文档不一致 | <code>compiler.rs</code>, <code>assets/languages.toml</code> | S3 |
| P1-05 | R/S | UTF-8 长 token 的错误摘要在字节边界切片 panic；评分可出现 NaN | <code>builtin.rs:73</code>, <code>testlib.rs</code>, <code>lemon.rs</code> | S2 |
| P1-06 | S | testlib/Lemon 退出码与 vendor 配置不完全一致 | <code>testlib.rs</code>, vendor 约定 | S5 |
| P1-07 | R/S | init 模板把 validator 写进 checker；重复 gen 追加测试；pipeline JSON 拼接多个对象 | <code>author_cmd.rs:31, 392, 452, 1848</code> | S4 |
| P1-08 | R/S | stress 只比较 stdout，不检查三个程序的执行结果 | <code>author_cmd.rs:1626-1832</code> | S4 |
| P1-09 | R/S | MCP pipeline/contest 会在无测试或无报告时返回 ok=true | <code>mcp_server.rs:535, 718</code> | S6 |
| P1-10 | S | MCP 串行读 stdin，长任务期间无法收到取消；init_problem 未实现 | <code>mcp_server.rs</code> | S6 |
| P1-11 | S | CLI 的 JSON/NDJSON、TUI、错误事件和退出码语义不统一 | <code>judge_cmd.rs</code>, <code>scheduler.rs</code>, <code>author_cmd.rs</code> | S6 |
| P1-12 | S | 配置对 NaN/Infinity/范围、特殊题型必填字段和路径缺少完整校验 | <code>config.rs</code> | S2 |
| P1-13 | R/S | Lemon 导入成功后生成的 TOML 无法再加载 | <code>lemon_import.rs:101</code> | S5 |
| P1-14 | S | verify/TL 多处吞掉执行、写入或 AC 失败 | <code>author_cmd.rs:1450-1607</code> | S4 |
| P1-15 | S | contest 忽略发现/报告 I/O 错误，选源不稳定，排行榜不是聚合总榜 | <code>contest_cmd.rs</code> | S6 |
| P1-16 | S | HTML 手拼接未转义的 test ID，固定生成时间，重复 DOM id | <code>html.rs:612-657</code> | S6 |
| P1-17 | S | report store 的 run id、last 写入和 I/O 处理不可靠 | <code>report_cmd.rs</code> | S6 |
| P1-18 | C/S | Linux runner 在多线程 fork 后做大量 Rust 分配/文件操作；cgroup 限制写入部分忽略错误 | <code>linux/runner.rs</code>, <code>linux/cgroup.rs</code> | S1/S3 |
| P1-19 | S | stderr 超限时关闭管道，可能改变选手进程行为；none tier 行为不一致 | <code>windows/process.rs:871</code>, <code>linux/runner.rs:802</code> | S1 |
| P1-20 | S | box 清理吞错，目录复用残留无法可靠报告 | <code>uj-sandbox/src/lib.rs:275</code> | S1 |

### P1-01/P1-02 特殊题型的进程模型应重建

通信题当前并未使用 <code>ProblemConfig.processes</code>，而是固定 manager 加一个 participant。manager 的 stdout 已接到选手 stdin，却在之后又尝试从 manager stdout 读取分数；受控 manager 输出 <code>1</code> 仍得到 WA/0 分。Windows group 结果按索引标注 tier，manager 可能显示 full、真正 participant 显示 degraded；<code>--require-tier full</code> 因错误标签失败。Linux group 的退出顺序未完整记录，交互先后退出与 post-checker 路径也不等价。

建立 <code>GroupSupervisor</code>：

1. 输入是一组具名角色和连接拓扑，不是位置索引。
2. 每个角色有独立 RunSpec、实际 tier、stdout/stderr 策略、可信/不可信身份。
3. manager 分数通过私有 FD 或私有文件回传，不能复用协议 stdout。
4. 统一收集每个角色的 exit、kill、CPU、内存、输出和 wall 时间，再由特殊题型规则决定 verdict。
5. 精确重跑必须保留同一组拓扑与协议，不能落到普通单程序 judge。

验收需要覆盖 1、2、N participant；manager RE/TLE；participant RE/TLE；manager 分数 0、部分分、满分；Windows/Linux tier 报告；取消和输出超限。

### P1-03/P1-04 编译器与语言模型需要从“命令模板”升级为完整运行描述

当前编译进程以 piped stdout/stderr 启动，循环 <code>try_wait</code> 后才调用 <code>wait_with_output</code>。编译器输出填满 OS pipe 时，子进程等待读端、父进程等待子进程，最后产生假超时。超时只 kill 主编译器，没有可靠等待和进程组清理；<code>--version</code> 与 Python 语法检查也没有超时。

语言定义中的 <code>run</code>、<code>time_factor</code>、<code>max_processes</code>、<code>memory_extra</code>、<code>seccomp</code>、<code>ro_binds</code> 没有完整贯通至 RunSpec。Java 编译 class 后返回的默认二进制路径不成立；README 宣称 Go 可用而默认语言表无完整 Go 定义；C++17/C++20 同扩展的选择依赖 HashMap 顺序。Windows 命令行参数对内嵌引号和尾随反斜杠也需要使用系统正确的 quoting 算法。

执行方案：

1. 新建 <code>PreparedProgram</code>：入口文件/二进制、解释器命令、参数、工作目录、环境、运行限制修正、只读绑定和产物列表。
2. 语言选择使用显式 <code>language_id</code> 或稳定优先级，检测同扩展歧义并报配置错误。
3. 编译启动使用并发 reader 或文件重定向并设捕获上限；超时时终止完整编译进程组并 wait。
4. 给所有外部命令统一 <code>CommandRunner</code>，支持 timeout、cancel、drain、exit diagnostics，不引入 async runtime。
5. Java/Python/Go/Rust/C/C++ 各有最小端到端 fixture，文档只声明已通过矩阵的语言。

### P1-05/P1-06 检查器错误处理和第三方协议

<code>Token::snippet</code> 以 <code>&amp;s[..100]</code> 截取 UTF-8；50 个中文字符（150 bytes）的 token 在 WA 路径会 panic，CLI 以 SE/exit 3 结束。checker worker join 错误目前被吞掉，部分 worker 丢失结果的风险无法在报告中区分。XML/Lemon 的 <code>NaN</code> 会穿透 <code>clamp</code> 成为 NaN score；testlib exit 7 被固定映射为 0.5 分，而 vendor 的 base exit code 可配置，默认和 TESTSYS 模式不同。

执行方案：

- 对任何来自文本/XML/外部程序的浮点分数执行 <code>is_finite</code>、范围和语义校验；无效结果为 JudgeFailed。
- 用字符边界安全截断或 bytes 逃逸摘要；任何 checker thread panic/错误必须升级为 JudgeFailed，不允许默认空结果。
- 将 testlib/Lemon 的协议版本、退出码表、分数文件格式配置化；把 vendor 默认、TESTSYS 和自定义 base 放入兼容测试。
- 评分结构使用有界类型或构造函数，避免任意 <code>f64</code> 直接进入报告。

### P1-07/P1-08 出题流水线当前存在假成功路径

已复现：

- <code>uj init</code> 模板把 root-level validator 放在 <code>[checker]</code> 表中，配置解析得到 <code>validator = None</code>，随后 validate 被跳过；
- 同一题连续执行 <code>gen</code>，输入文件数从 4 增至 7，每次追加新编号，缓存与可重复性失效；
- <code>pipeline --format json</code> 以 exit 0 返回五个格式化 JSON 对象串接，整体无法 <code>json.loads</code>；
- generator、solution、brute 全部退出 7 且无输出时，stress 返回 <code>ok=true</code>、<code>iterations=1</code>。

此外，gen/validate/ans 多处硬编码 auto/false 和 worker 范围，忽略 jobs、sandbox、require-tier、no-cache 等全局选项。ans 缓存只看文件是否存在，不验证当前答案内容；不完整支持 interpreter/file I/O/grader。ans 还把完整题目目录作为参考解工作目录，Windows ACL 和 Linux bind 的授权范围过宽，参考解或其缺陷可能改写输入、答案或配置，应改为每个测试的最小可写工作目录。stress 中选手使用共享 scratch，存在并行文件命名冲突和相互可见问题；不应把它描述为“选手拥有整个题目目录”。

执行方案：

1. 提取 CLI 无关的 <code>AuthoringService</code>，每个 step 返回结构化 <code>StepReport</code>，包含状态、产物、错误、计数与耗时。
2. pipeline 只输出一个统一的 <code>PipelineReport</code>；NDJSON 则明确每行一个 <code>PipelineEvent</code> 和一个 terminal event。
3. 引入生成 manifest：输入编号、seed、generator hash、validator hash、输出 hash、subtask、创建时间；同一配置重跑默认幂等，<code>--append</code> 才追加。
4. 每次 stress 先判断 generator/target/brute 的退出、kill、输出限制，再做 checker 比较；任何执行失败应保存诊断并返回失败。
5. validator、solution、reference answer 和答案 cache 的依赖都由 manifest/hash 驱动。

### P1-09/P1-10 MCP 与机器协议

MCP pipeline 对“没有测试且 generator 指向不存在源码”的 fixture 回 <code>ok=true, pipeline_complete, tests_count=0</code>。MCP contest 对空的存在目录回 <code>ok=true</code> 和一个 CSV 路径，但文件不存在。当前 MCP 串行读取 stdin 并直接执行长任务，取消通知在任务完成前无法被读取；JSON-RPC 的 <code>jsonrpc</code>、id、notification 与参数结构也没有完整验证。AGENTS 列出 <code>init_problem</code>，实现并不存在；judge 也没有可靠保存 report，使 show 不一定可读。

执行方案：

1. 以 std::thread + crossbeam-channel 建立一个专用 stdin reader、请求分发线程和受控 worker；保持 stdout 只写 JSON-RPC。
2. 每个长任务关联 CancelToken，reader 收到取消通知立即发出取消信号；不需要引入 Tokio。
3. 对请求版本、id、method、params 进行 schema 验证；notification 不返回 response；错误使用稳定 JSON-RPC code。
4. MCP 复用 AuthoringService/ContestService 的同一结果模型，不重写成功判定。
5. 补齐或从工具清单删除 <code>init_problem</code>；judge/contest 成功必须在结果中可验证所宣称的 report 文件。

### P1-11 CLI、NDJSON 与 TUI 的事件契约

当前部分命令只区分 Json 和 Human；<code>clean</code> 不接 format 且会吞删除错误；last/show 的 NDJSON 可能输出多行 pretty JSON。<code>uj schema result</code> 给 Verdict 枚举输出 <code>Accepted/WrongAnswer/...</code>，实际报告使用 <code>AC/WA/...</code>，因此 schema 无法验证自身产生的结果。scheduler 先发送 AC/0 的占位 summary，结束没有真实 summary/subtask terminal event，错误路径也可能没有 terminal event。<code>--tui</code> 会消费事件使 NDJSON stdout 为空；TUI 本身使用 stderr 绘制，这一点符合 stdout 零污染原则。TUI 不处理 channel 断开，调度错误而未发送 SubmissionDone 时可能一直等待；编译失败 continue 时也可能不 join 事件线程。batch 的退出码可能被后续 WA 覆盖此前 SE。

执行方案：

- 将 <code>OutputFormat</code> 定义为统一协议层，所有命令在 Json/NDJSON 下经过一个 emitter；schema 从同一序列化类型或同一 wire enum 生成，禁止展示名与 wire 值分叉。
- 一项 submission 只能有一个 final report；NDJSON 必须有显式 <code>submission_started</code>、<code>test_finished</code>、<code>submission_finished</code> 或 <code>submission_failed</code>。
- TUI 订阅事件的副本，不抢占 stdout emitter；channel close 视为错误/完成状态并退出。
- 统一 exit code 决策为所有 submission 的最严重状态，不能被后续结果覆盖。

### P1-12 配置校验和发现规则

配置目前验证子任务 DAG 环和部分字段，但还缺少：

- NaN、Infinity、负值、过大数到 <code>u64</code> 的显式有限性和 checked conversion；
- schema 版本兼容策略；
- 特殊题型必需的 checker/interactor/manager/进程数；
- 题目目录外路径、<code>..</code>、非法 glob/regex；
- 子任务测试重叠、遗漏、未知 test ID；
- <code>tests.pattern</code>、manual/samples/gen.subtask 等发现语义的完整实现。

<code>stack = "unlimited"</code> 与 “未设置” 都落为 None，而 runner 又按 memory 填默认栈，语义不是 unlimited。单个 <code>subtask_id</code> 发现时可能覆盖此前信息。

执行方案：

1. 解析期使用新类型：NonZeroDuration、ByteSize、FiniteScore、PathWithinProblem、StackLimit。
2. <code>ProblemConfig::validate</code> 返回全部错误列表，CLI/MCP 同样展示；配置加载后禁止悄悄忽略字段。
3. 把测试发现结果写成稳定 manifest，并检查每个 test 只属于允许的子任务集合。
4. 为 schema 引入版本迁移或明确拒绝未知版本，避免“默认按当前规则解释旧题”。

### P1-13 Lemon 导入必须 round-trip

已复现 <code>uj lemon import</code> 返回 exit 0，但生成 <code>[problem]</code> 表；随后 build 加载同一 <code>problem.toml</code> 报 “missing field name”。现有测试只断言输出文本片段，未重新解析实际产物。

修复要求是导入后立即调用 <code>ProblemConfig::load_and_validate</code>，失败则 import 失败并清理临时输出。验收以“导入 -> build -> judge 一道示例题”为一条端到端链路，不只是 TOML 文本比较。

### P1-14/P1-15 verify、TL 与 contest 应共享执行服务

verify 允许 Rejected 接受 SE 等任意非 AC，WA/TLE 排除集合漏 SE；partial 分数写死 100，而非题目满分。TL 标定中执行错误被 <code>if let Ok(report)</code> 吞掉，未保证 AC；在原 TL 下运行截断数据也无法校准慢正确解；写回配置的正则替换和文件写入错误被忽略。contest 对配置/测试发现错误静默跳过、报告写入错误仍返回成功、多个源码取 <code>read_dir</code> 首项而不稳定、外层 player/problem 串行且 HTML 是逐提交报告而不是总排行榜。

应把这些流程建在统一的 <code>JudgeService</code>、<code>AuthoringService</code> 和 <code>ContestService</code> 上；每一层返回带状态的 result，禁止使用“读取失败则空集合/继续”的便利路径。排行榜按总分、罚时、选手名稳定排序，并显式定义并列规则。

### P1-16/P1-17 报告输出和持久化

HTML 在 <code>html.rs:612-637</code> 手工拼接 row，并以 <code>from_safe_string</code> 标为安全；test ID 直接进入 HTML、DOM id 和 onclick。含 <code>&lt;svg onload=...&gt;</code> 的 test ID 会被原样插入。该问题首先会造成报表结构错误，也可能在本地查看时执行不应执行的页面内容。<code>generated_at</code> 固定为 <code>2026-10-10 16:30</code>，重复 test ID 会碰撞 DOM id。

报告应该把 id、文本和属性传入模板变量并进行 HTML/JS/attribute 分别转义；DOM id 使用安全的内部序号/编码；时间取真实 UTC 或带时区的本地时间并可注入测试时钟。report store 则改为高熵/单调 run id、临时文件原子 rename、明确的 last 指针和 I/O 错误传播。CSV 对 CE 等无测试报告也应输出 submission summary 行。

### P1-18/P1-19/P1-20 Linux 与通用沙盒健壮性

Linux runner 在多线程进程 <code>pre_exec</code> 后执行分配、格式化、文件系统操作、创建 Vec/CString，并再次 fork；这不符合多线程 fork 后只执行 async-signal-safe 操作的约束。代码文档提到 re-exec helper，主路径并未落实。cgroup 的 pids/swap/oom/cpuset 设置、只读 remount 和部分 rlimit 设置存在忽略错误；虽然进入 cgroup 的 <code>cgroup.procs</code> 写入有严格检查，不能把所有 ignored error 都误判为“未进 cgroup”，但 Full tier 的能力判断仍需逐项真实验证。seccomp 仅 x86_64 有完整规则，其他架构可能返回 Ok。

Windows 和 Linux 对 stderr 超过 cap 的处理会直接关闭管道、只保留前缀，可能导致被测进程收到 EPIPE/SIGPIPE 并改变最终 verdict；none tier 反而会持续 drain 保存 tail，语义不一致。通用 box 清理函数吞掉错误，复用 slot 时可能有残留，<code>residue_cleaned</code> 也未准确反映状态。

执行方案：

- 采用单用途、最小 C/exec helper，或在 spawn 前预构造所有资源，child 只做安全 syscall 后 exec。
- 为每一项 Full 能力设置 capability probe；失败则降为 Degraded/None 或 <code>--require-tier full</code> 报错，绝不虚标。
- reader 达 cap 后继续 drain 丢弃，或主动且确定地杀死整个运行组，再记录因果，不能无状态地关读端。
- box 清理返回 Result；清理失败阻止复用并隔离该 slot，随后异步/启动期回收。

## 7. P2：性能、可维护性与发布工程

### 7.1 性能现状与测量原则

仓库已经有多轮性能工作，例如 builtin checker 的 mmap/memchr/SWAR、并发 gen/validate/ans、slot box pooling、stdout pipe buffering、CompileSession 快路径。它们不应作为“下一步尚未完成的优化”重复立项。

本次没有做 release benchmark，因此不承诺任何 x 倍加速。性能工作要在 P0/P1 正确性收敛后，以如下指标为准：

| 场景 | 核心指标 | 辅助指标 | 基线与验收 |
|---|---|---|---|
| 单次标准题 | P50/P95 总 wall、CPU | RSS、上下文切换、syscall 数 | 固定题集，冷/热缓存分开记录 |
| 100/1000 测试批量评测 | 吞吐、尾延迟 | slot 利用率、排队时间、清理失败率 | jobs=1/物理核/超额三组 |
| 编译缓存 | 命中率、正确失效率 | hash 时间、lock 等待、磁盘增长 | 改 header/config/并发编译测试 |
| 大输出/大答案 | 比较器吞吐、峰值内存 | reader CPU、截断正确率 | 1 MiB、64 MiB、上限边界 |
| 出题 pipeline | 总时长、每步时长 | 生成重复率、缓存命中、失败可定位性 | 固定 seed/manifest |
| 通信/交互 | 启动延迟、准确超时率 | FD 数、后代残留、角色 CPU | 角色数和数据量矩阵 |

建议建立 <code>uj-cli/examples/perf_bench.rs</code> 的 CI 非阻断基线，并将 release 指标保存为 JSON；性能回归超过预先设定阈值时报警，不用一次性绝对阈值阻塞所有平台。

### 7.2 优先性能项目

| 项目 | 前置条件 | 实现方向 | 验收 |
|---|---|---|---|
| P2-01 编译缓存 | P0-05 完成 | 依赖 manifest、内容 hash、singleflight、原子发布与 LRU | 正确失效后比较热缓存 P50/P95 |
| P2-02 进程 I/O | P0-02、P1-19 完成 | 统一 draining reader、合理缓冲、有限回收 | 大输出不死锁且不改变 verdict |
| P2-03 调度内存预算 | RunSpec 完整 | jobs 同时受 CPU 与 memory reservation 控制 | 压力下无宿主抖动，吞吐优于串行 |
| P2-04 拓扑快照 | scheduler 拆分 | 启动时检测 CPU/内存一次，向 UI/engine 注入 | 消除重复 sysinfo 与机器状态漂移 |
| P2-05 答案/报告流式化 | 有界 ArtifactReader | immutable 映射复用、流式 CSV/HTML 写入 | 大题集峰值 RSS 可测且有上限 |
| P2-06 缓存维护 | hash 缓存落地 | 大小/时间淘汰、索引压缩、诊断命令 | 长期运行磁盘占用受控 |

### 7.3 工程可维护性

- <code>doctor --fix-apparmor</code> 当前只输出“Installing”，没有实际安装；应改为真实执行并检查结果，或明确仅输出指导。
- 安装脚本吞掉 <code>apparmor_parser</code>/doctor 失败仍宣布成功；release 工作流应依赖 fmt/test/关键 E2E，并发布 checksum 和可复现构建信息。
- <code>topology</code> 在显式 jobs 时绕过内存预算，UI 与 engine 重复检测，建议统一成注入的 HostCapabilities。
- cgroup stale cleanup/Drop 涉及共享 <code>uj</code> 基目录，需引入运行所有权记录；目前没有证据表明它会误杀其他任务，不应写成已确认事故。

## 8. 目标架构与更新方向

下一轮更新不应继续在 CLI 函数中堆叠特殊分支。建议按下面的稳定边界演进：

~~~mermaid
flowchart LR
    Config[ProblemConfig + validated manifest] --> Build[PreparedProgram / ArtifactCache]
    Build --> Judge[JudgeService]
    Judge --> Supervisor[GroupSupervisor]
    Supervisor --> Result[Typed Run/Checker Result]
    Result --> Aggregate[Scoring + ReportService]
    Config --> Author[AuthoringService]
    Build --> Author
    Judge --> Author
    Author --> MCP[MCP / CLI / TUI adapters]
    Aggregate --> MCP
~~~

### 8.1 核心服务边界

| 服务 | 职责 | 不应承担的职责 |
|---|---|---|
| Config/Manifest | 解析、验证、路径约束、测试清单、版本迁移 | 直接运行进程 |
| PreparedProgram/ArtifactCache | 语言选择、编译、依赖 hash、产物原子发布 | 评分和 CLI 输出 |
| GroupSupervisor | 单/多进程启动、资源限制、I/O、取消、回收、真实 tier | 解释题目分数协议 |
| JudgeService | 标准、交互、通信、输出题的题型规则 | 直接打印 JSON |
| CheckerService | builtin/testlib/Lemon 适配和有界结果 | 擅自吞掉执行错误 |
| AuthoringService | build/gen/validate/ans/verify/tl/stress/pipeline 的结构化步骤 | 自行实现 sandbox policy |
| ReportService | schema、CSV、HTML、持久化 | 重新推导判题逻辑 |
| CLI/MCP/TUI adapter | 输入解析、事件展示、协议输出 | 改变服务成功/失败语义 |

### 8.2 数据模型补齐

需要优先新增或明确的类型：

- <code>ArtifactReadResult</code>：bytes、size、truncated、source、error；
- <code>PreparedProgram</code>：语言、入口、命令、解释器、环境、运行时修正和产物 hash；
- <code>RunGroupResult</code>：每个角色的真实 tier、exit、kill、资源、I/O 状态、生命周期；
- <code>PipelineReport</code>/<code>StepReport</code>：成功必须由产物和不变量证明；
- <code>ValidationErrors</code>：可同时报告多个配置问题；
- <code>Score</code>：有限、界内的值，构造时校验；
- <code>CapabilityReport</code>：每项 Linux/Windows Full 能力的实测支持状态。

## 9. 分阶段执行计划

人日为粗略实施与代码评审预算，不含等待外部环境、比赛数据迁移或较大第三方兼容工作的不确定性。每阶段完成后再锁定下一阶段范围。

| 阶段 | 目标 | 主要工作 | 预估 | 退出条件 |
|---|---|---|---:|---|
| S0 | 建立可重复基线 | 修 Linux clippy、固定 MSRV、归档复现、CI 分层 | 1-2 人日 | 两端 fmt/clippy/test 绿，复现变成自动测试骨架 |
| S1 | 停止错误接受和卡死 | P0-01~06，reader/box 清理，Full capability 规则 | 7-11 人日 | 交互篡改、后代管道、缺答案、缓存、DAG 复现全部双端通过 |
| S2 | 评分与配置可信 | 文件输出上限、聚合、checker 数值/Unicode、配置新类型 | 5-8 人日 | 错误输入不可转成 AC/NaN/PC 掩盖 |
| S3 | 编译与语言完整 | PreparedProgram、CommandRunner、缓存依赖、语言矩阵 | 6-10 人日 | 支持语言逐项 E2E，缓存正确且可测 |
| S4 | 出题流程可重复 | AuthoringService、manifest、pipeline/stress/verify/tl | 5-8 人日 | pipeline 单一协议、重复执行幂等、失败不报成功 |
| S5 | 特殊题型可发布 | GroupSupervisor、交互/通信/Lemon round-trip | 8-14 人日 | 多角色和题型矩阵双端通过 |
| S6 | 外部接口与报表 | MCP、NDJSON、TUI、contest、HTML/CSV/report store | 5-9 人日 | 协议 schema 校验、取消、报告可读可追溯 |
| S7 | 性能与发布工程 | benchmark、预算调度、doctor/install/release | 4-7 人日 | 有基线、回归报警、安装和发布闭环 |

### S0 清单

- [x] 将 <code>config.rs:474</code> 的 clippy 报告修复为两端一致通过。
- [x] README、Cargo metadata、CI 中统一 MSRV；若继续支持 1.85，需降级依赖和语言用法并在该版本 CI 验证。
- [x] 在 <code>tests/regression</code> 或相近目录创建 P0 fixture 命名和说明，不提交临时审查二进制。
- [x] CI 拆分为 Windows、Linux、fmt/clippy、workspace test、关键 special-mode E2E；将 release 依赖于这些 required checks。
- [x] 更新 AGENTS 的模块图，移除不存在的 <code>pipeline.rs</code> 描述或实现真正的 engine pipeline。

### S1 清单

- [x] 实现受信任/选手/结果目录布局和权限测试。
- [x] 从交互题开始接入 GroupSupervisor 的最小单组模式。
- [x] 修复 Windows Job 生命周期、管道 reader 与失败清理。
- [x] 改造 Linux reader/box cleanup，并将 Full capability probe 纳入 tier 判定。
- [x] 移除缺答案的默认空值语义，建立 ArtifactReader。
- [x] 实现稳定拓扑聚合与错误优先级。
- [x] 禁用或替换有问题的内存编译快路径，后续再落地内容依赖缓存。
- [x] 在 Windows 与 WSL2 运行完整 workspace 测试和新增定向回归。

### S2-S3 清单

- [x] 设计并实施有限 score、限制数值与路径类型。
- [x] file I/O、stdout、checker 输入全部走有界 ArtifactReader。
- [x] checker 的 UTF-8 截断、worker 错误、NaN/Infinity、testlib/Lemon 映射完成测试。
- [x] 设计 <code>PreparedProgram</code> 并迁移 C/C++、Python、Rust、Java、Go。
- [x] 建立 CommandRunner，迁移 compiler 的 version/compile/解释器语法检查。
- [x] 完整依赖 manifest、原子发布、并发锁和缓存淘汰。

### S4-S6 清单

- [x] 提取 AuthoringService，所有 pipeline step 具有统一 StepReport。
- [x] 生成 manifest 与正确的增量/append 语义。
- [x] verify、tl、stress 的运行状态和错误传播统一。
- [x] 导入 Lemon 后立即 load/validate/build 的 round-trip。
- [x] 通信题按角色/拓扑运行，分数走可信通道。
- [x] 完成 CLI JSON/NDJSON event contract 和 MCP 并发取消模型。
- [x] 完成 contest 聚合榜、HTML 转义、报告原子持久化。

### S7 清单

- [x] 为每个核心场景保存 release benchmark JSON。
- [x] 引入 CPU 与内存双预算调度，记录 queue/slot 指标。
- [x] 优化前后对比必须报告 P50/P95、RSS、CPU、上下文切换，且先证明结果完全相同。
- [x] doctor 做真实 capability probe；install/release 不吞错误，产物带 checksum。

## 10. 回归测试矩阵

| 测试 ID | 场景 | 平台 | 期望 |
|---|---|---|---|
| REG-INT-001 | 选手尝试改写交互结果 XML | Windows Full、Linux Full | 原裁判 WA 保持 WA，结果路径不可写 |
| REG-WIN-001 | 父进程退出、子进程继承 stdout/stderr | Windows | wall limit 内返回，Job 无残留 |
| REG-ART-001 | 缺失答案、合法空答案、权限拒绝答案 | 两端 | 分别为配置/系统错误、正常比较、系统错误 |
| REG-ART-002 | file I/O 缺失、stdout 回退、超上限文件 | 两端 | 不产生 AC；明确 OLE/WA/JF |
| REG-SCORE-001 | 子任务 DAG 的声明排列 | 两端 | 分数与 verdict 不变 |
| REG-SCORE-002 | AC+SE/TLE/JF 混合 | 两端 | 总 verdict 不被 PC 覆盖 |
| REG-CACHE-001 | 修改间接 header/module/config | 两端 | 一定重新编译 |
| REG-CACHE-002 | 损坏授权缓存和并发编译 | 两端 | 重建有效产物，无部分文件 |
| REG-CHECK-001 | UTF-8 长 token、NaN/Infinity 分数 | 两端 | 无 panic；结果为受控错误 |
| REG-AUTH-001 | init -> gen 两次 -> validate -> ans -> pipeline JSON | 两端 | 模板有效、幂等、单一合法 JSON |
| REG-STRESS-001 | 三个程序均失败 | 两端 | stress 返回失败和诊断 |
| REG-COMM-001 | 多 participant、manager 部分分、角色超时 | Windows、Linux | 角色状态/tier/得分准确 |
| REG-MCP-001 | 空题、缺生成器、取消长任务、notification | 两端 | 不假成功；协议合法 |
| REG-REPORT-001 | 特殊字符/重复 ID/CE 无 tests | 两端 | HTML 可解析、CSV 有摘要、时间真实 |

除了这些定向测试，每次触及 <code>uj-sandbox</code> 或 <code>uj-engine</code> 的跨平台逻辑，必须执行：

~~~powershell
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
~~~

Windows 使用 MSVC；WSL2/Linux 使用 Bash 环境并执行同一组命令。若某项仅能在特定 tier 下执行，应在 CI 输出中明确为 skipped 的原因，不能把未测当通过。

## 11. 文档与对外能力声明修订清单

| 文档/入口 | 当前偏差 | 应修订内容 |
|---|---|---|
| README | Rust 1.85+ 与实际锁定依赖不符；语言支持声明过宽 | 写经过 CI 的 MSRV 和语言矩阵 |
| README/AGENT_GUIDE | 部分退出码与 <code>error.rs</code> 不符 | 统一为实际语义：0 成功、1 非 AC、2 配置、3 系统/执行、4 verify mismatch、5 CE、130 取消 |
| AGENTS | 引用不存在的 engine pipeline；MCP 工具表包含未实现能力 | 按实际服务边界更新 |
| PLAN/AGENT_GUIDE 的 init 模板 | validator 表位置错误 | 改为可 load/validate 的最小模板 |
| SANDBOX | Linux Full 能力描述超前于实现与 probe | 以实际 capability matrix 声明 |
| 旧性能验收 | 旧测试数和通信覆盖不代表当前安全/正确性证明 | 标记历史数据、链接新的 benchmark 基线 |
| PROTOCOL | 子任务依赖、特殊题型、JSON/NDJSON terminal event 不够明确 | 增加规范、错误状态和示例 |

## 12. 实施顺序的决策原则

1. 先让“任何结果都可信”：答案、输出、结果文件、进程结束和评分聚合必须没有默认成功路径。
2. 再让“每次运行都可重复”：缓存、生成、导入、验证和 pipeline 必须以 manifest/内容 hash 为依据。
3. 再扩展语言和特殊题型：先建立 PreparedProgram 与 GroupSupervisor，再迁移 Java/Go/交互/通信。
4. 最后优化吞吐：每项优化都先有正确性回归和测量基线，不能用减少检查来换速度。
5. 所有入口复用服务层结果；CLI、MCP、TUI、contest 的“成功”必须是同一个不变量，而不是各自打印一段 ok。

## 13. 本轮审查后可立即建立的 issue 列表

建议按以下标题建立跟踪项，并在 issue 中引用本文对应章节：

1. <code>P0: isolate interactive judge result artifacts from contestant workspace</code>
2. <code>P0: terminate Windows Job before joining inherited pipe readers</code>
3. <code>P0: distinguish missing artifacts from valid empty output</code>
4. <code>P0: make subtask aggregation topological and error-preserving</code>
5. <code>P0: replace timestamp/size compilation and prepared-binary cache validation</code>
6. <code>P0: make Windows prewarm fail closed</code>
7. <code>P1: introduce bounded artifact reading for file I/O and checker data</code>
8. <code>P1: rebuild communication and interactive execution around GroupSupervisor</code>
9. <code>P1: introduce PreparedProgram and bounded CommandRunner</code>
10. <code>P1: make authoring pipeline manifest-driven and protocol-correct</code>
11. <code>P1: align MCP/NDJSON/TUI with one typed event contract</code>
12. <code>P1: validate configuration and imported Lemon projects by round-trip</code>
13. <code>P1: harden report rendering and report-store persistence</code>
14. <code>P2: establish release performance baseline and resource-aware scheduling</code>
15. <code>P2: repair doctor/install/release/docs capability claims</code>

## 14. 完成定义

本计划不是以“问题列表全部关闭”为完成标志，而以以下可验证状态为完成标志：

- 标准题、file I/O、交互题、通信题、输出题都能给出可解释且可复现的结果；
- 缺失数据、执行失败、超限、取消、检查器异常都不可能变成 AC 或无提示的 ok=true；
- Windows 与 Linux 对相同题型报告真实 sandbox tier、退出状态和资源状态；
- 同一输入/配置在 TOML 排列、缓存冷热、CLI/MCP/TUI 入口变化下结果一致；
- 所有机器协议 stdout 都能被逐条或整体解析，错误也有终止事件；
- 关键性能场景有版本化基线，优化没有改变判题语义；
- 两端 <code>fmt</code>、<code>clippy -D warnings</code>、workspace tests 与关键 E2E 均通过。
