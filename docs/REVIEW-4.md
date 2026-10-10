# uj 第四轮验收报告

> 范围：commit `29314bb`（fix(review-3): resolve review 3 issues, harden linux isolation, and enforce tier requirements）
> 环境：Win11 24C/32T + Defender；WSL2 Ubuntu 26.04，内核 6.6，systemd，cgroup v2
> 方法：Windows 上 `cargo build/test` + 恶意程序集 + 新的 Windows 逃逸探针 + 性能测试；WSL2 上编译 + 恶意程序集 + 升级版 lx_all 探针 + seccomp 探针 + 性能测试 + 无 user bus 场景

## 结论

| 平台 | 结论 |
|---|---|
| **Linux** | ⚠️ **隔离已经达标**（逃逸探针全部被拦，性能很好）。剩 1 个 P0：**没有 systemd user bus 时 uj 完全不能用**；另有 1 个 P1：seccomp 可以通过 x32 绕过 |
| **Windows** | ⚠️ require-tier 已修好；但**本轮新引入 1 个 P1 回归：选手程序不能写自己的 box 目录** |

距离可以发布只剩下面第 2 节列的几项。

---

## 1. 第三轮问题复核

| 第三轮问题 | 现在 | 证据 |
|---|---|---|
| P0-1 `--require-tier` 假 AC | ✅ 退出码 3，`verdict: "SE"` 并附 `judge_failed_error` | 两个平台都实测过 |
| P0-2 Linux 文件系统隔离 | ✅ **全部被拦** | 见下表 |
| P0-3 auto 随机退化 | ✅ Mutex 缓存 + 唯一探测名；报告带 `sandbox_tier` | 每个测试点都输出 `tier=full` |
| P1-1 预热不在 Job 里 | ✅ 预热放进 Job（进程数 1 + 内存上限）并按 Job 终止 | 代码审查 |
| P1-2 cgroup 越界 | ✅ 只操作自己所在的 scope；不在委派 scope 时自动 re-exec 到 `systemd-run --user --scope` | 运行结束后不残留 `uj`/`runner` cgroup |
| `cargo test --workspace` | ✅ 全部通过 | |

**Linux 逃逸探针**（[lx_all.cpp](../tests/evil_seed/lx_all.cpp)，增加了硬编码宿主机路径的探测项）：

| 探测项 | 第三轮 | 第四轮 |
|---|---|---|
| 读答案（相对路径 / `/proc/ppid/root` / 宿主机绝对路径） | -1 / -1 / — | **-1 / -1 / -1** ✅ |
| 写题目目录 / `$HOME` | 1 / 1 | **0 / 0** ✅ |
| 读 `~/.bash_history` / `/etc/passwd` | 1 / — | **0 / 0** ✅ |
| 写 `/tmp` | 宿主机的 /tmp | 私有 tmpfs（宿主机上看不到）✅ |
| `/proc` 可见进程数 | 110 | **1**（pid=1）✅ |
| seccomp：`unshare` / `ptrace` | 未实现 | **EPERM** ✅ |

**Linux 恶意程序集**：MLE×3、TLE、OLE、RE、fork 炸弹被进程数上限拦住、孤儿进程/fork 炸弹无残留、网络被阻断、sleep 墙钟 2 s 判 TLE；run 目录和 cgroup 都无残留。

**性能（500 个 A+B 测试点）：**

| | `-j 1` | 默认 |
|---|---|---|
| Linux（WSL2） | **1.26 s** | **0.54 s** |
| Windows | 9.5 s | 3.2 s（第三轮 2.55 s，见次要问题 4） |

---

## 2. 必修项

### P0-1 没有 systemd user bus 时 uj 直接退出、没有任何输出（Linux）
```
$ env -u DBUS_SESSION_BUS_ADDRESS XDG_RUNTIME_DIR=/nonexistent uj judge ...
Failed to connect to user scope bus via local transport: No such file or directory
exit=1   （stdout 为空；连 `uj --version` 也一样）
```
- [main.rs:66-82](file:///e:/Data/Work/uj-cil/crates/uj-cli/src/main.rs#L66-L82)：只要 `systemd-run` 能启动，就直接用它的退出码退出，**不检查它是否真的成功拉起了 uj**。
- 受影响的场景：**GitHub Actions / 大多数 CI**、没开 linger 的 SSH 会话、Docker 容器、`sudo -u`、WSL 中没有用户会话的情况。项目自己的 Linux CI 跑 evil_tests 时大概率也会挂。
- 修复：
  - 先探测：`systemctl --user is-system-running` 或 `systemd-run --user --scope --quiet true` 成功，才 re-exec；
  - re-exec 失败（systemd-run 在 exec uj 之前就返回非 0）→ 在当前进程继续，auto 模式降到 None/Degraded 并打印警告（`--require-tier` 时报错退出）；
  - 只对需要沙盒的子命令（judge/contest/stress/run/pipeline…）re-exec；`--version`、`schema`、`doctor`、`mcp` 不应依赖 systemd；
  - 加一个测试：清空 `DBUS_SESSION_BUS_ADDRESS`/`XDG_RUNTIME_DIR` 后 `uj judge --sandbox none` 必须能正常出结果。

### P1-1 seccomp 可以通过 x32 ABI 绕过（Linux）
```
syscall(SYS_mount | 0x40000000, 0,0,0,0,0)  →  -1 / EFAULT(14)   ← 系统调用真的执行了，没有被过滤
```
- 当前 WSL 内核开启了 x32。[seccomp.rs](file:///e:/Data/Work/uj-cil/crates/uj-sandbox/src/linux/seccomp.rs#L111-L120) 黑名单只做精确匹配，`nr | __X32_SYSCALL_BIT` 可以绕过所有条目（unshare/setns/keyctl/bpf…）。
- 修复：加载 nr 之后先加一条 `if nr >= 0x40000000 → KILL/ERRNO`。另外建议把 `clone`/`clone3` 带 `CLONE_NEWUSER` 等 namespace 标志的情况也拦掉（`clone3` 的参数在结构体里，无法检查 → 直接返回 ENOSYS，glibc 会回退到 clone）。
- 把 [lx_seccomp.cpp](../tests/evil_seed/lx_seccomp.cpp) 探针加进 evil_tests（三项都必须是 EPERM 或 ENOSYS）。

### P1-2 选手程序不能写自己的 box 目录（Windows，本轮回归）
- Windows 逃逸探针 [win_probe.cpp](../tests/evil_seed/win_probe.cpp)：`A=-1,C=0,E=0,G=0,T=0,`**`K=0`**——读答案、写题目目录、写用户目录、写 Temp 都被拦，这是对的；但**往自己的工作目录写文件也失败了**。
- 原因：本轮改成每个 slot 用 `create_isolated()` 创建独立的 AppContainer，但 `runs/` 目录（[lib.rs:193](file:///e:/Data/Work/uj-cil/crates/uj-sandbox/src/lib.rs#L193)）和预热（[process.rs:975](file:///e:/Data/Work/uj-cil/crates/uj-sandbox/src/windows/process.rs#L975)）仍然只授权给共享的 `uj.sandbox` SID。
- 影响：需要写临时文件的程序会判错；以后实现文件 IO 时会全部失败。
- 修复：在创建 box 时授权给**本 slot 的 SID**（每个 box 单独授权，或者给每个 slot 一个独立的 `runs/slot-N/` 目录、只授权一次）；预热也使用本 slot 的 profile。把 win_probe 加进 evil_tests，断言 `K=1`。
- 注意：上一轮的 cheat_read 测试之所以还能通过，是因为 `runs/` 多了一层目录，导致它的相对路径失效了，并不能证明隔离有效。evil_tests 应改用 win_probe 这种从 cwd 推出题目目录的探针。

---

## 3. 次要问题

| # | 问题 |
|---|---|
| 1 | **选手程序在 Linux 上是 PID 1**：内核对 init 进程会忽略没有处理函数的信号 → `abort()` 被报告成 exit 139（SIGSEGV）而不是 SIGABRT；`raise(SIGTERM)` 等自杀方式无效。建议让中间进程留在新 ns 里当 PID 1（做 reaper），选手程序作为 PID 2 |
| 2 | Linux 信号被编码成 `exit 128+N`，报告里显示 `Process exited with return code 139` 而不是 `signal 11`。中间进程应该用同一个信号杀死自己（`signal(sig, SIG_DFL); kill(getpid(), sig)`），或者由父进程解码 |
| 3 | `aggregate_submission_report` 里的 `assert!(!test_results.is_empty())`：所有 worker 初始化沙盒失败时（没加 `--require-tier`）会 **panic**，应改为返回 JudgeFailed |
| 4 | Windows 每个 worker 都要创建并删除一个 AppContainer profile：默认并行度下 500 点从 2.55 s 变成 3.2 s；进程被强杀时 profile 会残留在注册表里（`uj.sandbox.<pid>.<n>`），`cleanup_stale` 应该负责清理 |
| 5 | **`[io] mode = "file"` 完全没有实现**（engine 中没有任何地方引用 `input_name`/`output_name`），而 LemonLime 导入会产生文件 IO 题 → 用 freopen 的程序会全部判错。修好 P1-2 之后应补上这一功能 |
| 6 | Linux：bind mount 时一并传 `MS_RDONLY` 不会生效（需要再 remount 一次），exe、`/dev/*`、ro_binds 实际上是可写挂载；目前影响有限（exe 运行中有 ETXTBSY 保护），建议修正 |
| 7 | 编译过程仍不在沙盒内 |

---

## 4. 给 Gemini 的修复顺序

1. **systemd-run 失败回退**（P0-1）+ 无 bus 场景的测试。
2. **seccomp 拦截 x32**（P1-1）+ sc.cpp 测试。
3. **Windows box 授权给 slot SID**（P1-2）+ win_probe 测试（`K=1`，其余为 0/-1）。
4. 次要问题 1–3（PID 1 / 信号 / panic）。
5. 实现文件 IO 模式（次要问题 5）。
6. 次要问题 4、6、7。
