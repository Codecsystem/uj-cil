# uj 改进方案（缺陷 / 性能 / 新功能）

> 本文由一次完整代码通读得出，**面向"找 bug + 提速 + 扩功能"**，不是网络安全审计。
> 读取基线：`main @ 1a9bcee`。所有定位用 `文件:行号`，重构后以语义为准。
>
> 说明：`docs/PROJECT_AUDIT_EXECUTION_2026-10-11.md` 把 S0–S7 大量条目勾成 `[x]`。
> 本次逐行复核发现**部分"已修"项代码里仍存在**（见 §1 标注 ⚠️"审计称已修但仍在"）。
> 本文只列"读代码能确认"或"读代码强烈怀疑"的问题，未做运行复现的用 🔶 标注。

---

## 1. 缺陷审查（正确性 / 挂起 / 崩溃 / 陈旧产物）

严重度：**P0** = 可错判 / 可挂死 / 可随机崩；**P1** = 特定题型或路径下行为错误；**P2** = 边界 / 可维护性。

### P0-A　Linux：多线程 fork 后在 `pre_exec` 中做大量非 async-signal-safe 操作 🔶
**位置**：`crates/uj-sandbox/src/linux/runner.rs:185-733`（整个 `cmd.pre_exec(...)` 闭包）
**问题**：
- 调度器 `scheduler.rs` 会起 `slot_count` 个 worker 线程，每个线程走 `judge_single_test → sandbox.run → execute_in_sandbox → Command::spawn()`。因此**宿主进程是多线程的**。
- `std::process::Command::spawn` 的 `pre_exec` 回调运行在 `fork()` 之后、`execve()` 之前的子进程里。Rust 标准库明确要求此处只能调用 async-signal-safe 函数。
- 但当前闭包里做了：`Vec`/`String`/`CString::new`/`format!` 分配、`fs::write`、`fs::create_dir_all`、`File::create` 等。这些会走 `malloc`。若 fork 发生的瞬间**另一个 worker 线程正持有 malloc 全局锁**，子进程里再次 `malloc` 会**永久死锁**（经典的 fork-in-threaded-program 陷阱）。
**后果**：并发评测（`-j > 1`）下，Linux 偶发单个测试点卡死在子进程、墙钟到点才被 kill，表现为随机 TLE / 随机挂起，难复现。并发度越高越容易触发。
**注**：审计 P1-18 已提到"应改为 re-exec helper 或 spawn 前预构造资源，child 只做 async-signal-safe syscall"，`SANDBOX.md` 也写了"re-exec `uj __sandbox-init`"，但**主路径并未落实**，仍是巨型 pre_exec 闭包。这是本仓库**Linux 侧最高优先级可靠性缺陷**。
**修法**：
1. 真正落地 `uj __sandbox-init` helper：worker 只 `Command::new(self_exe).arg("__sandbox-init")`，把配置通过 fd / 环境变量传入，所有 namespace/mount/rootfs 工作在 helper 的 `main()`（单线程、exec 之后）里做。
2. 或：在 `spawn` 之前预构造全部 `CString`/路径/rootfs 模板，`pre_exec` 内只调用裸 `libc::*` syscall，严禁任何分配与 `std::fs`。

### P0-B　编译缓存漏掉嵌套 include 与 Rust/Go 多文件依赖 ⚠️
**位置**：`crates/uj-engine/src/compiler.rs:320-353`
**问题**：缓存 key 的头文件依赖只扫描了
- `include_dirs` 的**顶层** `.h`/`.hpp`，和
- **源文件同目录**的 `.h`/`.hpp`。

遗漏：
- `#include` 到的**嵌套 / 其它目录**头文件；
- `.hh`/`.hxx`/`.inc`/`.cuh` 等扩展名；
- **Rust `mod`/Go `import` 的其它源文件**（只 hash 了单个 `.rs`/`.go`）。
**后果**：改了被 include 的头或另一个 `.rs`/`.go` 文件，`compile()` 命中旧缓存，**跑的是旧二进制**。出题校验、对拍、本地调试会用过期产物得到错误结论。审计 P0-05 勾了 `[x]`，但代码仍是"扫同目录 + include_dirs 顶层"的保守近似，**未实现真正的依赖清单**。
**修法**：
1. 优先用编译器自带依赖生成（`gcc -MM` / `rustc --emit=dep-info` / `go list -deps`），把依赖文件全部纳入 hash。
2. 退化方案：递归扫描且覆盖全部头文件扩展名，并在文档里写明其保守失效策略。

### P0-C　多选手通信题的管线拓扑是"环"，几乎必然错 ⚠️🔶
**位置**：`crates/uj-engine/src/judge.rs:1607-1624`
**问题**：`num_participants > 1` 时，wiring 连成一条环：
```
manager.stdout -> p1.stdin
p1.stdout      -> p2.stdin
...
pN.stdout      -> manager.stdin
```
真实通信题的常见拓扑是 **manager 与每个选手各自双向互联**，而不是选手之间串成传话链。当前实现下，manager 只能把输入喂给 p1、只能从 pN 读回，中间选手的 I/O 语义完全不对。
**后果**：`processes > 1` 的通信题判题结果无意义。审计 S5 勾了"通信题按角色/拓扑运行 `[x]`"，但多选手拓扑实际是错的。单选手（`num_participants==1`，双向）是对的。
**修法**：按配置声明的角色与连接拓扑建边（manager↔每个 participant 各一对管道），而非位置环。短期可在配置校验阶段**拒绝 `processes > 1`** 并明确报"暂不支持"，好过静默跑错。

### P0-D　Linux 共享 cgroup base 目录在 Drop 时被整体删除 🔶
**位置**：`crates/uj-sandbox/src/linux/mod.rs:122-135`（`cleanup_stale` 与 `Drop`）
**问题**：`cgroup_base` 解析为同一 systemd scope 下共享的 `<scope>/uj`（`cgroup.rs:112-115`）。`LinuxSandbox::drop` 和 `cleanup_stale` 都执行 `remove_dir_all(base)`。**同一宿主上并发跑两个 uj 进程**（例如一个 MCP server + 一次手动 `uj judge`）时，一个进程结束会把另一个进程正在用的 `uj` 子 cgroup 一起删。
**后果**：并发 uj 实例互相破坏 cgroup，导致第二个进程计量失准或报错。
**修法**：每个进程用带 pid 的子目录（`uj/<pid>/...`）作为自己的 base，只删自己的；或引入运行所有权记录。

### P0-E　`prepare_binary_content` 内存快表以 mtime+size 为键
**位置**：`crates/uj-sandbox/src/lib.rs:371-385`
**问题**：内存 `PREPARED_BINARIES_CACHE` 键为 `(exe_path, base_dir, mtime, size)`，命中即返回旧 `authorized_exe`，**不校验内容**。
**现实风险评估**：相对较低——因为上游编译产物路径本身是"内容 hash 目录"，内容变则路径变、键也变。但该函数是 `pub`、也被 interpreter/其它路径直接调用，一旦调用方传入**路径稳定而内容变化**的二进制（如外部预编译 exe 被覆盖、同秒同长替换），就会复用旧文件。
**修法**：快表命中后仍对目标做一次 size 校验已有，建议再退化为"候选 + 内容 hash 确认"，或直接以内容 hash 为键（与磁盘侧一致）。

### P1-F　NDJSON：开头发一个占位 `Summary(AC/0)`，结尾无真正 `Summary`/`SubtaskDone`
**位置**：`crates/uj-engine/src/scheduler.rs:260-267`（占位 Summary）、全流程无终结 `Summary`/`SubtaskDone`
**问题**：评测一开始就发 `Summary { final_verdict: Accepted, final_score: 0.0 }`，结束只发 `SubmissionDone`。`PROTOCOL.md` 第 4 节声称事件流含 `subtask_done` / `summary` 终结事件，实际：`SubtaskDone` 从不发送，`Summary` 只在开头发且内容是假的。
**后果**：NDJSON 消费者若信任开头的 Summary 会读到"AC / 0 分"的假终态；订阅方拿不到 `subtask_done`。
**修法**：删除开头占位；在结尾按协议补发 `SubtaskDone`（每个子任务一条）与真实 `Summary`，并保证错误路径也有终结事件。

### P1-G　testlib exit 7 固定映射 0.5 分
**位置**：`crates/uj-checker/src/testlib.rs:163-171`
**问题**：exit code 7（points）被硬编码成 `score_ratio = 0.5`，完全无视 checker 实际想给的分数（testlib 的分数应从 stderr / `-appes` XML 的 points 读取）。
**后果**：用 exit 7 返回部分分的 testlib checker，一律按半分算，错分。
**修法**：exit 7 时优先解析 `-appes` XML 的 `points`，无 XML 时从 stderr 约定字段取数；取不到则 `JudgeFailed` 而不是猜 0.5。

### P1-H　交互题"结果文件在选手可写目录"——已改但需确认 bind 边界 ⚠️
**位置**：`crates/uj-engine/src/judge.rs:863-873`
**现状**：已把 `trusted_dir`/`result_file`/`tout_file` 放到 `box_dir.parent()` 下的 `trusted_int_*`，不再放在 `box_dir` 里（审计 P0-01 的主要修复已落地，👍）。
**残留疑点**：Linux Full 下选手 namespace 只 bind `work_dir`(= box_dir) 到 `/box`，`trusted_dir` 是 box 的**兄弟目录**，默认不在选手视图内——看起来安全。但需补一条定向回归：选手尝试创建/改写同名 `interact_res_*.xml`，断言裁判结论不变（Windows Full + Linux Full 各一条）。目前 `tout_file` 由交互器写、后处理 checker 读，其信任链也应在测试中钉死。

### P1-I　`build_windows_cmd_line` 引用算法不符合 MSVC argv 规则
**位置**：`crates/uj-sandbox/src/windows/process.rs:927-943`
**问题**：仅在含空格/制表/空串时加引号，转义只做 `"` → `\"`，**未处理结尾反斜杠**。路径以 `\` 结尾且需要加引号时（如 `C:\dir\ `），会生成 `"...\"`，被解析器吞掉引号。
**后果**：含空格且以 `\` 结尾的路径 / 带内嵌引号的参数会被错误拆分。竞赛常见路径一般没事，但中文带空格目录 + 末尾分隔符会踩。
**修法**：实现标准 Windows argv quoting（成对反斜杠 + 引号前反斜杠翻倍）。

### P1-J　编译器外部命令缺超时/无并发 reader 保护的点
**位置**：`crates/uj-engine/src/compiler.rs:170-182`（`--version`）、`:257-266`（`py_compile`）
**问题**：主编译进程已改用并发 reader + 30s 超时（`:469-533`，👍）。但：
- `resolve_compiler_version` 的 `Command::new(bin).arg("--version").output()` **无超时**；
- Python `py_compile` 的 `.output()` **无超时**。

正常编译器不会卡，但 PATH 里放了异常的同名程序、或 NFS/Defender 抖动时可能挂住整条评测。
**修法**：给这两处也套统一的带超时 `CommandRunner`（纯线程实现，别引 async）。

### P2-K　`none` 与 Full/Windows 对 stderr 超限处理不一致
**位置**：`none.rs:174-195`（持续 drain 保留 tail）vs `windows/process.rs:913-921`（达 cap 后 drain 丢弃）vs `linux/runner.rs:789-807`（达 cap 直接 break、**停止读取**）
**问题**：Linux reader 在 stderr 超过 `stderr_limit` 时 `break` 退出读取循环，不再 drain。若选手大量写 stderr，管道写端会被填满 → 选手进程在 `write(2)` 阻塞 → 可能把一个本应正常结束的程序拖成墙钟 TLE，verdict 被 stderr 行为改变。Windows / none 则继续 drain。三端语义不一致。
**修法**：统一为"达 cap 后继续 drain 丢弃"（三端一致），stdout OLE 才是"达 cap 杀进程组"。

### P2-L　`judge_single_test` Windows Defender 重试集合永不清空
**位置**：`crates/uj-engine/src/judge.rs:160-162`
**问题**：`RETRIED_BINARIES` 是进程级 `static`，只增不减。长驻 MCP server 里，"首次运行放宽墙钟"的语义对同一路径只触发一次（整进程生命周期），一般没问题；但若同路径被重编译替换、确实需要再次预热，不会再放宽。属可接受取舍，记录备查。

---

## 2. 性能优化（重点）

> 前提：所有优化先有正确性回归，再测 P50/P95 / RSS / syscall 数，不能"减检查换速度"。
> 下面按**预计收益 × 落地难度**排序。

### 性能-1　Linux 每个测试点都重建整套 rootfs —— 最大的 Linux 开销 ⭐⭐⭐
**位置**：`crates/uj-sandbox/src/linux/runner.rs:226-461`
**现状**：每个测试点在 `pre_exec` 里都要：建 tmpfs rootfs → `MS_BIND` 挂 `/usr`（递归）→ 建 `/tmp`/`/dev`/`/etc` → bind `/dev/{null,zero,urandom,random}` → `pivot_root` → remount ro。**每个测试点上百次 mount/mkdir syscall**。
**影响**：N 个测试点 × M 份提交 = 海量重复挂载；这是 Linux 评测单点延迟的大头，也放大了 P0-A 的 fork 风险面。
**优化**：
- **每个 slot 建一次 rootfs 模板并复用**：rootfs 的 `/usr`/`/dev`/`/etc`/符号链接一次建好，后续测试点**只重新 bind `/box`**（换 work_dir）即可。
- 配合 §1 的 `__sandbox-init` helper，把模板构建移出热路径。
- `/usr` 用 `MS_BIND|MS_REC` + 只读 remount 一次即可，不必每点重挂。
**预期**：Linux 单测试点启动开销下降一个量级（上百 syscall → 十几个）。

### 性能-2　答案文件每个测试点重新读进堆 ⭐⭐⭐
**位置**：`crates/uj-engine/src/judge.rs:385`（`read_artifact_bounded(ans_path,...)` 每点一次）
**问题**：内置 checker 路径下，每个测试点都把答案文件整体读进 `Vec`。**contest 模式下 M 份提交 × 同一题 N 个测试点 = 同一个答案文件被读 M 次**。`uj-checker` 明明有 `read_file_bytes`（≥64KiB 走 mmap），但 judge 热路径没用它。
**优化**：
- 对答案/输入文件引入**按路径的进程级 mmap 缓存**（`HashMap<PathBuf, Arc<FileBytes>>`，带 mtime 失效），跨提交复用同一映射，零拷贝。
- 内置 checker 直接吃 `&[u8]` 切片，省掉每点一次全量读盘 + 堆分配。
**预期**：contest / 多提交场景答案 I/O 与峰值 RSS 显著下降；大答案（MB 级）尤甚。

### 性能-3　编译缓存"快路径"仍需读源 + 扫头文件 + 全量 hash ⭐⭐
**位置**：`crates/uj-engine/src/compiler.rs:233`（读源）、`:298-353`（hash 源 + 所有 extra + 所有头）
**问题**：`fast_results` 以 `cache_hash` 为键，而算 `cache_hash` **必须先读源文件 + 读并 hash 全部相关头文件**。所以哪怕是"秒级重复 compile 同一源"，每次都重读磁盘、重 hash（testlib.h 近 200KB 也会被扫到就全读）。出题流水线里 `compile()` 被反复调用，这笔开销是实打实的。
**优化**：
- 在 `fast_results` 之前加一层"**便宜预检**"：以 `(source_path, mtime, size, flags_version)` 做轻量 key；命中且 mtime/size 未变直接返回，跳过读源与头扫描。
- 真正失效判定（内容 hash）作为慢路径兜底。注意这和 §1 P0-B 不冲突：预检只在"同一会话、文件未被 touch"时短路。
**预期**：authoring pipeline 的 `build/ans/verify` 重复编译步骤从"每次读盘+hash"降到亚毫秒。

### 性能-4　拓扑/内存探测每次 `judge_submission` 都做一遍 ⭐
**位置**：`crates/uj-engine/src/topology.rs:23-47`（`sysinfo::System::new()` + `refresh_memory`）、`scheduler.rs:76`
**问题**：每份提交评测都 `Topology::detect`，内部 `detect_p_core_primary_cpus()`（Windows 要调 `GetLogicalProcessorInformationEx`）+ `sysinfo` 刷内存。contest 批量下重复几十上百次。
**优化**：进程启动时探测一次 `HostCapabilities`（P 核列表、物理核数、可用内存基线），注入到 scheduler / UI，避免重复系统调用与机器状态漂移。
**预期**：contest 批量启动更平稳，去掉每提交的系统调用抖动。

### 性能-5　Windows 单进程监控 busy-loop 的收尾等待 ⭐
**位置**：`crates/uj-sandbox/src/windows/process.rs:289`（`WaitForSingleObject(pi.hProcess, 5)`）
**现状**：主循环每 5ms 轮询，合理。但 `max_processes>1` 且主进程已退出、仍有后代存活时，`WaitForSingleObject` 对已退出的主句柄**立即返回**，循环退化为"每 ~0ms 空转 + 查 active_processes"直到后代退出。虽有 `job.terminate(3)` 兜底（在超限时），正常退出路径下这段空转查询略费 CPU。
**优化**：主进程退出后，改为对 **Job 的 IOCP**（已关联）等 `JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO` 通知，而非自旋查 `query_active_processes`。顺带让"组内全部退出"事件驱动化。
**预期**：多进程题（交互/通信）收尾 CPU 占用下降，判定更及时。

### 性能-6　`sorted_tests = tests.clone()` 与结果再排序 ⭐
**位置**：`crates/uj-engine/src/scheduler.rs:254`、`:531`
**问题**：为 LPT 排序克隆整个 `Vec<TestCase>`（含 PathBuf 等），之后结果又按 natural_cmp 排一遍。测试点多时有一次多余深拷贝。
**优化**：排序索引而非克隆对象（`Vec<usize>` + 按 size 排序），或 `Arc<TestCase>` 共享。收益随测试点数量线性。

### 性能-7　`clean_box_dir_contents` 每点 readdir + 删文件 ⭐
**位置**：`crates/uj-engine/src/judge.rs:70`（复用 box 时每点 `clean_box_dir_contents`）
**现状**：slot box 池化已做（Round 6）。但每个测试点仍 `read_dir` + 逐个删。对只产生 stdout（Pipe）的传统题，box 里往往只有 checker 的临时 `uj_out_*.txt`（还是写进 box 的，见 `judge.rs:430-438`）。
**优化**：
- 传统题内置 checker 根本不需要把 stdout 落盘成 `uj_out_*.txt`——那是 testlib/lemon 路径才需要。内置 checker 直接吃内存 bytes，**别写临时文件**，省掉每点一次写+删。
- testlib/lemon 的临时输出文件放到独立的 per-slot 固定路径并截断复用，而非每点新建带时间戳的文件名（`judge.rs:430`、`:464`）。

### 性能-8　release profile 可再压一档 ⭐
**位置**：`Cargo.toml`（`lto = "thin"`, `codegen-units = 1`, `strip = true`）
**建议**：比较器/沙盒是 CPU 密集，`lto = "fat"` 通常再给几个百分点；`panic = "abort"` 可减小体积并略快（需确认没有依赖 unwind 的测试）。属锦上添花，务必基准对比后再定。

---

## 3. 可新增的功能方向

按"对本项目（校赛出题 + AI Agent 出题）价值"排序。

### 功能-1　真正的依赖感知 + 跨会话持久编译缓存
在修掉 P0-B 的基础上，把 `.uj/cache/bin` 做成带**依赖清单 + 内容 hash + 原子发布 + LRU 淘汰**的持久缓存（审计 P2-01/P2-06 的方向）。收益：出题流水线反复 `build` 近乎免费、换机器/重启后仍命中。

### 功能-2　更多语言矩阵并与文档对齐
现状 `assets/languages.toml` 有 cpp17/cpp20/c11/python3/java/rust/go，但：
- 同扩展 `cpp` 同时被 cpp17 和 cpp20 声明，选择依赖 HashMap 顺序（不确定）——应支持 `problem.toml` 显式 `language_id` 或稳定优先级，并在歧义时报配置错误。
- 可加 PyPy、Node.js、C#、Kotlin。每种语言配一个最小 E2E fixture，README 只宣称"已过矩阵"的语言。

### 功能-3　Polygon / 标准题包导入导出
支持 Codeforces Polygon package、或定义 uj 自己的可打包题目格式（含 manifest、seed、生成器 hash）。方便校赛题在不同机器/评测系统间迁移，也契合 AI Agent 批量出题。

### 功能-4　生成 manifest 驱动的幂等出题流水线
给 `gen`/`ans`/`validate` 产出**生成清单**（输入编号、seed、generator/validator/输出 hash、子任务、时间戳）。同配置重跑默认幂等，`--append` 才追加。解决审计 P1-07 "重复 gen 追加测试" 与"缓存只看文件是否存在"的可重复性问题。

### 功能-5　`uj watch` 作者模式
监听题目目录变更，自动增量跑 `build → gen(变化项) → validate → verify`，终端实时反馈。配合依赖缓存，出题体验接近"保存即校验"。

### 功能-6　对拍（stress）结果可复现与最小化
找到反例后：保存触发 seed、自动 **delta-debug 最小化输入**、把反例固化成回归测试点。现状 stress 只比对 stdout，应先检查三程序各自的 exit/kill/OLE（审计 P1-08）。

### 功能-7　更强的报告
- HTML 报告加**交互式 diff 视图**（期望/实际逐 token 高亮，已有 `DiffSummary` 基础）。
- 子任务依赖 DAG、得分分布、每测试点时间/内存热力图可视化。
- CSV 对 CE / 无测试点的提交也输出 summary 行（审计 P1-16/P1-17）。

### 功能-8　资源感知调度（CPU + 内存双预算）
`Topology::detect` 已按内存粗算 slot 上限，但显式 `-j` 会绕过内存预算（`topology.rs:58`）。做成"slots 同时受 P 核数与内存预留约束"，并暴露 queue/slot 利用率指标，避免大内存题 `-j` 开太高把宿主拖垮。

### 功能-9　分布式 / 远程 worker（远期）
把 `GroupSupervisor`/`JudgeService` 抽象后，支持把测试点派发到远程机器，contest 大批量评测可横向扩展。需先完成服务层边界重构（审计 §8）。

---

## 4. 建议的落地顺序

| 阶段 | 内容 | 理由 |
|---|---|---|
| 第 1 批（正确性，必须先做） | P0-A（Linux fork 安全）、P0-B（编译依赖）、P0-C（通信拓扑或暂禁多选手）、P1-F（NDJSON 事件）、P1-G（exit 7 计分） | 这些会直接错判或随机挂死，且不依赖架构重构 |
| 第 2 批（性能，重点） | 性能-1（rootfs 模板复用）、性能-2（答案 mmap 缓存）、性能-3（编译预检短路） | 收益最大，且第 1 批修完后可安全提速 |
| 第 3 批（稳健 + 一致性） | P0-D、P0-E、P2-K、性能-4/5/6/7 | 并发多实例、跨端一致、尾延迟 |
| 第 4 批（功能） | 功能-1/2/4/5 优先（出题体验与可重复性），其余按需 | |

> 每批完成后按 `AGENTS.md` §4：`cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace`，并在 **Windows MSVC 与 WSL2** 双端验证；凡动 `uj-sandbox`/`uj-engine` 跨平台逻辑尤其如此。
```

### 附：与现有审计文档的差异提示
`PROJECT_AUDIT_EXECUTION_2026-10-11.md` 将 P0-05 / P1-18 / S5 通信题等勾为已完成，但本次通读发现对应代码仍是近似实现或未落实（见 P0-A / P0-B / P0-C）。建议把"勾选"口径改为"有对应双端回归测试通过"而非"写了相关代码"。
