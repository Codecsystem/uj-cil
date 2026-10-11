# uj 第二轮验收报告

> 范围：Gemini 返修后的工作区（**未提交**，23 个文件 +907/−306，新增 `evil_tests.rs`、`tests/evil/`）
> 环境：Win11，24C/32T（8P+16E），MinGW g++ 14.2，Defender 实时保护开启
> 方法：`cargo build/test`、在全新目录用恶意程序集实测、500 点性能测试、`cargo check --target x86_64-unknown-linux-gnu`

## 结论

| 平台 | 结论 |
|---|---|
| **Windows** | ⚠️ **安全问题已基本修好，但判定准确性和性能各有 1 个必修项** |
| **Linux** | ❌ **不通过：编译不过，沙盒仍未实现** |

第一轮的 P0 安全问题（读答案、写题目目录、改 ACL、残留）**全部修复，已实测确认**。

---

## 1. 已修复（实测确认）

| 第一轮问题 | 现在的表现 |
|---|---|
| 选手程序读答案（cheat_read） | 被阻止 → WA ✅ |
| 选手程序写题目目录（cheat_write） | 被阻止 ✅ |
| 题目目录被加 `ALL APPLICATION PACKAGES` ACE | 不再修改题目目录；每次运行使用独立的 `.uj/tmp/run-*/box`，只授权给 uj 自己的 AppContainer SID ✅ |
| 残留目录/进程 | run 目录结束后删除，进程无残留 ✅ |
| abort / 栈溢出 判错 | → RE ✅ |
| OLE 检测 | ✅ |
| 死循环 | → TLE（cpu 1015 ms）✅ |
| 遇到 CE 后批量评测中断 | 会继续评测，json 输出为数组 ✅ |
| 静默降级 | Degraded 会打印警告 ✅ |
| 通信题选手进程使用 `trusted` | 选手进程进入 AppContainer，只有裁判桩设为 trusted ✅ |
| 大小核调度 | 识别 P 核，slot 由 worker 独占 ✅ |
| 编译 | 读取 languages.toml、30 s 超时、缓存放在题目目录、头文件内容纳入哈希、按 UTF-8 字符边界截断 ✅ |
| 测试 | `cargo test --workspace` 全部通过（含 9 个 evil 测试、8 个 sandbox 测试）✅ |

---

## 2. 必修项

### P0-1 Linux 编译失败
```
error[E0609]: no field `timing` on type `RunResult`
  --> crates/uj-sandbox/src/linux/mod.rs:65:13
     res.timing.confidence = "low".to_string();
```
- 用 `cargo check --target x86_64-unknown-linux-gnu --workspace` 复现，**Linux 下整个 workspace 都编译不过**。
- 即使修好编译：`auto` 模式仍是用 NoneSandbox 运行却标成 `Tier::Degraded`（**仍然冒充**）；`run_group` 是顺序执行 → **交互题会死锁**。
- 要求：先修好编译，并在 CI 中加入 Linux target 的 `cargo check`；没有沙盒时必须如实报告 `Tier::None`，`--require-tier` 要能拒绝；然后**在 WSL2 中**实现计划 P1（namespaces + cgroup v2 + seccomp + pidfd，内核 ≥5.19）。

### P0-2 一次性大块分配超限被判 RE，而不是 MLE
| 程序 | 结果 | 期望 |
|---|---|---|
| `mle_new`（循环 malloc 16 MB，失败时 exit 3） | **MLE** ✅（峰值 264 MB） | MLE |
| `mle_vector`（`vector` 一次性 resize 600 MB，ML 256 MB） | **RE 0xC0000409**，内存 1.7 MB ❌ | MLE |

- 原因：Job 内存上限直接拒绝这次分配 → `bad_alloc` → 快速失败；峰值从未超过上限，而 judge.rs 把「致命崩溃」放在 TLE/MLE 之前判定。
- `vector<int> a(N)` 中 N 过大是**最常见的 MLE 写法**，必须判对。
- 修复：在 IOCP 上监听 `JOB_OBJECT_MSG_JOB_MEMORY_LIMIT` / `JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT`，或在结束后查询 `JobObjectLimitViolationInformation`；只要触发过内存上限，**MLE 优先于 RE**。
- 顺带：崩溃信息写在 `checker` 字段里，属于字段误用，应放到 `message` 之类的字段。

### P1-1 性能退化 3.7–4.6 倍
500 个 A+B 测试点（全部 AC）：

| 并行度 | 第一轮 | 第二轮 | 退化 |
|---|---|---|---|
| `-j 1` | 14.4 s | **66.5 s** | ×4.6 |
| 默认 | 2.6 s | **9.7 s** | ×3.7 |

- 每个点的平均墙钟只有 20–24 ms，说明**开销在计时范围之外**：每次运行都要建 box、**复制 exe**、设置 ACL、删除目录（每点约 100 ms）。
- 刚复制出来的 exe 会被 Defender 重新扫描：evil 集中 trivial 程序墙钟约 400 ms，fork_bomb / mle 系列为 5.9–7.9 s。
- 修复：**每个二进制只准备一份**只读副本（放在 `.uj/tmp/bin/<hash>.exe`，授权一次，所有测试点复用），box 里只放可写的工作目录；或者使用硬链接。prewarm 只等 50 ms 没有实际作用，改为在副本准备好后先空跑一次。

### P1-2 Defender 重试把墙钟放宽到 10 s
- 判断「墙钟超时且 CPU < 10% TL」就重跑，并把墙钟放宽到 10 s → **任何真正阻塞/sleep 的程序每个点都要多等约 10 s**（sleep 测试墙钟 10001 ms）。
- 修复：只在**该二进制首次运行**时允许重试；修好 P1-1 之后应该基本不再需要重试。

---

## 3. 次要问题

| # | 问题 |
|---|---|
| 1 | 编译过程仍不在沙盒内（只加了超时）——恶意头文件/`#include` 读取任意文件仍有可能 |
| 2 | 编译命令中的 `{memory_mb}` / `{stack_mb}` 写死为 256，没有读取题目限制 |
| 3 | `docs/DECISIONS.md` **第 13 条（stdio MCP 零 stdout 污染）被误删**，需要恢复 |
| 4 | `-j` 大于 P 核数时会共享核心，应当提示或自动限制 |
| 5 | Linux target 下有若干 unused import / variable 警告（none.rs、linux/mod.rs） |

不算缺陷：stderr_flood → TLE、static_big → AC（属于测试设计问题）。

---

## 4. 给 Gemini 的修复顺序

1. **MLE 判定**（P0-2）：IOCP 内存上限通知 / LimitViolation，MLE 优先于 RE；把 `mle_vector` 加进 evil_tests。
2. **Linux 编译**（P0-1 前半）：修 E0609，CI 加 `cargo check --target x86_64-unknown-linux-gnu`；无沙盒时报告 `Tier::None`。
3. **exe 复用**（P1-1）：每个二进制一份副本，授权一次；验收标准：500 点 `-j 1` ≤ 20 s，默认 ≤ 4 s。
4. **重试策略**（P1-2）：只在首次运行时重试。
5. 恢复 DECISIONS 第 13 条，修 `{memory_mb}`/`{stack_mb}`。
6. **Linux 沙盒 P1**：在 WSL2 中开发和测试，evil_tests 必须在 Linux 上也全部通过。
7. 编译沙盒化。
