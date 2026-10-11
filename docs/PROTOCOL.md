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

当命令执行出错且使用 JSON 模式时，stdout/stderr 输出如下标准错误对象：

```json
{
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

在评测执行期间，如果开启 `--format ndjson`，将依次推送以下事件：

- `compile_start`
- `compile_done`
- `test_start`
- `test_done`
- `subtask_done`
- `submission_done`
- `warning`
- `summary`

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
- 子任务可声明 `dependencies = [subtask_id_1, ...]`。
- 依赖判定按子任务拓扑排序严格执行（声明顺序无差别不变性）。
- **门控规则**：若被依赖的子任务结论非 `Accepted`（或未达标），依赖该子任务的所有测试点将被标记为 `Skipped`，得分强制计 `0.0`。

### 5.3 计分模式 (Scoring Models)

| 模式 | 计算公式 | 适用场景 | 示例 |
|---|---|---|---|
| **Group (Min)** | $Score = MaxScore \times \min_{t \in Subtask} (\frac{score_t}{max\_score_t})$ | OI/ICPC 传统子任务（全对得满分，错一题得 0 分） | 子任务 40 分，含 2 测试点，分别得 100% 和 0% -> 子任务得 0 分 |
| **Sum** | $Score = \sum_{t \in Subtask} score_t$ | 宽松梯度得分题 / 比赛总分累加 | 子任务 1 (40 分) + 子任务 2 (60 分) -> 100 分 |

---

## 6. MCP Server 规范与并发取消模型 (MCP Protocol)

`uj mcp` 实现 Model Context Protocol 2024-11-05 规范，采用 Stdio 传输：
1. **纯净标准输出**：标准输入输出只流转合法 JSON-RPC 2.0 数据，禁止向 stdout 输出任何日志或 ANSI 控制码。
2. **并发取消机制**：
   - 客户端发送 `$/cancelRequest` 或 `notifications/cancelled` 时，服务端通过共享 `CancelToken` 触发正在执行的评测或出题进程组安全回收与中断。
   - 被取消任务立即终止沙盒与子进程树，并返回包含 `{"isError": true, "content": [{"text": "Cancelled"}]}` 的受控响应。
3. **Fail-Closed 故障防护**：
   - 对任何非法的 JSON-RPC 协议版本（如 `jsonrpc: "1.0"`），立即返回标准错误码 `-32600 (Invalid Request)`。
   - 目标题目不存在或配置破损时，直接返回 `isError: true`，绝不静默报告假成功。
