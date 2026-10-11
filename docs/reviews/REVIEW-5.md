# uj 第五轮（快速）验收报告

> 范围：commit `d9792e4`（fix(review-4): address P0/P1 security vulnerabilities and minor issues）
> 方法：只复核第四轮的必修项 + 回归检查（Windows：`cargo test` + win_probe；WSL2：lx_all / lx_seccomp / 信号 / 无 user bus）

## 结论：✅ 通过（剩 2 个次要问题，不阻塞）

| 第四轮问题 | 结果 | 证据 |
|---|---|---|
| P0-1 无 systemd user bus 时 uj 不能用 | ✅ | 无 bus 时：`--version` 正常；`judge` 打印警告，按 `tier=none` 跑完，退出码 0 |
| P1-1 seccomp 可以通过 x32 绕过 | ✅ | `x32mount=-1/38`（ENOSYS），`unshare`/`ptrace` 返回 EPERM；clone3 返回 ENOSYS，clone 带 NS 标志返回 EPERM |
| P1-2 Windows 选手程序不能写自己的 box | ✅ | win_probe：`A=-1,C=0,E=0,G=0,T=0,`**`K=1`**；改为每个 slot 一个 `runs/slot-N/` 目录、只授权一次；注册表中没有残留的 slot profile |
| 选手程序是 PID 1 | ✅ | lx_all：`pid=2,ppid=1,F=2`；`abort` 现在是 134（SIGABRT），`raise(SIGTERM)` 能正常终止（143） |
| `cargo test --workspace`（Windows） | ✅ | 全部通过（evil 测试 13 个，pipeline 测试 20 个） |
| Linux 隔离回归 | ✅ | lx_all 中读答案、写题目目录/`$HOME`、读私有文件全部被拦 |
| 残留 | ✅ | 没有残留的 cgroup 和 run 目录 |

## 剩余次要问题（下个迭代处理即可）

| # | 问题 | 建议 |
|---|---|---|
| 1 | Linux 信号仍然被报告成 `exit 128+N`（`Process exited with return code 139`）。原因：PID 1 给自己发信号会被内核忽略，最后只能走 `_exit(128+sig)` 兜底 | 让 PID 1 通过管道或退出码约定把「被信号 N 杀死」传给外层，由外层进程解码成 `ExitStatus::Signaled(N)`；或者在 judge 层把 `>128` 的退出码映射成信号名（有歧义，不推荐） |
| 2 | 没有 user bus 时直接降到 **None**（完全不隔离） | namespace、pivot_root、seccomp 都不依赖 cgroup。没有 cgroup 时可以保留这些隔离，再用 `RLIMIT_AS`/`RLIMIT_CPU`/`RLIMIT_NPROC` 代替资源限制，作为真正的 Degraded，比 None 安全得多 |
| 3 | 每次 `judge` 都会先跑一次 `systemd-run --user --scope true` 探测；无 bus 时它的报错会原样输出到 stderr | 已经在委派 scope 内时跳过探测；探测命令的 stderr 重定向到 null |

**顺带确认：** `[io] mode = "file"` 本轮已经实现（freopen `aplusb.in/out` → AC，已在 Windows 实测）。**仍未处理：** 编译过程不在沙盒内。
