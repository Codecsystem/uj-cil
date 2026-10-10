# Round 7: 标准输出管道 64KB 环形缓冲与预分配流式读取 (Stdout Pipe Buffering)

## 1. 优化背景与性能痛点
在评测子进程（选手程序、标程、交互器、通信管理器）执行时，沙盒通过 Windows 匿名管道（Anonymous Pipe）捕获子进程的 `stdout` 和 `stderr`：
1. **默认管道缓冲区狭窄 (4KB)**：先前的管道创建代码使用 `CreatePipe(&mut read, &mut write, None, 0)`。向系统传入 `0` 会让 Windows 使用系统默认的最低管道缓冲（通常仅 4KB）。一旦子进程输出超过 4KB，子进程的写操作立即发生内核阻塞，触发操作系统强制将子进程挂起，并将父进程 reader 线程唤醒换入；reader 读出后子进程再被唤醒换入。在批量测试用例中造成了剧烈的线程上下文切换（Context Switch）；
2. **读取端零容量反复 Realloc**：管道读取循环 `read_pipe_to_end` 初始 `buf = Vec::new()`，从 0 字节开始反复进行 `0 -> 4 -> 8 -> 16 ...` 的动态数组扩容与数据拷贝；
3. **单次读取块过小**：先前的读缓冲区仅为 8KB，在 10MB 等大数据量输出下需执行 1280+ 次 `ReadFile` 阻塞系统调用。

## 2. 优化方案：内核 64KB 缓冲提升与读取预分配
1. **提升管道内核建议缓冲区至 64KB**：
   ```rust
   CreatePipe(&mut read_pipe, &mut write_pipe, None, 65536)?;
   ```
   绝大多数算法题的输出在几十 KB 以内。64KB 的内核缓冲区使得子进程无需发生线程调度阻塞，即可一次性将输出写入内核管道缓冲区并高速结束进程。
2. **预分配根据 Output Limit 设上限**：
   ```rust
   let mut buf = Vec::with_capacity(limit.min(65536));
   ```
   消除小块输出时的连续 `realloc`，直接在一次性预分配缓冲区内完成数据接收。
3. **块大小扩容至 64KB**：
   单次系统调用读取吞吐提升 8 倍，减少高达 87.5% 的内核态到用户态切换。
4. **跨平台兼顾 (`none.rs`)**：
   在通用兼容后端 `none.rs` 中同步升级 64KB 块读取与 `stdout_limit.min(65536)` 预分配。

## 3. 安全性与 OLE (Output Limit Exceeded) 复核
- **OLE 截断严格生效**：保留原有的 `ole_flag` 机制与 `limit.saturating_sub(buf.len())` 严格边界截断；
- **防内存炸裂**：预分配上限被 `limit.min(65536)` 限制，绝不会因为 limit 极大而盲目申请过多虚拟内存；
- **测试通过**：经 `test_evil_ole` 与 `test_windows_sandbox_output_limit` 实测，OLE 判定与截断字节数 100% 精确。

## 4. 实测效果
- 消除子进程输出时的上下文切换颠簸；
- 批量用例评测更加平稳顺畅，完全兼容所有沙盒模式。
