//! Self-contained HTML report generation using minijinja.
//!
//! Provides a dark-themed, responsive, fully offline single-file report with
//! expandable test case details, diff visualization, and optional contest leaderboard.

use crate::result::SubmissionJudgeReport;
use minijinja::{Environment, context};
use serde::Serialize;

const REPORT_TEMPLATE: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>uj Judge Report - {{ title }}</title>
  <style>
    :root {
      --bg-main: #09090b;
      --bg-card: #141417;
      --bg-card-hover: #1c1c21;
      --border-color: #27272a;
      --text-main: #f4f4f5;
      --text-muted: #a1a1aa;
      --text-dim: #71717a;
      
      --color-ac: #10b981;
      --bg-ac: rgba(16, 185, 129, 0.15);
      --border-ac: #059669;

      --color-wa: #ef4444;
      --bg-wa: rgba(239, 68, 68, 0.15);
      --border-wa: #dc2626;

      --color-tle: #f59e0b;
      --bg-tle: rgba(245, 158, 11, 0.15);
      --border-tle: #d97706;

      --color-mle: #a855f7;
      --bg-mle: rgba(168, 85, 247, 0.15);
      --border-mle: #9333ea;

      --color-re: #ec4899;
      --bg-re: rgba(236, 72, 153, 0.15);
      --border-re: #db2777;

      --color-ole: #eab308;
      --bg-ole: rgba(234, 179, 8, 0.15);
      --border-ole: #ca8a04;

      --color-rf: #f43f5e;
      --bg-rf: rgba(244, 63, 94, 0.15);
      --border-rf: #e11d48;

      --color-ce: #fbbf24;
      --bg-ce: rgba(251, 191, 36, 0.15);
      --border-ce: #d97706;

      --color-se: #ff3333;
      --bg-se: #000000;
      --border-se: #ef4444;

      --color-skipped: #71717a;
      --bg-skipped: rgba(113, 113, 122, 0.15);
      --border-skipped: #3f3f46;
    }

    * {
      box-sizing: border-box;
      margin: 0;
      padding: 0;
    }

    body {
      font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
      background-color: var(--bg-main);
      color: var(--text-main);
      line-height: 1.5;
      padding: 2rem 1.5rem;
    }

    .container {
      max-width: 1200px;
      margin: 0 auto;
    }

    header {
      display: flex;
      justify-content: space-between;
      align-items: center;
      margin-bottom: 2rem;
      border-bottom: 1px solid var(--border-color);
      padding-bottom: 1.25rem;
    }

    .logo-group {
      display: flex;
      align-items: center;
      gap: 0.75rem;
    }

    .logo-badge {
      background: linear-gradient(135deg, #3b82f6, #8b5cf6);
      color: white;
      font-weight: 800;
      font-size: 1.25rem;
      padding: 0.25rem 0.75rem;
      border-radius: 6px;
      letter-spacing: 0.05em;
    }

    .app-title {
      font-size: 1.5rem;
      font-weight: 700;
    }

    .meta-info {
      color: var(--text-muted);
      font-size: 0.875rem;
    }

    /* Cards */
    .card {
      background-color: var(--bg-card);
      border: 1px solid var(--border-color);
      border-radius: 8px;
      padding: 1.5rem;
      margin-bottom: 1.5rem;
    }

    .summary-grid {
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
      gap: 1rem;
      margin-bottom: 1.5rem;
    }

    .stat-box {
      background-color: var(--bg-card);
      border: 1px solid var(--border-color);
      border-radius: 8px;
      padding: 1.25rem;
    }

    .stat-label {
      font-size: 0.75rem;
      text-transform: uppercase;
      letter-spacing: 0.05em;
      color: var(--text-muted);
      margin-bottom: 0.35rem;
    }

    .stat-value {
      font-size: 1.5rem;
      font-weight: 700;
    }

    /* Verdict Badges */
    .badge {
      display: inline-flex;
      align-items: center;
      padding: 0.25rem 0.65rem;
      border-radius: 9999px;
      font-size: 0.8rem;
      font-weight: 700;
      letter-spacing: 0.03em;
      text-transform: uppercase;
    }

    .badge-lg {
      font-size: 1.25rem;
      padding: 0.35rem 1.25rem;
    }

    .badge-ac { background-color: var(--bg-ac); color: var(--color-ac); border: 1px solid var(--border-ac); }
    .badge-wa { background-color: var(--bg-wa); color: var(--color-wa); border: 1px solid var(--border-wa); }
    .badge-tle { background-color: var(--bg-tle); color: var(--color-tle); border: 1px solid var(--border-tle); }
    .badge-mle { background-color: var(--bg-mle); color: var(--color-mle); border: 1px solid var(--border-mle); }
    .badge-re { background-color: var(--bg-re); color: var(--color-re); border: 1px solid var(--border-re); }
    .badge-ole { background-color: var(--bg-ole); color: var(--color-ole); border: 1px solid var(--border-ole); }
    .badge-rf { background-color: var(--bg-rf); color: var(--color-rf); border: 1px solid var(--border-rf); }
    .badge-ce { background-color: var(--bg-ce); color: var(--color-ce); border: 1px solid var(--border-ce); }
    .badge-se { background-color: var(--bg-se); color: var(--color-se); border: 1px solid var(--border-se); }
    .badge-skipped { background-color: var(--bg-skipped); color: var(--color-skipped); border: 1px solid var(--border-skipped); }

    /* Tables */
    table {
      width: 100%;
      border-collapse: collapse;
      text-align: left;
      font-size: 0.9rem;
    }

    th {
      background-color: rgba(255, 255, 255, 0.02);
      color: var(--text-muted);
      font-weight: 600;
      padding: 0.75rem 1rem;
      border-bottom: 1px solid var(--border-color);
    }

    td {
      padding: 0.75rem 1rem;
      border-bottom: 1px solid var(--border-color);
    }

    tr.clickable-row {
      cursor: pointer;
      transition: background-color 0.15s ease;
    }

    tr.clickable-row:hover {
      background-color: var(--bg-card-hover);
    }

    .detail-row {
      display: none;
      background-color: rgba(0, 0, 0, 0.25);
    }

    .detail-row.expanded {
      display: table-row;
    }

    .detail-box {
      padding: 1.25rem;
      font-family: ui-monospace, "SFMono-Regular", Consolas, "Liberation Mono", Menlo, monospace;
      font-size: 0.85rem;
    }

    .code-block {
      background-color: #0c0c0e;
      border: 1px solid var(--border-color);
      border-radius: 6px;
      padding: 0.75rem 1rem;
      overflow-x: auto;
      margin-top: 0.5rem;
      white-space: pre-wrap;
      word-break: break-all;
    }

    .diff-block {
      display: grid;
      grid-template-columns: 1fr 1fr;
      gap: 1rem;
      margin-top: 0.5rem;
    }

    .diff-col {
      background-color: #0c0c0e;
      border: 1px solid var(--border-color);
      border-radius: 6px;
      padding: 0.75rem 1rem;
      overflow-x: auto;
      font-family: ui-monospace, Consolas, monospace;
    }

    .diff-col.exp { border-left: 3px solid var(--color-ac); }
    .diff-col.got { border-left: 3px solid var(--color-wa); }

    /* Collapsible Subtasks */
    .subtask-group {
      border: 1px solid var(--border-color);
      border-radius: 8px;
      margin-bottom: 1rem;
      overflow: hidden;
    }

    .subtask-header {
      background-color: var(--bg-card);
      padding: 1rem 1.25rem;
      display: flex;
      justify-content: space-between;
      align-items: center;
      cursor: pointer;
      user-select: none;
    }

    .subtask-header:hover {
      background-color: var(--bg-card-hover);
    }

    .subtask-content {
      border-top: 1px solid var(--border-color);
    }

    .chevron {
      transition: transform 0.2s ease;
      display: inline-block;
      margin-right: 0.5rem;
    }

    .collapsed .chevron {
      transform: rotate(-90deg);
    }

    .collapsed + .subtask-content {
      display: none;
    }

    /* Leaderboard */
    .leaderboard-table th {
      font-size: 0.8rem;
    }

    .rank-1 { color: #ffd700; font-weight: 700; }
    .rank-2 { color: #c0c0c0; font-weight: 700; }
    .rank-3 { color: #cd7f32; font-weight: 700; }
  </style>
</head>
<body>
  <div class="container">
    <header>
      <div class="logo-group">
        <span class="logo-badge">uj</span>
        <span class="app-title">Judging Report</span>
      </div>
      <div class="meta-info">
        Generated: {{ generated_at }} | uj v{{ uj_version }}
      </div>
    </header>

    {% if is_contest %}
    <!-- Contest Leaderboard -->
    <div class="card">
      <h2 style="margin-bottom: 1rem;">Contest Standings</h2>
      <table class="leaderboard-table">
        <thead>
          <tr>
            <th style="width: 60px;">Rank</th>
            <th>Participant</th>
            <th style="width: 100px;">Total Score</th>
            <th style="width: 120px;">Max Time</th>
            <th style="width: 100px;">Verdict</th>
          </tr>
        </thead>
        <tbody>
          {% for r in reports %}
          <tr>
            <td class="rank-{{ loop.index }}">{{ loop.index }}</td>
            <td><strong>{{ r.submission }}</strong></td>
            <td><strong style="color: var(--color-ac);">{{ r.score | round(1) }}</strong></td>
            <td>{{ r.max_time_ms }} ms</td>
            <td>
              <span class="badge {{ badge_class(r.verdict) }}">{{ r.verdict }}</span>
            </td>
          </tr>
          {% endfor %}
        </tbody>
      </table>
    </div>
    {% endif %}

    {% for r in reports %}
    <div class="report-section" id="report-{{ loop.index }}">
      <div class="summary-grid">
        <div class="stat-box">
          <div class="stat-label">Problem / Submission</div>
          <div class="stat-value" style="font-size: 1.25rem;">{{ r.problem }}</div>
          <div style="font-size: 0.875rem; color: var(--text-muted); margin-top: 0.25rem;">{{ r.submission }}</div>
        </div>

        <div class="stat-box">
          <div class="stat-label">Verdict</div>
          <div>
            <span class="badge badge-lg {{ badge_class(r.verdict) }}">{{ r.verdict }}</span>
          </div>
        </div>

        <div class="stat-box">
          <div class="stat-label">Final Score</div>
          <div class="stat-value">{{ r.score | round(1) }}</div>
        </div>

        <div class="stat-box">
          <div class="stat-label">Peak Resources</div>
          <div class="stat-value" style="font-size: 1.25rem;">{{ r.max_time_ms }} ms</div>
          <div style="font-size: 0.875rem; color: var(--text-muted); margin-top: 0.25rem;">
            {% if r.max_memory_kb > 1024 %}
              {{ (r.max_memory_kb / 1024.0) | round(2) }} MiB
            {% else %}
              {{ r.max_memory_kb }} KiB
            {% endif %}
          </div>
        </div>
      </div>

      <!-- Subtasks / Tests -->
      {% if r.subtasks and r.subtasks | length > 0 %}
        {% for st in r.subtasks %}
        <div class="subtask-group">
          <div class="subtask-header" onclick="toggleSubtask(this)">
            <div>
              <span class="chevron">▼</span>
              <strong>Subtask {{ st.id }}</strong>
              <span style="color: var(--text-muted); margin-left: 0.5rem;">({{ st.test_ids | length }} tests)</span>
            </div>
            <div style="display: flex; gap: 1rem; align-items: center;">
              <span><strong>{{ st.score | round(1) }}</strong> / {{ st.max_score | round(1) }} pts</span>
              <span class="badge {{ badge_class(st.verdict) }}">{{ st.verdict }}</span>
            </div>
          </div>
          <div class="subtask-content">
            <table>
              <thead>
                <tr>
                  <th style="width: 50px;">#</th>
                  <th>Test Case</th>
                  <th style="width: 100px;">Verdict</th>
                  <th style="width: 90px;">Time</th>
                  <th style="width: 100px;">Memory</th>
                  <th style="width: 80px;">Score</th>
                  <th>Details</th>
                </tr>
              </thead>
              <tbody>
                {% for t in r.tests %}
                  {% if t.subtask == st.id %}
                    {{ render_test_row(t, loop.index) }}
                  {% endif %}
                {% endfor %}
              </tbody>
            </table>
          </div>
        </div>
        {% endfor %}
      {% else %}
        <!-- Flat tests list -->
        <div class="card" style="padding: 0; overflow: hidden;">
          <table>
            <thead>
              <tr>
                <th style="width: 50px;">#</th>
                <th>Test Case</th>
                <th style="width: 100px;">Verdict</th>
                <th style="width: 90px;">Time</th>
                <th style="width: 100px;">Memory</th>
                <th style="width: 80px;">Score</th>
                <th>Details</th>
              </tr>
            </thead>
            <tbody>
              {% for t in r.tests %}
                {{ render_test_row(t, loop.index) }}
              {% endfor %}
            </tbody>
          </table>
        </div>
      {% endif %}
    </div>
    {% endfor %}
  </div>

  <script>
    function toggleDetail(rowId) {
      const el = document.getElementById(rowId);
      if (el) {
        el.classList.toggle('expanded');
      }
    }

    function toggleSubtask(header) {
      header.classList.toggle('collapsed');
    }
  </script>
</body>
</html>
"#;

#[derive(Serialize)]
struct ReportContext<'a> {
    title: &'a str,
    generated_at: String,
    uj_version: &'a str,
    is_contest: bool,
    reports: &'a [SubmissionJudgeReport],
}

fn map_verdict_badge(v: &str) -> &'static str {
    match v {
        "AC" | "Accepted" => "badge-ac",
        "WA" | "WrongAnswer" => "badge-wa",
        "TLE" | "TimeLimitExceeded" => "badge-tle",
        "MLE" | "MemoryLimitExceeded" => "badge-mle",
        "RE" | "RuntimeError" => "badge-re",
        "OLE" | "OutputLimitExceeded" => "badge-ole",
        "RF" | "RestrictedFunction" => "badge-rf",
        "CE" | "CompileError" => "badge-ce",
        "SE" | "SystemError" | "JudgeFailed" => "badge-se",
        "Skipped" => "badge-skipped",
        _ => "badge-skipped",
    }
}

/// Generates a standalone HTML report for a single submission.
pub fn generate_html(report: &SubmissionJudgeReport) -> Result<String, String> {
    generate_multi_html(std::slice::from_ref(report), false)
}

/// Generates a standalone HTML report for one or more submissions (e.g., contest mode).
pub fn generate_multi_html(
    reports: &[SubmissionJudgeReport],
    is_contest: bool,
) -> Result<String, String> {
    let mut env = Environment::new();

    // Helper functions for template
    env.add_function("badge_class", |verdict: String| -> String {
        map_verdict_badge(&verdict).to_string()
    });

    // Custom test row macro renderer
    env.add_function(
        "render_test_row",
        |test: minijinja::Value, idx: usize| -> minijinja::Value {
            let id = test.get_attr("id").unwrap_or_default().to_string();
            let verdict = test.get_attr("verdict").unwrap_or_default().to_string();
            let time_ms = test
                .get_attr("time_ms")
                .ok()
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let wall_ms = test
                .get_attr("wall_ms")
                .ok()
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let memory_kb = test
                .get_attr("memory_kb")
                .ok()
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let score = test
                .get_attr("score")
                .map(|v| format!("{:.1}", v))
                .unwrap_or_else(|_| "0.0".to_string());

            let badge_cls = map_verdict_badge(&verdict);
            let row_detail_id = format!("test-detail-{idx}-{id}");

            let mem_str = if memory_kb > 1024 {
                format!("{:.2} MiB", memory_kb as f64 / 1024.0)
            } else {
                format!("{memory_kb} KiB")
            };

            // Details and diff
            let checker_val = test
                .get_attr("checker")
                .ok()
                .filter(|v| !v.is_undefined() && !v.is_none())
                .map(|v| v.to_string());
            let diff_val = test.get_attr("diff").ok().filter(|v| !v.is_undefined());

            let mut details_html = String::new();
            if let Some(ref chk) = checker_val {
                details_html.push_str(&format!(
                    "<div><strong>Checker Output:</strong><div class=\"code-block\">{}</div></div>",
                    html_escape(chk)
                ));
            }

            if let Some(diff) = diff_val
                && let (Ok(line), Ok(col), Ok(exp), Ok(got)) = (
                    diff.get_attr("line"),
                    diff.get_attr("column"),
                    diff.get_attr("expected"),
                    diff.get_attr("found"),
                )
            {
                details_html.push_str(&format!(
                    "<div style=\"margin-top: 0.5rem;\"><strong>Mismatch at Line {}, Column {}:</strong></div>",
                    line, col
                ));
                details_html.push_str("<div class=\"diff-block\">");
                details_html.push_str(&format!(
                    "<div class=\"diff-col exp\"><strong>Expected:</strong><pre>{}</pre></div>",
                    html_escape(&exp.to_string())
                ));
                details_html.push_str(&format!(
                    "<div class=\"diff-col got\"><strong>Found:</strong><pre>{}</pre></div>",
                    html_escape(&got.to_string())
                ));
                details_html.push_str("</div>");
            }

            let timing_info = test.get_attr("timing").ok();
            let mut timing_html = String::new();
            if let Some(t) = timing_info {
                let rejudged = t
                    .get_attr("rejudged")
                    .ok()
                    .and_then(|v| v.is_true().then_some(true))
                    .unwrap_or(false);
                let confidence = t
                    .get_attr("confidence")
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                if rejudged {
                    timing_html.push_str(&format!(
                        "<div style=\"color: var(--text-muted); font-size: 0.8rem; margin-top: 0.25rem;\">Rejudged (Confidence: {confidence})</div>"
                    ));
                }
            }

            let summary_note = if let Some(ref chk) = checker_val {
                chk.lines().next().unwrap_or("").to_string()
            } else {
                String::new()
            };

            let row_html = format!(
                r#"<tr class="clickable-row" onclick="toggleDetail('{row_detail_id}')">
                    <td>{idx}</td>
                    <td><strong>{id}</strong></td>
                    <td><span class="badge {badge_cls}">{verdict}</span></td>
                    <td>{time_ms} ms</td>
                    <td>{mem_str}</td>
                    <td>{score}</td>
                    <td style="color: var(--text-muted); font-size: 0.85rem;">{summary_note}</td>
                </tr>
                <tr id="{row_detail_id}" class="detail-row">
                    <td colspan="7">
                        <div class="detail-box">
                            <div style="display: flex; gap: 2rem; margin-bottom: 0.75rem;">
                                <div><strong>CPU Time:</strong> {time_ms} ms</div>
                                <div><strong>Wall Time:</strong> {wall_ms} ms</div>
                                <div><strong>Peak Memory:</strong> {mem_str}</div>
                            </div>
                            {timing_html}
                            {details_html}
                        </div>
                    </td>
                </tr>"#,
                summary_note = html_escape(&summary_note)
            );

            minijinja::Value::from_safe_string(row_html)
        },
    );

    let template_name = "report.html";
    env.add_template(template_name, REPORT_TEMPLATE)
        .map_err(|e| format!("Failed to register report template: {e}"))?;

    let tmpl = env
        .get_template(template_name)
        .map_err(|e| format!("Failed to get report template: {e}"))?;

    let title = if reports.len() == 1 {
        format!("{}: {}", reports[0].problem, reports[0].submission)
    } else {
        "Contest Report".to_string()
    };

    let ctx = ReportContext {
        title: &title,
        generated_at: "2026-10-10 16:30".to_string(),
        uj_version: env!("CARGO_PKG_VERSION"),
        is_contest,
        reports,
    };

    tmpl.render(context! {
        title => ctx.title,
        generated_at => ctx.generated_at,
        uj_version => ctx.uj_version,
        is_contest => ctx.is_contest,
        reports => ctx.reports,
    })
    .map_err(|e| format!("Failed to render HTML report: {e}"))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use uj_core::verdict::Verdict;

    #[test]
    fn test_generate_html() {
        let report = SubmissionJudgeReport {
            schema_version: 1,
            problem: "aplusb".to_string(),
            submission: "ac.cpp".to_string(),
            verdict: Verdict::Accepted,
            score: 100.0,
            max_time_ms: 12,
            max_memory_kb: 1024,
            subtasks: vec![],
            tests: vec![crate::result::SingleTestResult {
                id: "1".to_string(),
                subtask: None,
                verdict: Verdict::Accepted,
                score: 100.0,
                time_ms: 5,
                wall_ms: 6,
                memory_kb: 512,
                exit: Default::default(),
                checker: Some("ok answer matches".to_string()),
                diff: None,
                timing: Default::default(),
            }],
        };

        let html = generate_html(&report).expect("HTML generation failed");
        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("aplusb: ac.cpp"));
        assert!(html.contains("ok answer matches"));
        assert!(html.contains("badge-ac"));
    }
}
