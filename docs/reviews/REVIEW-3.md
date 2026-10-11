# uj 第三轮验收报告

> 范围：commit `4538af4`（feat(sandbox): implement linux cgroup v2 & namespace sandbox …）
> 环境：Win11，24C/32T（8P+16E），Defender 开启；**WSL2 Ubuntu 26.04，内核 6.6.87，systemd，cgroup v2**
> 方法：Windows 上 `cargo build/test` + 恶意程序集 + 500 点性能测试；**Linux 上在 WSL2 中实际编译、运行恶意程序集、逃逸探针和样例题**

## 结论

| 平台 | 结论 |
|---|---|
| **Windows** | ✅ **第二轮必修项全部修复**；剩下 1 个 P0（`--require-tier` 假 AC）和 1 个 P1（预热在 Job 外运行选手代码） |
| **Linux** | ❌ **不通过**：资源限制和判定已经能用，但**文件系统隔离基本没有**（可以写 `$HOME` → 等同于逃逸），auto 模式**随机**退化成无沙盒 |

---

## 1. Windows：第二轮问题复核

| 第二轮问题 | 现在 | 证据 |
|---|---|---|
| P0-2 vector 一次性分配 600 MB 被判 RE | ✅ **MLE** | IOCP `JOB_OBJECT_MSG_*_MEMORY_LIMIT` + LimitViolation |
| P1-1 性能退化 ×4 | ✅ **已修复，比第一轮还快** | 见下表 |
| P1-2 每个点都多等 10 s | ✅ 每个二进制只重试一次 | sleep：只有首个超时点是 10001 ms |
| P0-1 Linux 编译失败 | ✅ 编译通过；CI 加了 linux target check | WSL 中 `cargo build --release` 通过 |
| 崩溃信息写在 `checker` 字段 | ✅ 改到 `message` | |
| DECISIONS 第 13 条 | ✅ 已恢复 | |
| `cargo test --workspace` | ✅ 全部通过（含新增 `test_evil_mle`） | |

**性能（500 个 A+B 测试点，全部 AC）：**

| 并行度 | 第一轮 | 第二轮 | **第三轮** |
|---|---|---|---|
| `-j 1` | 14.4 s | 66.5 s | **9.6 s** |
| `-j 4` | 3.2 s | — | **3.6 s** |
| 默认 | 2.6 s | 9.7 s | **2.55 s** |

**恶意程序集（全新目录）：** ac AC、mle_vector/mle_new/mle_touch **MLE**、sleep TLE、cheat_read WA（读不到答案）、cheat_write 没能写入、abort/栈溢出/segv RE、loop TLE、ole OLE、CE 后继续评测；run 目录无残留，进程无残留。

---

## 2. 必修项

### P0-1 `--require-tier` 不满足时输出**假 AC**（两个平台都有）
```
uj judge --sandbox none --require-tier ac.cpp
→ {"verdict":"AC","score":0.0,"tests":[]}   退出码 0
```
- `--sandbox degraded --require-tier` 也一样。没有跑任何测试点却报 **AC + 退出码 0**，Agent 会把它当成通过。
- 要求：tier 不满足时直接报错退出（非 0 退出码，verdict 设为 `JudgeFailed` 或者不输出评测结果）；**`tests` 为空时绝不能给 AC**（judge 汇总逻辑里加断言）。

### P0-2 Linux 文件系统隔离缺失（等同于逃逸）
逃逸探针 [lx_all.cpp](../tests/evil_seed/lx_all.cpp) 在 auto（Full）模式下的结果：

| 探测项 | 结果 |
|---|---|
| 直接读 `<题目>/tests/1.ans` | ✅ 被阻止（tmpfs 遮盖） |
| 经 `/proc/<ppid>/root/...` 读答案 | ✅ 被阻止 |
| **写 `<题目>/PWNED_lx.txt`** | ❌ **成功**（可以篡改 problem.toml、checker、标程） |
| **写 `/home/lenovo/uj_pwned_lx.txt`** | ❌ **成功** → 可以改 `~/.bashrc`，等于**完全逃逸** |
| 写 `/tmp` | ❌ 成功 |
| **读 `~/.bash_history`**（可推及 `~/.ssh`） | ❌ **成功** |
| `/proc` 中可见的进程数 | ❌ 110（与宿主机相同；pid ns 无效） |

原因（[runner.rs](file:///e:/Data/Work/uj-cil/crates/uj-sandbox/src/linux/runner.rs#L130-L197)）：
- 只用 tmpfs 遮盖了 `tests/` 这一个目录，**宿主机的整个文件系统都可读写**（userns 中 root 映射回真实 uid，宿主机上的权限全部有效）。
- 在 `pre_exec` 里 `unshare(CLONE_NEWPID)` **对调用者自身不生效**（只作用于它的子进程），而且没有重新挂载 `/proc`。
- **没有 seccomp**（模块文档里写了，但没有实现）。
- 所有 `mount()` 的返回值都被忽略：挂载失败时**静默失效**，结果仍然标成 Full。

要求（按计划第 4 节）：
- 新建 mount ns，**pivot_root 进最小 rootfs**：`/usr /lib /lib64 /bin` 只读 bind，box 目录可写，`/tmp` 用小 tmpfs，`/proc` 新挂载（`subset=pid`），其余一律不可见。
- 用 `clone3(CLONE_INTO_CGROUP | CLONE_NEWPID | …)` 或「先 fork 中间 init 再 exec」，让选手程序真正进入新的 pid ns。
- 加 seccomp 黑名单（ptrace、mount、bpf、keyctl、perf_event_open、unshare/setns 等）。
- 任何隔离步骤失败 → 返回错误，**不能静默继续**。
- 把 lx_all.cpp 加进 evil_tests（Linux 下 C/E/G 必须全是 0，F ≤ 3）。

### P0-3 Linux auto 模式**随机**退化为无沙盒
- 同一台机器、同一条命令：**10 次运行中有 3 次**打印 `WARNING: Linux Full sandbox unavailable (No writable cgroup v2 directory found); falling back to None tier`，而同一进程里其他测试点却成功创建了 cgroup。
- 推测原因：[find_base_dir](file:///e:/Data/Work/uj-cil/crates/uj-sandbox/src/linux/cgroup.rs#L60-L67) 先 `get` 再 `set`，不是原子操作，多线程会同时探测；各线程都用 `.uj_test_<pid>` 作为探测目录名（同一进程的 pid 相同），`create_dir` 撞上 EEXIST 就误判为不可写。
- 后果：部分提交**没有任何隔离**地运行，退出码仍为 0，而且报告里**看不到 tier**。
- 要求：用 `OnceLock::get_or_try_init`（或 Mutex）保证只探测一次；探测目录名加线程 id 或随机数；**JSON 报告的每个测试点都要带 `sandbox_tier`**。

### P1-1 预热（prewarm）在 Job 外运行选手代码（Windows）
- [prewarm_binary](file:///e:/Data/Work/uj-cil/crates/uj-sandbox/src/windows/process.rs#L974) 现在是在 AppContainer 中**真正 Resume** 选手程序，运行最多 100 ms，**没有挂到 Job 上**（没有进程数、内存、CPU 限制，也没有 KILL_ON_JOB_CLOSE）。TerminateProcess 只杀主进程，它创建的子进程会逃逸。
- 本机实测没有复现：spawn_one 探针没有留下残留进程，大概是 Defender 扫描超过 100 ms，程序还没跑到 main 就被杀了。但在关闭 Defender 或加了排除项的机器上，fork_bomb 会变成**不受限制的真实 fork 炸弹**。
- 要求：预热**必须**在一个 Job 里运行（ActiveProcessLimit=1 + KILL_ON_JOB_CLOSE + 内存上限），或者恢复成「挂起创建 → 等待 → 终止」，不执行任何用户代码。

### P1-2 cgroup 目录选择过于激进（Linux）
- 一次运行会在 `user@1000.service/uj`、`app.slice/uj`、`app.slice/run-*.scope/uj` **三处**创建目录；`enable_subtree_controllers` 还会把父 cgroup 中的进程迁移到 `runner/` 子组。这会改动 systemd 管理的 cgroup 树，超出了 uj 被委派的范围。
- 要求：只在**自己所在的 scope 内**操作（或者用 `systemd-run --user --scope -p Delegate=yes` 重新执行自身），不要向上走去改父组；退出时删除 `uj` 目录。

---

## 3. 次要问题

| # | 问题 |
|---|---|
| 1 | Linux `Tier::Degraded` 仍然只是 NoneSandbox 换了个标签（实测 degraded 模式能读到答案 A=3）。要么实现「只有 cgroup + rlimit」的真降级，要么在 Linux 上去掉 degraded |
| 2 | Windows：所有并发 run 共用一个 AppContainer SID，`runs/` 目录整体授权 → 同一时刻的不同 run（比赛模式下不同选手）可以互相读写对方的 box |
| 3 | 编译过程仍不在沙盒内 |
| 4 | Defender 首次扫描仍算进墙钟（MLE 系列 wall 约 5.3 s，CPU 时间正确，判定不受影响），可接受 |

---

## 4. 给 Gemini 的修复顺序

1. **`--require-tier` 假 AC**（P0-1）：tier 不满足时报错退出；空 tests 不能判 AC；补测试。
2. **find_base_dir 竞态 + 报告带 tier**（P0-3）。
3. **Linux 文件系统隔离**（P0-2）：pivot_root 最小 rootfs、真正的 pid ns + 新 `/proc`、seccomp、失败即报错；lx_all 加进 evil_tests，**在 WSL2 里跑通**。
4. **预热放进 Job**（P1-1）。
5. **cgroup 只在自己的 scope 内操作**（P1-2）。
6. 次要问题 1–3。
