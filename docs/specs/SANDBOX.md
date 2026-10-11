# uj 沙盒设计与平台特性说明 (SANDBOX.md)

本文档说明 `uj` 在各平台下的沙盒实现方案、安全边界与系统依赖要求。

---

## 1. 架构总览

`uj` 针对不同操作系统提供了统一的抽象接口 `Sandbox`，底层根据操作系统特性原生实现：

- Linux：采用非 root 命名空间（user, pid, mount, net, ipc, uts）+ cgroup v2 + seccomp-bpf 系统调用过滤。
- Windows：采用 Windows Job Object + AppContainer 安全隔离配置文件 + 受限安全描述符/令牌。
- None：无隔离直跑模式（仅用于开发调试，通过 `--sandbox none` 显式启用）。

---

## 2. Windows 沙盒实现与平台特性

### 2.1 内存口径差异 (Commit vs RSS)
- Windows 采用 Job Object 统计提交内存（Commit Charge，即 `PeakJobMemoryUsed`）。若程序声明大数组 `static int a[100000000]`，在 Windows 下即使未完全实际触碰页面，也会全额计入内存。
- Linux 采用 cgroup v2 统计实际驻留物理页 (`memory.peak`)。
- 此差异在评测环境与报表中明确区分标记。

### 2.2 Windows Defender 扫描延迟与自适应重跑
- Windows Defender 会对新编译生成的可执行文件进行驱动级实时安全扫描，可能导致新可执行文件的首次冷启动墙钟时间显著增加（3~7 秒）。
- `uj` 内置了 Defender 扫描自适应机制：若检测到程序触发墙钟限制被终止，但实际消耗 CPU 时间极小（< 10% TL），系统会判定为扫描阻滞，自动进行单次重跑并放宽墙钟上限（CPU 时限保持不变），保障判定准确。
- 建议运行 `uj doctor` 检查环境，并将题目目录与 `.uj` 缓存目录添加至 Defender 排除项。

### 2.3 编译器参数与栈空间扩展
- Windows MinGW 默认栈深度通常只有 1MB 至 2MB。`uj` 在编译参数中自动附加 `-Wl,--stack,{stack_bytes}`，根据 `problem.toml` 中的 `limits.stack` 设置自动扩栈，确保贴齐竞赛要求。
- 默认链接附加 `-static`，防止因缺少运行时 DLL 导致启动失败。

### 2.4 句柄继承防范
- 多任务并发时，子进程若无序继承父进程句柄，会导致其它任务的 stdout/stderr 管道写端无法正常关闭，造成读取挂起。
- `uj` 通过 `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` 白名单机制，严格限制子进程仅能继承当前任务对应的输入输出 3 个句柄。

### 2.5 路径与目录隔离
- 每次运行分配独立的临时工作目录，仅为当前任务对应的 AppContainer SID 赋予访问权限，禁止使用全局通配 SID，杜绝不同测试点与选手之间跨目录偷读或篡改数据。
- 路径处理全面基于 Windows 宽字符 (UTF-16) API，完整支持带空格、中文及特殊字符的路径。

---

## 3. Linux 沙盒实现与隔离机制

### 3.1 隔离层级 (Tiers)
- Full 层级：启用完整的用户命名空间（`CLONE_NEWUSER`）、PID 命名空间（`CLONE_NEWPID`）、挂载命名空间（`CLONE_NEWNS`）、网络命名空间（`CLONE_NEWNET`）、IPC 命名空间（`CLONE_NEWIPC`）与 UTS 命名空间（`CLONE_NEWUTS`），挂载私有只读 rootfs 与 tmpfs，配合 cgroup v2 进行微秒级 CPU 与物理内存峰值采集，并通过 seccomp-bpf 阻断特权系统调用与 x32 ABI。
- Degraded 层级：当系统未开启 systemd cgroup v2 委派或非 cgroup v2 环境时，自动降级为 Degraded 层级（保留全部命名空间与 seccomp 强隔离，辅以 POSIX rlimits 与 getrusage 进行软性限制）。若用户指定 `--require-tier full`，则明确报错终止，拒绝假冒隔离。
- 内核版本要求：建议内核版本 >= 5.19（完整支持 `memory.peak`、`cgroup.kill` 与 pidfd）。

### 3.2 信号安全（Async-Signal-Safe）的 Pre-Exec 执行流
- 在多线程并发评测场景下，标准库 `Command::spawn` 的 `pre_exec` 运行在 `fork()` 之后、`execve()` 之前。为杜绝在多线程程序 fork 后调用 `malloc` 导致全局锁死锁的问题：
  - 调度器在父线程中预先构造全部 CString 指针数组、rootfs 目录骨架、UID/GID 映射文本与挂载对；
  - `pre_exec` 闭包内严禁任何堆内存分配与 `std::fs` 调用，严格仅使用原始 `libc::*` 系统调用执行 unshare、tmpfs mount、bind mount 与 execve。

### 3.3 进程三级结构与防残留机制
- Worker 线程在命名空间中派生 PID 1 mini-init 进程。
- mini-init 进程 fork 出被测选手程序（PID 2），并负责监控与回收。
- 选手程序崩溃退出时，PID 1 通过匿名状态管道将原始 `wait4` 状态与 rusage 结构体回传给宿主进程，确保真实退出状态与崩溃信号准确上报。
- 评测结束时，PID 1 退出触发内核回收整个 PID 命名空间内的全部残留后代进程，结合 `cgroup.kill` 提供双重防残留保障。

### 3.4 cgroup 多实例隔离
- 针对并发启动多个 `uj` 实例（例如同时运行 MCP 评测与本地命令行操作）的场景，每个实例在 cgroup base 下建立专属的 `proc_<pid>` 子目录，生命周期结束时仅释放自身子 cgroup，互不干扰。
