# uj (Universal Judge)

uj 是一个轻量、现代且跨平台的算法竞赛本地评测与命题工具，使用 Rust 编写。

它主要解决算法竞赛（OI / ACM-ICPC / 校赛出题）与程序设计教学中常见的几个痛点：
- 传统评测工具架构老旧，缺乏现代操作系统的细粒度安全隔离与精准计量；
- 出题过程中，测试数据生成、校验、标程出数据、多解法反向验证和时限测定流程零散，缺乏一致的自动化管理；
- 本地脚本或简易评测器容易受到恶意/异常代码干扰（如死循环、内存耗尽、句柄泄漏、跨目录读取答案等）；
- AI 编程助手或自动化流水线在调用评测器时，容易因标准输出混入日志而解析失败。

uj 将单题评测、命题全流程管理、差分对拍、整场比赛导入评测以及 AI 智能体协议（MCP）整合为一个统一的单文件二进制程序。

---

## 核心能力

### 1. 本地高效评测
- 支持 C++、C、Python、Rust、Go 等主流竞赛语言，自动检测环境编译器。
- 支持传统题（Traditional）、交互题（Interactive）、多选手通信题（Communication）与提交答案题（Output-only）。
- 内置基于内存映射与 SIMD/SWAR 加速的高速文本比较器（exact、wcmp、lcmp、fcmp 等），同时无缝兼容 Testlib（Checker / Interactor / Validator）与 Lemon / LemonLime 传统工程。

### 2. 双平台原生沙盒与精准计量
- Linux：基于非 root 用户命名空间（user, pid, mount, net, ipc, uts）+ cgroup v2 资源控制 + seccomp-bpf 系统调用过滤。无 cgroup v2 委派时平滑降级至 Degraded 模式，保证隔离不中断。在多线程 fork 场景下保证 async-signal-safe，杜绝子进程死锁。
- Windows：基于 Job Object 作业对象限制 + AppContainer 细粒度临时目录访问控制。具备句柄泄漏防护与 Windows Defender 首次冷启动扫描自适应重跑机制，并支持物理 P 核独占绑定。
- 零虚标原则：当前环境不支持所要求的隔离层级时，明确报错或真实降级，绝不谎报隔离状态。

### 3. 一站式命题流水线
- 提供 `init` -> `build` -> `gen` -> `validate` -> `ans` -> `verify` -> `tl` 的完整出题流水线。
- 支持子任务 DAG 依赖（Subtask Dependencies）与灵活计分（Min / Sum）。
- 支持多解法行为标签验证（标程需 AC，暴力解需 TLE，错误解需 WA），防止题目数据水或标程写假。
- 内置多核并发差分对拍（`uj stress`），自动对比随机数据下不同解法的输出差异。

### 4. 机器与人类友好的双重接口
- 人类用户：提供直观的终端格式化报表与交互式 Ratatui 终端实时看板（`--tui`）。
- 外部集成：提供严格遵循规范的 JSON 与 NDJSON 实时事件流，日志与调试信息严格写入 stderr，确保 stdout 纯净。
- AI Agent 原生集成：内置标准 Model Context Protocol (MCP) 服务端，支持与 Claude Desktop、Cursor、Antigravity 等编程助手通过 stdio 协议直接联动。
- 支持将评测结果一键导出为独立离线 HTML 网页或 CSV 成绩单。

---

## 快速上手

### 1. 编译环境与安装

要求已安装 Rust 工具链（Rust 1.88+，Edition 2024）以及对应语言的编译器（如 g++ / clang++ / python）。

```bash
git clone https://github.com/Codecsystem/uj-cil.git
cd uj-cil
cargo build --release

# 编译产物位于 target/release/uj
```

建议将编译出的 `uj` 加入系统的 PATH 环境变量中。

### 2. 环境健康检查

首次运行建议执行环境诊断，检查当前系统对沙盒、编译器和内核特性的支持状态：

```bash
uj doctor
```

### 3. 评测单份代码

对单题目录下的选手代码进行评测：

```bash
# 默认终端模式评测
uj judge solution.cpp -p /path/to/problem

# 启用实时终端交互看板
uj judge solution.cpp -p /path/to/problem --tui

# 输出纯 JSON 结果（供脚本或流水线调用）
uj judge solution.cpp -p /path/to/problem --format json
```

### 4. 快速体验出题流水线

```bash
# 1. 初始化标准传统题模板
uj init my_problem --type traditional
cd my_problem

# 2. 根据需要修改 problem.toml，编写生成器 gen.cpp 与校验器 validator.cpp

# 3. 编译题内组件
uj build

# 4. 生成测试点并验证合法性
uj gen
uj validate

# 5. 使用标程生成参考答案
uj ans

# 6. 验证所有预期解法是否符合预期
uj verify

# 7. 或者，使用一条命令自动执行整套流水线
uj pipeline
```

### 5. 差分对拍

当解法遇到 WA 或找不出反例时，可以使用对拍命令：

```bash
uj stress --gen gen.cpp --sol solutions/wa.cpp --brute solutions/brute.cpp -n 1000
```

---

## 常用命令速查

| 命令 | 用途说明 |
|---|---|
| `uj judge <file>` | 评测单份源码或可执行文件 |
| `uj init <name>` | 创建题目目录骨架（支持 traditional, interactive, communication 等） |
| `uj build` | 编译当前题目下的生成器、校验器、交互器与各解法 |
| `uj gen` | 运行生成器生成测试输入数据 |
| `uj validate` | 运行校验器检查输入数据合法性 |
| `uj ans` | 使用主标程为各测试点生成标准答案文件 |
| `uj verify` | 批量运行解法并核对实际结论与声明标签是否匹配 |
| `uj tl` | 依据标程在各测试点上的运行时间给出推荐时间限制 |
| `uj pipeline` | 一键顺序执行完整出题生命周期 |
| `uj stress` | 使用数据生成器、待测解与暴力正解进行并发差分对拍 |
| `uj contest <dir>` | 评测 Lemon / LemonLime 结构的整场比赛包并汇总成绩 |
| `uj report` | 将最近一次评测记录导出为 HTML 网页报告或 CSV 表格 |
| `uj doctor` | 检查宿主机系统环境、沙盒可用性与编译器状态 |
| `uj mcp` | 启动标准 stdio Model Context Protocol (MCP) 服务端 |

---

## 配置文件示例 (`problem.toml`)

题目根目录下的 `problem.toml` 是题目的核心配置文件。以下为一个包含子任务依赖的标准传统题范例：

```toml
schema = 1
name = "aplusb"
title = "A + B Problem"
type = "traditional"

[limits]
time = "1000ms"       # CPU 时间上限
wall_time = "2000ms"  # 墙钟时间上限（防挂死）
memory = "256MB"      # 内存限制
output = "10MB"       # 最大允许输出量

[io]
mode = "stdio"        # stdio 或 file

[checker]
type = "builtin"
name = "wcmp"         # 过滤行末空格与空行的文本比较

validator = "validator.cpp"

[tests]
dir = "tests"
pattern = "auto"

[[tests.gen]]
generator = "gen.cpp"
args = "100 100"
count = 5

[[subtasks]]
id = 1
score = 40.0
tests = ["1-5"]

[[subtasks]]
id = 2
score = 60.0
tests = ["6-10"]
depends = [1]         # 依赖子任务 1，子任务 1 未 AC 时当前子任务直接跳过

[[solutions]]
path = "solutions/ac.cpp"
tag = "main"          # 主标程，要求全部通过

[[solutions]]
path = "solutions/tle.cpp"
tag = "time_limit"    # 预期超时的解法，必须触发 TLE 且不得发生 WA/RE
```

对于多选手通信题（`type = "communication"`），若选手进程数大于 1，需在根配置中显式指定拓扑结构：

```toml
type = "communication"
manager = "manager.cpp"
processes = 2
communication_topology = "ring"
```

---

## 全局常用参数

各子命令均支持以下全局选项：

| 参数 | 说明 | 默认值 |
|---|---|---|
| `-p, --problem <DIR>` | 指定包含 problem.toml 的题目根目录 | 当前目录 `.` |
| `-j, --jobs <N>` | 并发评测/编译槽位数 | 自动探测物理核心数 |
| `--sandbox <MODE>` | 沙盒隔离级别：`auto` \| `full` \| `degraded` \| `none` | `auto` |
| `--require-tier` | 严格要求 Full 沙盒，若当前环境不支持则直接报错退出 | `false` |
| `--fail-fast` | 遇到第一个未通过测试点时立即中止当前子任务后续测试 | `false` |
| `--tui` | 启用 Ratatui 交互式终端评测看板 | `false` |
| `--format <FMT>` | 报告输出格式：`human` \| `json` \| `ndjson` | 交互终端为 human，非终端为 json |
| `--no-cache` | 禁用编译与运行结果的 Blake3 增量缓存，强制重测 | `false` |

---

## 退出状态码规范

自动化脚本与集成系统可根据进程退出码快速判定执行结论：

| 退出码 | 常量定义 | 语义说明 |
|---|---|---|
| `0` | `EXIT_SUCCESS` | 评测全部 Accepted (AC)，或命令顺利完成 |
| `1` | `EXIT_NON_AC` | 评测完成，但存在非 AC 测试点（WA, TLE, MLE, RE 等） |
| `2` | `EXIT_USAGE_OR_CONFIG` | 命令行参数无效、测试用例未找到或 problem.toml 损坏 |
| `3` | `EXIT_JUDGE_FAILED` | 裁判系统错误（沙盒错误、校验器崩溃、环境异常等） |
| `4` | `EXIT_VERIFY_MISMATCH` | 解法实际评测结论与声明的预期标签不匹配 |
| `5` | `EXIT_COMPILE_ERROR` | 选手源码或评测辅助程序编译失败 |
| `130` | `EXIT_INTERRUPTED` | 用户通过 Ctrl+C (SIGINT) 主动中止 |

---

## 平台说明与最佳实践

### Windows 环境
- 内存统计口径：采用 Job Object 的 Commit Charge（提交内存）。程序静态预分配的大数组即使未完全访问，也会全额计入内存。
- Defender 扫描提示：Windows Defender 对新编译二进制的首次启动扫描可能消耗数秒。uj 内置了扫描阻滞自适应机制，但仍建议将出题目录与 `.uj` 缓存目录添加至 Defender 排除名单以获得最平稳的性能。

### Linux 环境
- 内核版本建议：建议使用 Linux 内核 >= 5.19，以获得完整的 cgroup v2 peak 内存统计、cgroup.kill 与 pidfd 支持。
- systemd cgroup 委派：普通用户建议启用 systemd user 委派（`systemd-run --user --scope`），以获得免 root 的 Full 级别沙盒体验。

---

## 关联文档

- 文档总览与分类索引：[docs/README.md](docs/README.md)
- 架构技术决策记录：[docs/specs/DECISIONS.md](docs/specs/DECISIONS.md)
- 协议与结构化交互规范：[docs/specs/PROTOCOL.md](docs/specs/PROTOCOL.md)
- 沙盒底层设计与隔离机制：[docs/specs/SANDBOX.md](docs/specs/SANDBOX.md)
- AI Agent 集成与自动化工作流：[docs/specs/AGENT_GUIDE.md](docs/specs/AGENT_GUIDE.md)
- Agent 核心代码维护指南：[AGENTS.md](AGENTS.md)

---

## 许可证

本项目采用 [GNU General Public License v3.0 (GPL-3.0)](LICENSE) 开源许可证授权。
