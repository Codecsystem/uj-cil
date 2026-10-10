# uj 性能优化执行单（供 Gemini 3.8 Flash 修改）

审阅日期：2026-10-11。参考代码版本：`83c7c78`。这个版本号仅用于定位审阅上下文，不要求执行前锁定提交或做 hash 比对。

**结论：存在明确的优化空间。优先减少测试点循环中的重复文件读取、hash、外部进程启动和内存复制。** 收益最可能出现在大量短测试点、大答案文件、多子任务匹配和重复出题场景。长时间运行的选手程序仍主要消耗在自身计算上。

本文依据实际源码和调用关系进行静态审阅，尚未运行性能基准，不承诺具体提速百分比。README 和 `docs/PLAN.md` 中的启动时间、千点评测时间属于项目目标，不能直接作为已达成的测量结果。本次交付只新增执行文档，没有修改 Rust 实现，也没有运行工作区测试。

## 1. 给执行模型的工作要求

先阅读根目录 [AGENTS.md](../AGENTS.md)，然后按第 3 节顺序落实 A–H。每项保持小范围修改，完成相关验证后继续下一项，无须逐项请求确认。第 4 节是后续候选，不要求在本轮顺带重构。

- 保持 `std::thread + crossbeam-channel + parking_lot` 模型，不引入 async 运行时。
- 保持沙盒隔离、资源限制、进程回收、判定优先级和精确复测语义；结构化 stdout 继续只有协议数据。
- 优先用局部变量、已有状态和清楚的对象生命周期消除重复工作。无需新增性能开关、审批阶段、影子执行、缓存守护进程或通用缓存框架。
- 保留现有编译缓存所必需的内容 hash；将重复计算移到正确层级。不要增加逐测试点 hash、整目录指纹、运行结果 hash、二进制反复验真或多层 manifest 校验，也不要用单纯的路径/mtime 长期替代源码内容校验。
- 用少量有意义的行为测试和一份修改前后测量记录验证。性能结果不接入 CI 硬阈值，不为耗时抖动设计门控。

实际目录与旧说明有两处差异：出题流水线在 `crates/uj-cli/src/author_cmd.rs`，当前没有 `uj-engine/src/pipeline.rs`；对抗集成测试位于 `crates/uj-cli/tests/evil_tests.rs`。按实际文件修改。

## 2. 已核实的开销

以下行号对应审阅版本；后续优先按函数名定位。

| 项目 | 代码证据 | 当前额外工作 | 目标 |
| --- | --- | --- | --- |
| A：调度准备 | `uj-engine/src/scheduler.rs`：59、206–212、231–255 行 | 比较器内部反复 `metadata`；创建后立即丢弃预检沙盒；线程数可能超过测试数 | 每个输入至多查询一次大小，复用预检实例，按实际任务数创建 worker |
| B：子任务匹配 | `uj-core/src/tests.rs`：142–147、178–218 行 | 每个测试名匹配时重新编译 Regex / Glob | 每组模式编译一次，循环内只匹配 |
| C：可执行文件准备 | `uj-engine/src/judge.rs`：107、631、1145 行；`uj-sandbox/src/lib.rs`：222–276 行 | 每个测试点先完整读取并 hash 二进制，然后才查询准备缓存 | 同一提交的一份二进制准备一次，测试点复用结果 |
| D：Windows ACL | `uj-sandbox/src/windows/mod.rs`：73–106 行；`security.rs`：138–149 行 | 已授权 slot 父目录，新 box 仍再次启动 `icacls /T` | 利用新建 box 的 ACL 继承，省去逐点外部进程 |
| E：答案读取 | `uj-checker/src/lib.rs`：75–87 行 | 大文件 `mmap` 后立即 `to_vec()`；小文件先 open 又 `fs::read` | 持有映射并直接借用；小文件从已打开句柄读取 |
| F：出题 hash | `uj-cli/src/author_cmd.rs`：405–419、673–685 行 | gen 循环反复读/hash 同一个程序；ans 为 hash 整份分配输入 | 复用 generator 的 hasher 前缀状态；流式计算输入 hash |
| G：编译缓存命中路径 | `uj-engine/src/compiler.rs`：92–122、169–179、276 行 | 命中磁盘缓存之前仍反复解析配置、查 PATH、运行 `--version` | 同一命令或 MCP 请求内复用编译环境信息 |
| H：比较器中间分配 | `uj-checker/src/builtin.rs`：257–286、431–482、671–758 行 | 行比较先建立两个全量行切片 Vec；大整数正常匹配也分配 String | 使用行迭代器和借用的大整数视图 |

例如，8 MiB 二进制评测 1000 点，C 当前会处理约 7.8 GiB 的重复二进制字节。这是代码推导的读取/hash 总量，不等于实际磁盘读取量，也不是实测耗时。64 位系统两个行切片 Vec 每行合计至少约 32 字节，百万行约 30.5 MiB，尚未计算 Vec 扩容余量。

## 3. 按顺序实施

### A. 调度器只做一次必要的准备（优先级 P0）

修改 [scheduler.rs](../crates/uj-engine/src/scheduler.rs) 的 `Scheduler::judge_submission`。

1. 用 `sort_by_cached_key` 缓存文件大小，保留降序 LPT 和等大小时的原有顺序。例如：

   ```rust
   use std::cmp::Reverse;
   sorted_tests.sort_by_cached_key(|test| {
       Reverse(fs::metadata(&test.input_path).map(|m| m.len()).unwrap_or(0))
   });
   ```

2. 实际 worker 数取 `topology.slots.len().min(total_tests)`。没有测试时不创建 worker，但仍完成现有配置和沙盒层级检查，并生成正常空报告。
3. 将入口预检成功的沙盒实例移动给第一个 worker，其余 worker 按原机制创建各自的实例。保留及时报告初始化失败的行为，不把 Windows 不同 slot 合并成共享 SID。
4. TUI 使用的槽位数与实际 worker 数保持一致。不要为了这一项重建调度框架或引入常驻线程池。

排序的文件查询从通常的 `O(N log N)` 次降到 `O(N)` 次；收益取决于测试数量和文件系统。测试重点是等大小/不同大小输入、单点和空题、`-j 1` / 多 worker，以及 `--require-tier` 失败路径。保留 fail-fast、最终自然排序和 precise 复测行为。

### B. 子任务模式预编译（优先级 P0）

修改 [tests.rs](../crates/uj-core/src/tests.rs) 的 `discover_tests`、`test_matches_patterns`。

1. 引入一个小型的已解析模式类型，保存 Regex、GlobMatcher、数值区间或字符串；使用已有 `regex`、`globset` 依赖。
2. 每个 subtask 在扫描 cases 之前准备好模式集合，循环内只调用 `matches(stem)`。不需要全局缓存或锁。
3. 保留公开的 `test_matches_patterns` 作为便捷包装；`discover_tests` 的循环必须使用已准备好的集合，避免只改了函数内部却仍逐点编译。
4. 保持现有语义：trim、空列表不匹配、非法正则不匹配、模式按 OR 组合、区间边界、子任务覆盖顺序与权重分配。注意现有数值区间/glob 匹配失败后还会尝试字符串精确匹配，重写时不能无意删除该行为。

验证使用现有模式测试，加上非法模式、区间字符串本身作为测试名、重叠子任务的用例。测量可使用 10000 个测试名和混合模式，不必为这些名字生成磁盘输入文件。

### C. 将二进制内容准备移出逐测试点路径（优先级 P0）

主要修改 [scheduler.rs](../crates/uj-engine/src/scheduler.rs)、[judge.rs](../crates/uj-engine/src/judge.rs)、[sandbox lib.rs](../crates/uj-sandbox/src/lib.rs)。

现有 `AUTHORIZED_BINARIES` 只记录是否做过授权/预热。`fs::read(exe_path)` 和 `blake3::hash(&bytes)` 在查这个集合之前，所以命中集合仍会完整读/hash。

1. 在 `judge_submission` 启动 worker 前完成原生二进制的内容准备，生成小型准备结果，保留原始路径和实际运行路径。
2. 普通、交互、通信题的选手程序，以及 precise 重跑，共用这份准备结果；内部单点函数直接接受准备结果，不再调用完整的准备函数。核对所有调用点，不只修改传统题。
3. 工作目录与授权目标遵循现有题目/测试目录规则。若一次调用确实涉及多个 base_dir，按实际目标目录分别准备，二进制内容读取与摘要仍可共用。不要假设自定义 `tests.dir` 一定叫 `tests`。
4. 内容准备与 Windows slot 授权分开：共享只读程序内容可以准备一次，每个实际 slot 仅取得所需读取/执行权限。工作目录继续逐点创建并清理。
5. Python 等解释型路径保留每点独立脚本副本，不能把用户脚本路径当作原生 exe；提交答案题不做二进制准备。Defender 单次重试继续以原始程序身份判断，不能因路径搬动而变成每点重试。
6. 准备结果的生命周期限定在本次评测任务内。下一次调用重新准备，可看到外部 exe 或源码的变化；不新增进程级 `path -> 永久有效` 缓存。

**授权边界：**当前 helper 中确有对 `S-1-15-2-1` 的授权，并且 `grant_path_access` 使用 `F` 权限。重整这段路径时应改为给实际 slot SID 授予程序读取/执行权限，避免让任一选手改写其他点共用的程序；不要把现有宽泛授权复制到新接口或扩大到题目目录。这属于落实根目录已有约束，不需要增加 hash 验真流程。

目标是每份原生程序每次提交只读/hash 一次；如需按 slot 存放副本，副本准备次数至多随实际 worker 数增长，不能随测试点数增长。内容准备在并发运行前完成，随后只读复用。

验证传统/交互/通信入口、解释型入口、precise 重跑和跨两次调用替换程序。准备失败走正常评测错误路径；不要忽略错误后运行尚未准备好的共享文件。可用临时计数或系统跟踪确认调用次数，测完移除，不增加公开诊断字段。

### D. Windows 新建 box 复用已继承的 ACL（优先级 P0）

修改 [windows/mod.rs](../crates/uj-sandbox/src/windows/mod.rs) 与 [windows/security.rs](../crates/uj-sandbox/src/windows/security.rs)。

`create_box_dir` 已为 `slot-N` 授予 `(OI)(CI)` 可继承权限，但 `run` 又对唯一的新 `spec.work_dir` 查询同一个集合并重新调用 `icacls`。这会给每个新测试目录增加外部进程启动和递归遍历。

1. slot 父目录授权成功后，其下由本 sandbox 新创建的 box 可直接登记到已有的授权集合，使 `run` 命中该记录。仅登记本对象实际创建且继承权限的目录，不用字符串前缀猜测任意路径已授权。
2. 对调用者直接传入的其他 work_dir 保留现有必要授权。`spec.trusted` 的执行没有使用 AppContainer，先判断 trusted，避免给这类裁判工作目录做无用的 AppContainer 授权。
3. `grant_path_access` 应检查 `icacls` 的进程启动和退出结果；只有成功后才记录已授权。保留正常错误传播，不能把授权失败登记为成功。
4. 覆盖 `run_group`、直接 sandbox 调用和 C 的二进制准备路径。不同 slot 继续独立 SID，每个测试点继续独立目录。

首轮优先消除每点重复调用即可。无需同时把整套 ACL 实现换成 Win32 API；原生 DACL 更新放入后续候选。

验证同一个 slot 连续执行多个测试点，以及两个并发 slot 的文件隔离；沿用 `cheat_read`、`cheat_write` 和无残留测试。针对本对象创建的普通测试 box，`icacls` 调用次数应由随测试点数增长改为随 slot 数增长。不要以共享工作目录或关闭 Defender 达到这个结果。

### E. 大答案直接借用映射，去掉 `mmap.to_vec()`（优先级 P1）

修改 [checker lib.rs](../crates/uj-checker/src/lib.rs) 和 [judge.rs](../crates/uj-engine/src/judge.rs) 的内置比较器调用点。

1. 返回持有存储的类型，例如 `FileBytes::Owned(Vec<u8>) / Mapped(memmap2::Mmap)`，实现 `AsRef<[u8]>`。已有 checker 的 `check(&[u8], &[u8])` 接口可保留。
2. 沿用现有 64 KiB 分界作为初始值；空文件走 Owned，避免空映射。小文件从已打开的 File 读取，避免再次按路径 open。
3. 大文件映射所有者活到比较完成，比较代码借用切片；调用端不得再 `to_vec()`。检查传统、交互附加 checker、output-only 的答案加载位置。
4. 映射限于评测期间约定不变的答案等稳定文件；对仍可能被外部修改的提交文件保留 Owned 读取。只读打开本身不保证文件不会被其他进程修改，要写清 unsafe 映射的生命周期依据，无需增加内容轮询或 hash。
5. 保持现有输出限制、文件错误和缺失答案处理口径。不要为这项优化一次预载全部答案，也不要创建跨提交的答案缓存。

验证空文件、阈值两侧文件、相同内容和首尾不匹配时的判定与 diff。使用 32 MiB / 128 MiB 稳定答案比较修改前后宿主内存及耗时。mmap 仍会占用地址空间和文件页，目标是减少整份堆复制，不是宣称文件不占内存。

### F. 生成器复用 hash 前缀，答案输入流式 hash（优先级 P1）

修改 [author_cmd.rs](../crates/uj-cli/src/author_cmd.rs) 的 `run_gen`、`run_ans`。

1. `run_gen` 每个 generator 编译完成后，读取其二进制并更新一个基础 hasher，再更新 `gen_spec.args`。把它移到 `for i in 0..gen_spec.count` 外。
2. 每轮 clone 这个小型 hasher 状态，再追加原有 `i.to_le_bytes()` 并 finalize。保持现有字节拼接顺序，从而保留 cache key 语义；不要改成 hash(hash(binary) + args + i)。
3. `run_ans` 当前已经把主程序 hash 放在循环外，无需再包一层缓存。将主程序及大输入文件的摘要改为固定大小缓冲区流式计算，及时释放读取缓冲，避免完整输入 Vec 和程序输出同时驻留。
4. `cfg.to_limits()` 等循环不变量也放到循环外。保留生成器参数/编号、答案缓存、`force`、取消与失败语义。

这里只调整计算位置和读取方式。现有 gen 从最大编号后追加新输入，其缓存命中逻辑涉及编号语义；本轮不要顺带改变编号、增加输入指纹或重建缓存格式。ans 的数据读取与选手 stdin 读取仍是两次用途不同的读取，流式 hash 主要降低宿主内存峰值，不宣称消除了所有输入 I/O。

验证生成内容和编号不变，既有缓存仍能识别相同 key。测量 generator `count=100` 的运行，以及大输入生成答案时的宿主峰值内存。

### G. 在一次命令/请求内复用编译环境信息（优先级 P1）

修改 [compiler.rs](../crates/uj-engine/src/compiler.rs)，并接入 CLI 的 judge/build/verify/tl/pipeline/contest 及 [mcp_server.rs](../crates/uj-cli/src/mcp_server.rs) 相应调用点。

1. 引入轻量 `CompileSession`，由一次顶层命令或 MCP 请求持有，缓存该请求中解析过的编译器绝对路径与版本首行。
2. 同一个编译器的 `--version` 在这个 session 中调用一次。默认内置语言配置可以初始化一次；全局/题目配置按当前请求、实际配置根目录复用，不跨题目误用 overlay。
3. 源码、extra_sources、头文件、编译参数与版本仍参与原有内容缓存 key。不要为命中加第二轮二进制校验，也不要用 `mtime` 代替源码/头文件内容。
4. 保留现有静态 `Compiler::compile*` 便捷入口，由它创建短 session；批量循环显式复用同一个 session。调度器中编译 checker/interactor/manager 的调用也要接入，避免只有选手编译受益。
5. MCP 每个请求创建新 session；请求间修改 `languages.toml`、源码或更换编译器后仍能被看到。CLI pipeline 的各步骤共享 session；如命令自身修改配置，应在修改后刷新对应配置对象。

先限于环境信息复用，避免引入跨请求编译产物内存缓存。当前 CLI pipeline 与 MCP pipeline 的实现范围并不一致，分别接线，不顺带重写 MCP 流水线功能。

验证同一请求批量热缓存编译、不同题目语言 overlay，以及两次请求间修改配置/源码。测量多个已缓存源码的批量 build，记录版本查询进程数和总耗时。

### H. 比较器用迭代器与借用切片（优先级 P1）

修改 [builtin.rs](../crates/uj-checker/src/builtin.rs)。

1. `fcmp`、`lcmp` 改为逐行迭代，避免先收集双方全部行切片。可以复用已有 `memchr` 依赖寻找换行，不需要手写 SIMD。
2. 保持 `fcmp` 的行末空格/tab/CR 裁剪，以及 `lcmp` 的逐行 token 规则。中间空行不能一律跳过；双方一边 EOF 时仍按现有逻辑处理剩余空白行，保持首个差异的行号和列号。
3. `normalize_bigint` 返回借用视图，例如符号与有效数字切片；零统一处理，成功匹配不构造 String，仅在错误诊断分支格式化文字。保留 `+0`、`-0`、前导零、非法数字和超长整数语义。
4. `wcmp`、`ncmp`、`rcmp` 已经使用借用 token 迭代器，不必重写。不要给数值 checker 统一增加 `output == answer` 就 AC 的分支：相同的非法数值仍需要被校验。

验证 CRLF/LF、空输入、内部空行、尾部空白、只含空格的剩余行、行数不同和数字格式边界。百万短行与大整数 token 的基准用于观察分配减少；不为本轮单独设计内存分配器或通用 token 缓存。

## 4. 后续候选：本轮不默认扩展

这些路径也有优化可能，但涉及更大的行为变化，先记录实际开销再另做小改动。

| 候选 | 依据与方向 | 实施注意 |
| --- | --- | --- |
| Windows 原生 ACL 更新 | `security.rs::grant_path_access` 仍调用外部 `icacls`；可用已有 windows crate 的 DACL API | A–D 先去掉高频调用；后续替换时合并既有 ACE，处理 native 内存释放和错误，不扩大授权 |
| 编译器大日志阻塞 | `compiler.rs`：326–356 行把 stdout/stderr 设为 pipe，却先 `try_wait` 再 `wait_with_output`；日志超过管道容量可能堵到 30 秒超时 | 应并发持续排空两个管道，只保留诊断上限；超时必须回收进程及持有管道的后代，不能只缩短 sleep 或无限等待读线程 |
| Windows 组进程退出等待 | `windows/process.rs::execute_group_in_jobs` 扫描句柄后无条件 sleep 5 ms，全部结束的当轮也会 sleep | 可先消除最后一次无用等待；进一步使用多句柄等待时保留资源检查、取消和退出次序语义 |
| Linux cgroup 采样 | `linux/cgroup.rs::read_cpu_time_us/read_oom_killed` 每轮 open/read/分配字符串 | 只在测得监管开销明显时改为每次 run 持有句柄、从偏移 0 读取和复用缓冲；仍逐次新建 cgroup，保留最终计量与清理 |
| 出题/比赛阶段并行 | `run_validate`、`run_ans`、`run_build` 和 contest 的提交外层循环是串行；单份提交内部已有并行 | 后续优先给独立验证任务使用有界 worker；需先隔离工作目录，避免多层线程池过量并发，不并行 `tl` 的独占计时样本 |
| 长批次复用环境 | `verify/tl/contest` 对同题多次建立调度环境，TUI 也会另查拓扑 | 测得必要后再引入命令内 session；不跨提交复用可写 box，不长期保存动态可用内存值 |
| 大测试集 TUI 索引 | `uj-tui/src/lib.rs::handle_event` 对每个事件线性查找测试名 | 可保留展示 Vec 并增加一次构建的名称到位置索引；不影响无 TUI 路径，优先级低于 A–H |

不要把 `WaitForSingleObject(process, 5)` 或 Linux `poll(pidfd, ..., 10)` 误判成每个程序必付 5/10 ms：它们在退出事件到达时能提前返回。现有 `NoneSandbox` 的 sleep 轮询不能代表 Full 沙盒性能。当前 release 已启用 thin LTO、单 codegen unit；首轮无需堆叠编译选项。

## 5. 轻量测量方式

使用 release 二进制，固定测试数据、编译器、沙盒模式、worker 数与机器电源状态。每个场景记录一次预热和至少三次正式运行，报告中位数及原始耗时即可。Windows 的首次编译/Defender 冷启动单独记录，不能混入全部热运行的平均值。

计时用外部单调时钟覆盖整个命令；报告的 `time_ms` 主要是选手 CPU 时间，不能体现配置解析、ACL、hash、排序和比较器的宿主开销。宿主峰值内存也应单独测量，不能拿选手的 `memory_kb` 代替。

| 场景 | 输入与运行方式 | 主要观察 |
| --- | --- | --- |
| 大量短测试点 | 临时题目 1000 个 A+B 测试点，先编译并预热；分别 `-j 1` 和固定多 worker 数 | 整体墙钟、二进制读取/hash 次数、Windows `icacls` 次数 |
| 多子任务匹配 | 10000 个内存测试名，混合 range/glob/regex | 发现/分配耗时，模式编译次数随模式数增长 |
| 大答案 | 32 MiB / 128 MiB 稳定文件，完全相同与首尾差异；另测百万短行 | checker 耗时、宿主内存；结果和 diff 一致 |
| 批量编译热缓存 | 同一命令编译约 20 份已有缓存的源码 | 缓存命中总时间、编译器版本查询次数 |
| 出题循环 | 固定 generator 与 seed，`count=100`；大输入 ans | 程序读取次数、总耗时、宿主内存 |

基准数据放临时目录或 `.uj/perf/`，不要覆盖现有题目；用简单临时脚本/小型 Rust example 即可，无需引入 criterion 或新的基准服务。不要把预热产生的新 gen 编号拿来与另一组不同编号混比。

现有 CLI 的参数示例：

```powershell
cargo build --release --bin uj
& .\target\release\uj.exe judge tests/problems/aplusb/solutions/ac.cpp -p tests/problems/aplusb --format json --precise false -j 1
```

上面的现有题目只有 3 点，是命令及结果的快速验证；正式短程序基准需使用临时生成的 1000 点题目。设置了 `CARGO_TARGET_DIR` 时使用实际输出路径。Linux 使用 `./target/release/uj`。测量时可固定 `--precise false` 排除复测噪声，正确性验证仍覆盖默认 precise。

前后记录真实 `sandbox_tier`，不要将 Full 与 Degraded/None 的耗时混比；专门测 Full 时使用 `--sandbox full --require-tier`。当前 `--require-tier` 是布尔开关，后面不接 `full`。

## 6. 必要验证与交付

沿用根目录要求，完成 Rust 修改后按顺序执行：

```text
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Windows 使用 MSVC 工具链；WSL2/Linux 加载 `~/.cargo/env` 后也执行 `cargo test --workspace`。不要以交叉编译的 `cargo check` 代替另一平台实际测试。运行环境不足时如实记录未验证部分，不能通过放松测试或伪造 sandbox tier 获得通过结果。

对抗集成测试随完整测试运行，重点检查偷读/篡改、崩溃、TLE、MLE、OLE、Linux seccomp 和清理行为；需要单独定位时命令是 `cargo test -p uj-cli --test evil_tests`。只为改动涉及的行为边界补充测试，不引入新的全量 hash 比对或耗时断言。不同平台测试分支的跳过情况也要如实记录。

交付实际代码及简短记录，至少包含：

1. A–H 各项完成情况和实际修改文件；后续候选不混写成已完成。
2. 代表场景的修改前/后墙钟时间、宿主内存，以及已测得的关键调用次数；没有测得的数字标明未测。
3. Windows 与 Linux 的检查结果及具体未解决项。
4. 行为验证以 verdict、score、diff、隔离层级等语义字段为准；运行时间、内存和随机目录名不要求字节级一致。

用局部生命周期把重复成本从每点一次降到每提交/每 slot 一次，是本轮的主要实施方向。完成 A–H 后以测量结果决定下一轮重点，不继续堆叠缓存层或校验门控。
