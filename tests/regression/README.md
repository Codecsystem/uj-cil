# uj Regression Test Suite

This directory contains regression test fixtures and specifications for defects identified in the 2026-10-11 system audit.

## Test ID Matrix

| Test ID | Area | Scenario | Expected Behavior |
|---|---|---|---|
| `REG-INT-001` | P0-01 | Contestant process attempts to create/overwrite/delete interaction result XML/transcript | Verdict remains original interactor verdict (e.g. WA); contestant cannot alter judge score or verdict |
| `REG-WIN-001` | P0-02 | Contestant parent process exits while child keeps stdout/stderr pipe handles open | Execution terminates within wall limit; no orphaned processes; status is Timeout or handled error |
| `REG-ART-001` | P0-03 | Missing answer file, legitimate empty answer, or unreadable answer | Missing/unreadable answer yields Configuration/JudgeFailed error, never silent AC |
| `REG-ART-002` | P0-03 | File I/O output missing, unauthorized stdout fallback, or output exceeds limit | Does not falsely award AC; correctly reports WA/OLE/JudgeFailed |
| `REG-SCORE-001` | P0-04 | Subtask DAG defined in different TOML declaration orders | Score and subtask evaluation order are topologically stable and invariant to order |
| `REG-SCORE-002` | P0-04 | Mixed AC + SE/TLE/JudgeFailed in subtasks | System errors and execution failures are preserved and never masked by partial credit (PC) |
| `REG-CACHE-001` | P0-05 | Modification to indirect included headers, modules, or language config | Forces recompilation; never reuses stale binary |
| `REG-CACHE-002` | P0-05 | Corrupted binary cache or concurrent compile sessions | Recovers cleanly; atomic cache publishing; no partial binary execution |
| `REG-CHECK-001` | P1-05 | UTF-8 multi-byte long tokens and NaN/Infinity checker scores | No panic on byte slicing; invalid scores yield JudgeFailed |
| `REG-AUTH-001` | P1-07 | `uj init` -> `gen` twice -> `validate` -> `ans` -> `pipeline --format json` | Generator is idempotent without `--append`; pipeline produces single valid JSON root object |
| `REG-STRESS-001` | P1-08 | Generator, target solution, and brute-force solution all exit non-zero | Stress testing reports execution failure with diagnostic log, not false success |
| `REG-COMM-001` | P1-01 | Multi-participant communication problem with manager scoring | Tier and role statuses are reported accurately; score flows through private channel |
| `REG-MCP-001` | P1-09 | Empty problem, missing generator, cancellation during task | Clean JSON-RPC error responses; cancellation honoured; no false ok=true |
| `REG-REPORT-001` | P1-16 | Malicious/special characters in test IDs, duplicate test IDs, CE without tests | Properly escaped HTML without script injection; well-formed CSV and persistent storage |
