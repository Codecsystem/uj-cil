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
