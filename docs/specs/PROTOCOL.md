# uj 协议与结构化交互说明 (PROTOCOL.md)

本文档规范 `uj` 针对 AI Agent 与外部自动化调用方的交互协议与输出格式。

---

## 1. 通信原则

1. **Stdout 纯净性**：
   - 在 `--format json` 模式下，`stdout` 保证且仅输出一个合法的顶层 JSON 结果对象。
   - 在 `--format ndjson` 模式下，`stdout` 实时按行输出单个 JSON 事件对象。
   - 日志、进度条、调试信息一律输出至 `stderr`。
2. **Schema 版本化**：
   - 所有输出结果均携带 `"schema_version": 1` 字段。
   - 可通过 `uj schema [config|result|events]` 查看或导出当前版本的完整 JSON Schema。
3. **确定性退出码**：
   - `0`：成功通过（Judge 全部 AC / Verify 全部符合预期 / Stress 未检出分歧）。
   - `1`：存在非 AC 结论 / 对拍发现差异。
   - `2`：命令行配置或参数用法错误。
   - `3`：评测系统故障（JudgeFailed：Checker/Validator 崩溃、沙盒故障、进程残留）。
   - `4`：Verify 结论与解法期望标签不符。
   - `5`：编译错误 (Compile Error)。
   - `130`：外部信号中断 (SIGINT / Ctrl-C)。

---

## 2. 结构化错误响应格式

当命令执行出错且使用 `--format json` 或 `--format ndjson` 模式时，`stdout` 输出如下标准错误对象（`schema_version: 1`）：

```json
{
  "schema_version": 1,
  "ok": false,
  "error": {
    "kind": "ConfigError",
    "message": "failed to parse problem.toml: invalid limit '1x'",
    "hint": "Limit string should be in format '<num>ms' or '<num>s'",
    "path": "problem.toml",
    "line": 15,
    "column": 8
  }
}
```

---

## 3. 测试点评测结果结构 (Single Test Result)

```json
{
  "id": "7",
  "subtask": 2,
  "verdict": "WA",
  "score": 0.0,
  "time_ms": 132,
  "wall_ms": 140,
  "memory_kb": 10240,
  "exit": {
    "code": 0
  },
  "checker": "wrong answer 3rd numbers differ - expected: '5', found: '6'",
  "diff": {
    "line": 1,
    "column": 5,
    "token_index": 3,
    "expected": "... 4 5 7 ...",
    "found": "... 4 6 7 ...",
    "truncated": false
  },
  "timing": {
    "rejudged": false,
    "confidence": "high"
  }
}
```

---

## 4. NDJSON 事件流 (NDJSON Events)

在评测执行期间，如果开启 `--format ndjson`，stdout 将按行实时推送 JSON 事件。典型事件序列如下：

1. `compile_start` / `compile_done`: 编译阶段通知。
2. `test_start` / `test_done`: 单个测试点评测开始与结束，payload 为测试点结果。
3. `subtask_done`: 子任务所有测试点完成后的结算事件：
   ```json
   {
     "event": "subtask_done",
     "data": {
       "id": 1,
       "verdict": "AC",
       "score": 40.0,
       "tests": ["1", "2"]
     }
   }
   ```
4. `submission_done`: 评测整体完成事件，payload 为完整的评测报告对象。
5. `summary`: 最终汇总事件，位于流末尾，汇报真实最终结论与消耗：
   ```json
   {
     "event": "summary",
     "data": {
       "total_time_ms": 132,
       "peak_memory_kb": 10240,
       "final_verdict": "AC",
       "final_score": 100.0
     }
   }
   ```
6. `warning`: 运行时警告信息（如评测重试或环境提示）。

---

## 5. 子任务 DAG 依赖与评分聚合规则 (Subtask Scoring & Aggregation)

### 5.1 聚合优先级 (Verdict Precedence)
当多个测试点或子任务出现不同结论时，判题结果按以下非递减严苛度合并：
1. **SystemError (SE) / JudgeFailed (JF)**：最高优先级。基础设施、沙盒故障或缺少答案绝不可被部分分（PC）或普通错误覆盖。
2. **CompileError (CE)**：编译失败，无法执行后续测试。
3. **RuntimeError (RE) / RestrictedFunction (RF) / MemoryLimitExceeded (MLE) / TimeLimitExceeded (TLE) / OutputLimitExceeded (OLE)**：运行时致命异常。
4. **WrongAnswer (WA)**：答案不匹配。
5. **PartiallyCorrect (PC)**：部分分判定。
6. **Accepted (AC)**：仅当所有有效测试均 AC 且无任何前置异常时成立。

> **核心原则**：若存在 SystemError 或 JudgeFailed，最终评测 Verdict 必须呈现失败状态，即使部分子任务已获得分数。

### 5.2 子任务依赖门控 (Subtask Dependency Gating)
- 子任务可声明 `depends = [subtask_id_1, ...]`。
- 依赖判定按子任务拓扑排序严格执行（声明顺序无差别不变性）。
- **门控规则**：若被依赖的子任务结论非 `Accepted`（或未达标），依赖该子任务的所有测试点将被标记为 `Skipped`，得分强制计 `0.0`。

### 5.3 计分模式 (Scoring Models)

| 模式 | 计算公式 | 适用场景 | 示例 |
|---|---|---|---|
| **Group (Min)** | $Score = MaxScore \times \min_{t \in Subtask} (\frac{score_t}{max\_score_t})$ | OI/ICPC 传统子任务（全对得满分，错一题得 0 分） | 子任务 40 分，含 2 测试点，分别得 100% 和 0% -> 子任务得 0 分 |
| **Sum** | $Score = \sum_{t \in Subtask} score_t$ | 宽松梯度得分题 / 比赛总分累加 | 子任务 1 (40 分) + 子任务 2 (60 分) -> 100 分 |

---

## 6. MCP Server 规范与并发取消模型 (MCP Protocol)

`uj mcp` 实现 Model Context Protocol（支持 `2024-11-05`、`2025-03-26`、`2025-06-18` 协议版本协商），采用 Stdio 传输：
1. **纯净标准输出**：标准输入输出只流转合法 JSON-RPC 2.0 数据，禁止向 stdout 输出任何日志或 ANSI 控制码。
2. **协议级 JSON-RPC 2.0 校验与错误码**：
   - `-32700 (Parse error)`：非合法 JSON 载荷。
   - `-32600 (Invalid Request)`：非 JSON 对象、空批处理数组 `[]`、`jsonrpc != "2.0"`、缺失或非字符串 `method`、非法 `id` 类型（非 string/number/null）或非法 `params` 类型。
   - `-32601 (Method not found)`：未知 RPC 方法。
   - `-32602 (Invalid params)`：调用未知工具名称、缺失必填工具参数（如 `judge` 缺少 `submission`/`sources`、`stress` 缺少 `generator`/`solution`/`brute`、`contest`/`init_problem` 缺少 `dir`）或参数类型/枚举值非法（包括非法的 `run_id` 路径穿越）。
   - **Notification 语义**：省略 `"id"` 字段的请求严格视为 JSON-RPC Notification，服务端绝不向其发送响应；显式携带 `"id": null` 的请求按标准返回 `"id": null` 响应。
   - **运行时业务错误**：当工具参数合法但在编译、发现用例或评测执行中发生业务错误时，返回 `result: { "isError": true, "content": [...] }`。
3. **非阻塞并发与 `requestId` 精准取消**：
   - 当耗时 `tools/call` 在后台工作线程执行时，`ping`、`tools/list`、`initialize` 等轻量请求立即在线响应，后续 `tools/call` 自动进入有序队列。
   - 客户端发送 `$/cancelRequest` 或 `notifications/cancelled` 时，服务端按 `params.requestId` 精准匹配当前执行中或队列中的任务并通过 `CancelToken` 触发沙盒与子进程树安全回收。
