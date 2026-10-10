# uj 验收报告（第 1 轮）

**结论：❌ 不通过。** 能编译，62 个测试全过，命令也都在；但四项核心要求里，「沙盒健全」和「判定准确」两项都没达标：
- **Linux 沙盒根本没实现**：代码直接调用无隔离后端，却把自己报告成 Full 层级。
- **Windows 上，被测程序能读取标准答案、改写测试数据**。
- **新编译的程序第一次运行会被大面积误判为 TLE**。

---

## 1. 验收方法

| 项 | 做法 |
|---|---|
| 构建与测试 | `cargo build --release`、`cargo test --workspace`：通过（62 个测试） |
| 代码审查 | 沙盒、调度、判定、编译四个核心模块逐行看过 |
| 恶意程序实测 | 在本机（Windows，24 核 / 32 线程）上用 `uj judge` 实际评测 22 个恶意/边界程序和 2 个作弊程序 |
| 性能 | 500 个测试点的 A+B，分别用 `-j 1`、`-j 4`、默认并行度 |

测试程序已复制到 `tests/evil_seed/`，可以复现（`uj judge -p tests/evil_seed tests/evil_seed/sol/<名字>.cpp`）。注意：`cheat_read.cpp`、`cheat_write.cpp` 里写死了验收时用的绝对路径，复用前要改成实际题目目录的路径。

---

## 2. 问题清单（按严重程度）

### 🔴 P0 阻断级

**① Linux 沙盒是假的**
- [linux/mod.rs:47-52](file:///e:/Data/Work/uj-cil/crates/uj-sandbox/src/linux/mod.rs#L47-L52)：`run()` 直接调用 `NoneSandbox`（完全不隔离），却把结果标成 `sandbox_tier = Full`，时间置信度报告为 high；`run_group` 还是**顺序**执行的，交互题会死锁。
- 计划里 P1 的内容（helper/init 进程树、namespace、cgroup、seccomp、挂载、残留清理）**一行都没写**；git 历史里也没有 P1 的提交。
- [SANDBOX.md](file:///e:/Data/Work/uj-cil/docs/SANDBOX.md) 却把 Linux 沙盒写成了已实现。`--require-tier` 在 Linux 上也照样通过。

**② Windows：被测程序能读答案、写题目目录（已实测）**
- [scheduler.rs:63-69](file:///e:/Data/Work/uj-cil/crates/uj-engine/src/scheduler.rs#L63-L69) 每次评测都对**整个题目目录**执行 `icacls /grant *S-1-15-2-1:(OI)(CI)F /T`（[security.rs:126-138](file:///e:/Data/Work/uj-cil/crates/uj-sandbox/src/windows/security.rs#L126-L138)）。
- 实测：`cheat_read.cpp` 直接读取 `tests/1.ans` 并输出 → **AC**；`cheat_write.cpp` 在 `tests/` 下写出了 `HACKED.txt` → **写入成功**。
- 附带问题：这是**永久修改用户文件的权限**，并且授权对象是 ALL APPLICATION PACKAGES（系统里所有 AppContainer 应用，包括 Edge 和商店应用都能访问），不只是 uj 自己的沙盒。

**③ Windows Defender 导致首次运行被误判 TLE（已实测）**

同一批程序，第一次评测和第二次（Defender 已扫描过）的结果：

| 程序 | 第 1 次 | 第 2 次 | 正确结论 |
|---|---|---|---|
| loop | TLE，cpu=1ms，「疑似空闲」 | TLE，cpu=1015ms | TLE(Cpu) |
| mle_touch | TLE，cpu=1ms，内存 392KB | **MLE** | MLE |
| mle_vector | TLE，cpu=1ms | **MLE** | MLE |
| deep_rec | TLE，cpu=1ms | **AC** | AC |

第一次的进程根本还没开始执行（内存才 392KB），就被 2 s 的墙钟限制杀掉了。临界复测也救不回来（三次采样都是 1,1,1）。Agent 出题时**每份代码都是刚编译的**，所以这是系统性误判。

**④ 所有运行共用同一个工作目录**
- [judge.rs:60](file:///e:/Data/Work/uj-cil/crates/uj-engine/src/judge.rs#L60)：所有运行的 cwd 都是 `C:\ProgramData\uj\tmp`。实测 `write_cwd.cpp` 写出的 `leak.txt` 在评测结束后**仍然留在那里**。
- 后果：并行的文件 IO 题会互相覆盖；一次运行能看到、也能篡改其它运行的文件（checker 用的临时输出文件也放在这个目录里）。

### 🟠 P1 判定准确性

| # | 问题 | 证据 |
|---|---|---|
| ⑤ | `abort()` 被判成 TLE(Wall)，应为 RE | 两种层级下都复现。错误弹窗没有被屏蔽：没调用 `SetErrorMode`，UI 限制被注释掉了（[job.rs:76-90](file:///e:/Data/Work/uj-cil/crates/uj-sandbox/src/windows/job.rs#L76-L90)） |
| ⑥ | 栈溢出被判成 TLE(Wall)，cpu=1ms，内存 396KB，应为 RE | 第二次运行仍然复现 |
| ⑦ | **单次运行失败时，静默降级为无隔离** | [windows/mod.rs:78-86](file:///e:/Data/Work/uj-cil/crates/uj-sandbox/src/windows/mod.rs#L78-L86)：AppContainer 启动失败就改用普通 Job 重跑（用户的完整权限，没有受限令牌和低完整性级别）。实测 degraded 模式下**能连上 1.1.1.1:80**；结果里只把置信度标成 medium，没有任何告警 |
| ⑧ | 通信题的选手进程不在 AppContainer 里 | [process.rs:449](file:///e:/Data/Work/uj-cil/crates/uj-sandbox/src/windows/process.rs#L449) 只有下标 0 的进程启用 AppContainer，而通信题中选手是下标 1；另外只支持 1 个选手实例 |
| ⑨ | 绑核错误 | [scheduler.rs:381-383](file:///e:/Data/Work/uj-cil/crates/uj-engine/src/scheduler.rs#L381-L383) 按任务序号预先分配核心，而不是按 worker，所以两个同时运行的程序可能被绑到同一个核；[topology.rs:51-53](file:///e:/Data/Work/uj-cil/crates/uj-engine/src/topology.rs#L51-L53) 把逻辑核编号当物理核用，没有避开超线程兄弟核；**本机是大小核混合架构（24 核 / 32 线程），没有区分 P 核和 E 核，同一程序跑在不同槽位上时间会不一样**；临界复测写死在逻辑核 1 上，而它正好是预留核 0 的超线程兄弟 |
| ⑩ | Job 内存上限实际没有生效 | mle_vector 在 ML=256MB 下峰值达到 617MB；只是靠轮询事后判成 MLE。来不及轮询时，大量分配会拖垮整机 |
| ⑪ | 无限输出被判成 TLE，而不是 OLE | `ole.cpp` |

### 🟡 P2 功能缺口

| # | 问题 |
|---|---|
| ⑫ | [compiler.rs](file:///e:/Data/Work/uj-cil/crates/uj-engine/src/compiler.rs)：完全**没读 languages.toml**，编译参数写死为 `-O3 -Wall -Wextra`（计划是 `-O2`），也不支持 Java；**编译不在沙盒里且没有超时**，`#include "CON"` 或模板炸弹会让 uj 永远卡住；缓存放在**当前工作目录**的 `.uj/`，而不是题目目录；头文件内容没进缓存 key，改了 grader 头文件会用到旧二进制；`truncate_10k` 按字节截断字符串，GCC 输出的 `‘’` 这类多字节字符会导致 **panic** |
| ⑬ | 批量评测时**遇到一个 CE 就整体中止**：22 份代码评到第 10 份（net.cpp 编译失败）就停了，后面 12 份没有评 |
| ⑭ | `--format json` 评测多份代码时输出多个拼在一起的 JSON 文档，无法作为一个 JSON 解析（违反计划 4.3 节） |
| ⑮ | 没有 `tests/evil/` 恶意程序矩阵；沙盒测试只有 6 个，部分还用的是 `--sandbox none`。在 Linux CI 上会「全绿」，因为 Linux 根本没有隔离 |
| ⑯ | 单进程每次运行约 29 ms（目标 ≤ 15 ms）：监控循环里 `WaitForSingleObject(5ms)` 之后又 `sleep(5ms)`，重复等待 |
| ⑰ | 分数用 f64 累加，计划要求定点整数 |

---

## 3. 做得好的部分（Windows Full 层级下实测通过）
- 写宿主文件被拦截，网络被断开，创建子进程被阻止（fork 炸弹、孤儿进程都没能启动），评测结束后**零残留进程**。
- 段错误、除零、非零返回码都正确判为 RE，并给出 NTSTATUS 名称；sleep 判为 TLE(Wall)；第二次运行时 MLE 判定正确；1e6 层递归在 256MB 栈下 AC。
- 交互题判定规则基本按计划 7.4 节实现；CLI 命令齐全，有 schema 输出和结构化错误。
- 性能：500 个点默认并行 2.6 s（`-j 1` 时 14.4 s）。

---

## 4. 交还 Gemini 的修复顺序（每步都要附带测试）

1. **立刻止血**：在 Linux 上，`LinuxSandbox` 在真正实现之前必须返回 `UnsupportedPlatform`，不能再冒充 Full；更正 SANDBOX.md。
2. **重做 Windows 授权模型**：每次运行建立独立的 box 目录（`.uj/tmp/run-<id>/`），只对这个目录授权，并且只授权给 uj 自己的 AppContainer SID（不能用 ALL APPLICATION PACKAGES）；可执行文件复制进 box；输入通过句柄传入；**题目目录不做任何授权**；运行结束后删除 box。把 cheat_read 和 cheat_write 加进测试，期望结论都是不能得逞。
3. **处理 Defender**：编译完成后先预热一次（以挂起方式启动后立即终止，等扫描完成再开始计时）。墙钟超时且 CPU 时间低于 TL 的 10% 时，判为「可疑」，自动重跑一次。拿新编译的程序测试，要求第一次运行的结论正确。
4. **抑制错误弹窗**：spawn 前调用 `SetErrorMode`，启用 UI 限制；abort 和栈溢出都要判为 RE。
5. **禁止静默降级**：AppContainer 启动失败时返回 SandboxError，判 JudgeFailed。Degraded 层级要么按计划实现「受限令牌 + 低完整性级别」，要么必须显式指定才能使用，并且每次运行都告警。
6. 通信题：所有选手进程都必须放进 AppContainer，并支持 N 个实例。
7. 绑核：槽位改为由 worker 独占；按物理核和 EfficiencyClass 选核，**只用 P 核**，跳过超线程兄弟核。
8. 编译：读取 languages.toml；编译放进沙盒并设置超时；缓存放在题目目录下的 `.uj/`；把头文件内容纳入缓存 key；按字符边界截断。
9. 批量评测时 CE 不中止，继续评下一份；json 模式只输出一个 JSON 文档。
10. 补上计划 10.1 节的 `tests/evil/` 矩阵（本次的 24 个程序可以直接作为种子），在 Windows CI 中运行。
11. **最后单独做 Linux 沙盒（P1）**：必须在 Linux 或 WSL2 环境里开发，并在 ubuntu CI 上用 `--require-tier full` 跑通 evil 矩阵。Gemini 很可能是因为在 Windows 上没法测试，才跳过了这一部分。

> [!WARNING]
> uj 已经永久修改了这些目录的权限，需要撤销：
> `icacls "E:\Data\Work\uj-cil\tests\problems" /remove *S-1-15-2-1 /T /C`
> 另外，你用 uj 评测过的其它题目目录也要执行同样的命令。
