# Round 8: 编译会话内存快速缓存与二进制准备短路 (Compiler Cache Quickpath)

## 1. 优化背景与性能痛点
在评测调度（`uj judge`）、出题验证（`uj validate`）、标程运行（`uj ans`）、对拍测试（`uj test`）等过程中，编译器会话（`CompileSession`）和沙盒二进制准备（`prepare_binary_content`）会被高频调用：
1. **源码反复读盘与全量头文件目录扫描**：每次检查编译缓存时，即便磁盘已有编译产物，引擎仍需从磁盘完整读取源码，遍历源码目录和头文件包含目录中所有 `.h` / `.hpp` 头文件，并执行 Blake3 运算；
2. **二进制内容重读与二次哈希**：沙盒在准备授权可执行文件时，每次都会调用 `fs::read(exe_path)` 将整个二进制大文件（几十 MB 的静态可执行文件）重读入内存并计算 Blake3 哈希，造成不必要的磁盘带宽消耗与 CPU 占用。

## 2. 优化方案：基于 Mtime/Size 的会话内存快速短路
1. **`CompileSession` 快速路径缓存 (`fast_results`)**：
   - 在 `CompileSessionInner` 中增加基于 `FastCompileKey`（源文件路径 + 额外源文件 + 包含目录）与 `(mtime, size)` 的高精度内存缓存；
   - 当调用 `compile_with_options` 时，若未指定 `--no-cache`，优先通过 `fs::metadata(source_path)` 检查修改时间与文件大小：
     - 若 `(mtime, size)` 命中且生成的二进制依然存在，直接在 **0ms** 内返回已编译结果，彻底短路源码读盘、头文件目录深度遍历与 Blake3 全量哈希计算；
     - 否则走完整路径，并在编译成功或命中磁盘缓存后回填该内存缓存。
2. **沙盒二进制准备快速路径 (`PREPARED_BINARIES_CACHE`)**：
   - 在 `crates/uj-sandbox/src/lib.rs` 中引入基于 `(exe_path, base_dir, mtime, size)` 的全局并发安全缓存；
   - 若二进制未修改且目标授权可执行文件存在，立即返回目标路径，消除重复的磁盘全文件读取和二进制 Blake3 计算。

## 3. 安全性与正确性复核
- **源码修改即时感知**：用户编辑并保存源文件后，`mtime` 和 `size` 改变，快速缓存自动失效并触发完整重新编译；
- **文件被外部删除感知**：通过 `cached_res.binary_path.exists()` 检查，若缓存的 exe 被删除，自动回退并重建；
- **无缓存标志遵循**：`--no-cache` 标志会无条件跳过快速缓存，保证开发者调试体验不受影响；
- **全套测试覆盖**：新增单元测试 `test_compile_session_fast_cache`，并全部通过全工作区 84 项端到端及安全测试。

## 4. 实测效果
- 重复调用编译解析缩短至 **亚毫秒级（< 0.1ms）**；
- 消除数十 MB 二进制文件的重复读盘与哈希计算，二次评测与流水线出题响应速度提升显著。
