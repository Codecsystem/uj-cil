# uj 沙盒设计与平台特性说明 (SANDBOX.md)

本文档说明 `uj` 在各平台下的沙盒实现方案、安全边界与系统依赖要求。

---

## 1. 架构总览

`uj` 针对不同操作系统提供了统一的抽象接口 `Sandbox`，底层根据操作系统特性原生实现：

- Linux：采用非 root 命名空间（user, pid, mount, net, ipc, uts）+ 零 Capability 降权 + cgroup v2 + seccomp-bpf 系统调用过滤。
- Windows：采用 Windows Job Object + 槽位独占 AppContainer 安全隔离配置文件 + 受限安全描述符/令牌。
- None：无隔离直跑模式（仅用于开发调试，通过 `--sandbox none` 显式启用；在 Unix 下通过 `wait4` 采集真实 `rusage` 与崩溃信号）。

---

## 2. Windows 沙盒实现与平台特性

### 2.1 内存口径差异 (Commit vs RSS)
- Windows 采用 Job Object 统计提交内存（Commit Charge，即 `PeakJobMemoryUsed`）。若程序声明大数组 `static int a[100000000]`，在 Windows 下即使未完全实际触碰页面，也会全额计入内存。
- Linux 采用 cgroup v2 统计实际驻留物理页 (`memory.peak`)。
- 此差异在评测环境与报表中明确区分标记。

### 2.2 Windows Defender 扫描延迟与零执行预热
- Windows Defender (`WdFilter.sys`) 会对新编译生成的可执行文件在首次创建镜像节（`NtCreateUserProcess`）时进行驱动级同步安全扫描，可能导致首次冷启动墙钟时间显著增加（3~7 秒）。
- **零执行挂起预热 (`prewarm_binary`)**：在准备二进制缓存时，使用 `CREATE_SUSPENDED | CREATE_NO_WINDOW` 挂起创建进程以触发内核镜像节同步扫描，并在从不调用 `ResumeThread` 的前提下立即 `TerminateProcess` 销毁进程，确保预热阶段**零指令执行**不可信代码。
- **自适应重跑**：若检测到程序触发墙钟限制被终止，但实际消耗 CPU 时间极小（< 10% TL），系统判定为扫描阻滞，自动进行单次重跑并放宽墙钟上限（CPU 时限保持不变），保障判定准确。
- 建议运行 `uj doctor` 检查环境，并将题目目录与 `.uj` 缓存目录添加至 Defender 排除项。

### 2.3 编译器参数与栈空间扩展
- Windows MinGW 默认栈深度通常只有 1MB 至 2MB。`uj` 在编译参数中自动附加 `-Wl,--stack,{stack_bytes}`，根据 `problem.toml` 中的 `limits.stack` 设置自动扩栈，确保贴齐竞赛要求。
- 默认链接附加 `-static`，防止因缺少运行时 DLL 导致启动失败。

### 2.4 句柄继承防范与命令参数转义
- 多任务并发时，子进程若无序继承父进程句柄，会导致其它任务的 stdout/stderr 管道写端无法正常关闭，造成读取挂起。
- `uj` 通过 `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` 白名单机制，严格限制子进程仅能继承当前任务对应的输入输出句柄。
- 内部调用 `icacls.exe` 赋权/撤权时，显式使用 `CreateProcessW` 并设置 `bInheritHandles = FALSE` 与 `CREATE_NO_WINDOW`，彻底杜绝并发线程管道句柄泄漏至 `icacls.exe`。
- 命令行拼接严格遵循 MSVC CRT / `CommandLineToArgvW` 转义规范（自动加倍引号前及闭合引号前的尾部反斜杠）。

### 2.5 槽位独占 AppContainer 与最小权限目录生命周期
- 每个评测槽位（`WindowsSandbox` 实例）创建独立的 `AppContainerProfile`（`uj.sandbox.<pid>.<slot>`），严禁使用全局共享或通配 SID。
- **叶子目录最小授权**：仅对当前测试运行的叶子工作目录 `box` 授予 `(OI)(CI)F` 权限，仅对被测可执行文件授予只读执行（`RX`）权限；绝不对父级 `runs`、`slot-*` 或交互题可信目录 `trusted_int_*` 授权，彻底阻断跨槽位偷读或篡改交互器结果。
- **ACE 自动回收**：当 `WindowsSandbox` 实例销毁时，自动撤销其对残留缓存路径的专属 SID 访问控制项（ACE），再注销临时 AppContainer Profile。

---

## 3. Linux 沙盒实现与隔离机制

### 3.1 隔离层级 (Tiers) 与真实控制器校验
- **Full 层级**：启用完整的用户命名空间（`CLONE_NEWUSER`）、PID 命名空间（`CLONE_NEWPID`）、挂载命名空间（`CLONE_NEWNS`）、网络命名空间（`CLONE_NEWNET`）、IPC 命名空间（`CLONE_NEWIPC`）与 UTS 命名空间（`CLONE_NEWUTS`），挂载私有只读 rootfs 与 tmpfs，配合 cgroup v2 进行微秒级 CPU 与物理内存峰值采集，并通过 seccomp-bpf 阻断特权系统调用与 x32 ABI。
  - 初始化时严格校验 `cpu.stat`、`memory.max`、`pids.max`（以及当宿主机启用 swap 时校验 `memory.swap.max`）控制器可用性与写入成功，并启用 `cpu`、`cpuset`、`memory`、`pids` 子树控制器。
- **Degraded 层级**：当系统未开启完整 cgroup v2 控制器委派时，自动降级为 Degraded 层级（保留全部命名空间、零 Capability 与 seccomp 强隔离，辅以 POSIX `rlimit`、`sched_setaffinity` 与 `wait4` `rusage` 进行限制与采集）。若用户指定 `--require-tier full`，则明确报错终止，拒绝虚标隔离层级。
- **硬性文件写入上限 (`RLIMIT_FSIZE`)**：无论处于 Full 还是 Degraded 层级，容器内被测进程均强制设置 `RLIMIT_FSIZE = limits.output_bytes`，防止向 `/box` 写入超大文件耗尽宿主磁盘。

### 3.2 信号安全（Async-Signal-Safe）的 Pre-Exec 执行流与只读挂载
- 在多线程并发评测场景下，标准库 `Command::spawn` 的 `pre_exec` 运行在 `fork()` 之后、`execve()` 之前。为杜绝在多线程程序 fork 后调用 `malloc` 导致全局锁死锁的问题：
  - 调度器在父线程中使用 CSPRNG 随机后缀（`tempfile::Builder`）在 `/dev/shm` 创建唯一临时 rootfs 目录，并预先构造全部 `CString` 指针数组、UID/GID 映射文本与挂载对；
  - `pre_exec` 闭包内严禁任何堆内存分配与 `std::fs` 调用，严格仅使用原始 `libc::*` 系统调用执行 `unshare`、`mount`、`pivot_root` 与 `execve`。
- **只读挂载锁定 (`remount_bind_rdonly`)**：所有只读 bind mount（`/usr`、可执行文件 `/__uj_bin`、`ro_binds`、标准输入文件 `/__uj_stdin`）在重挂载为 `MS_RDONLY` 时自动通过 `statvfs` 保留宿主机挂载点的锁定标志（`MS_NODEV`、`MS_NOSUID`、`MS_NOEXEC`、`MS_RELATIME` 等），并严格校验返回值。
- **标准输入只读保护**：当非可信进程使用 `Stdio::File` 作为 `stdin` 时，不在宿主读写挂载点上直接打开文件描述符传递，而是将输入文件以只读方式 bind-mount 至容器内临时路径 `/__uj_stdin`，在 `pivot_root` 后以 `O_RDONLY` 打开为 `fd 0` 并立即卸载（`MNT_DETACH`）与删除挂载点，彻底阻断通过 `/proc/self/fd/0` 篡改或 `fchmod` 宿主测试输入文件。

### 3.3 进程三级结构、非特权 UID 与零 Capability 强化
- **非零 UID 与全 Capability 剥离**：在 `user_namespace` 中将宿主用户映射为非零普通 UID/GID；完成挂载与 `pivot_root` 后，被测进程（PID 2）在 `execve` 前清空 Ambient Capabilities、剥离全部 64 位 Bounding Set、锁定 `SECBIT_NOROOT`（`0x2f`），并通过 `capset` 将 Effective / Permitted / Inheritable 置零（`CapEff = 0`）。
- **PID 1 防篡改与状态管道校验**：
  - 容器内 PID 1（mini-init）在启动后立即调用 `prctl(PR_SET_DUMPABLE, 0)`，使 `/proc/1/{fd,mem,maps,environ}` 归属权变为 `root:root`（容器内映射为 `65534:65534`），彻底阻止被测进程（PID 2）读取宿主环境变量或向状态管道注入伪造数据包。
  - 宿主 supervisor 读取状态管道直至 EOF，仅当精确收到单个合法的 `SandboxStatusPacket`（32 字节）时才采信其退出状态与 `rusage`。
- **扩展 Seccomp-BPF 黑名单**：同时覆盖 `x86_64`（含拦截 x32 ABI `>= 0x40000000`）与 `aarch64`，阻断 45 个高危系统调用（含 `ptrace`、旧版与新版挂载 API `mount`/`umount2`/`pivot_root`/`open_tree`/`move_mount`/`fsopen`/`fsconfig`/`fsmount`/`fspick`/`mount_setattr`、`io_uring_*`、`process_vm_readv`/`writev`、`userfaultfd`、`pidfd_getfd`、`kcmp`、`open_by_handle_at`、`bpf`、`perf_event_open`、`keyctl`、`unshare`、`setns`、`clone3` 等）以及携带任意命名空间标志（含 `CLONE_NEWUTS` 在内的 `0x7e020000` 掩码）的 `clone` 调用。

### 3.4 cgroup v2 引用计数与残留清理
- 每个 `uj` 进程在 cgroup base 下建立专属的 `proc_<pid>` 子目录，并通过进程内引用计数（`Arc<CgroupBase>`）管理生命周期：当最后一个 `LinuxSandbox` 实例销毁时，自底向上使用 `rmdir` 清理空 cgroup 节点（绝不对 cgroupfs 调用递归删除文件）。
- `cleanup_stale` 自动扫描并清理已退出的历史 `uj` 进程遗留的 `proc_<dead_pid>` cgroup 树与 `/dev/shm/.uj_rootfs_<dead_pid>_*` 临时目录。
