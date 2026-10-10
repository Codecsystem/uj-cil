# uj 沙盒设计与平台特性说明 (SANDBOX.md)

## 1. 架构总览

`uj` 针对不同操作系统提供了统一抽象的沙盒接口 `Sandbox`，但在底层实现上因地制宜：

- **Linux**：采用 Linux 原生命名空间（user, pid, mount, net, ipc, uts）+ cgroup v2 + seccomp-bpf 过滤。
- **Windows**：采用 Windows Job Object 作业对象 + AppContainer 安全隔离配置文件 + 受限安全描述符/令牌。
- **None**：无隔离降级模式（仅用于开发测试，`--sandbox none` 显式启用）。

---

## 2. Windows 沙盒要点与平台差异

### 2.1 内存口径差异 (Commit vs RSS)
- **Windows** 采用 Job Object 统计提交内存（Commit Charge，即 `PeakJobMemoryUsed`）。若程序声明大数组 `static int a[100000000]`，在 Windows 下即使未完全实际触碰页面，也会全额计入内存。
- **Linux** 采用 cgroup v2 统计实际驻留页 (`memory.peak`)。
- 此差异已在评测环境与报告中明确标记。

### 2.2 Windows Defender 干扰
- Windows Defender 可能会对新编译的二进制进行实时安全扫描，导致新可执行文件的**首次冷启动墙钟时间显著增加**。
- `uj doctor` 会探测 Defender 状态，并建议将 `.uj` 缓存与工作目录添加至排除项列表中。
- 墙钟限制针对 Windows 平台默认预留足够的安全余量，判题以 CPU 计量为准。

### 2.3 MinGW 编译器链接与栈空间
- Windows MinGW 默认栈深度通常只有 1MB 至 2MB。`uj` 在编译参数中自动附加 `-Wl,--stack,{stack_bytes}` 拓展栈空间以贴齐竞赛限制。
- 默认附加 `-static`，防止因缺少 libstdc++-6.dll 或 libgcc_s_dw2-1.dll 导致启动崩溃。

### 2.4 句柄继承泄露防范
- 多任务并行时，子进程若无序继承句柄可能导致其它任务的 stdout/stderr 管道无法正常触发 EOF，造成死锁。
- `uj` 使用 `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` 严格限制子进程仅能继承当前任务对应的输入输出 3 个句柄。

### 2.5 国际化路径
- 全面使用 Windows 宽字符 (UTF-16) API，完整无损支持带空格、中文、特殊 Unicode 字符的路径。

---

## 3. Linux 沙盒要点与当前状态
 
> [!NOTE]
> 当前阶段 Linux 原生 Full 层级（userns + cgroup v2 + seccomp 过滤）正在基于 WSL2/Linux 原生环境深度研发中。
> 在 Linux 宿主机上，如果显式指定 `--require-tier full`，系统会严格返回 `UnsupportedPlatform` 错误，禁止假冒 Full 层级；开发调试可显式使用 `--sandbox degraded` 或 `--sandbox none`。

### 3.1 内核版本硬性要求
- **内核版本必须 ≥ 5.19**（强依赖 `memory.peak`、`cgroup.kill`、`subset=pid` 及 pidfd）。

### 3.2 进程三级结构与防残留机制
- Worker 线程通过 re-exec `uj __sandbox-init` 派生单线程 helper。
- helper 创建并进入独立 namespace，fork 出 PID 1 mini-init。
- mini-init fork 出被测选手程序（PID 2）。
- mini-init 死亡时内核自动彻底回收整个 pidns 内部残留的所有派生进程；cgroup.kill 进一步提供宿主级双重保底。
