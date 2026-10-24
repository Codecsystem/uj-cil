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
          {% for s in standings %}
          <tr>
            <td class="rank-{{ s.rank }}">{{ s.rank }}</td>
            <td><strong>{{ s.participant }}</strong></td>
            <td><strong style="color: var(--color-ac);">{{ s.total_score | round(1) }}</strong></td>
            <td>{{ s.total_time_ms }} ms</td>
            <td>
              <span class="badge {{ badge_class(s.verdict) }}">{{ s.verdict }}</span>
            </td>
          </tr>
          {% endfor %}
        </tbody>
      </table>
    </div>
    {% endif %}

    {% for r in reports %}
    {% set report_idx = loop.index %}
    <div class="report-section" id="report-{{ report_idx }}">
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
                    {{ render_test_row(t, loop.index, report_idx) }}
                  {% endif %}
                {% endfor %}
              </tbody>
            </table>
          </div>
        </div>
        {% endfor %}
        {% set extra_tests = unassigned_tests(r.tests, r.subtasks) %}
        {% if extra_tests | length > 0 %}
        <div class="subtask-group">
          <div class="subtask-header" onclick="toggleSubtask(this)">
            <div>
              <span class="chevron">▼</span>
              <strong>Other Tests</strong>
              <span style="color: var(--text-muted); margin-left: 0.5rem;">({{ extra_tests | length }} tests)</span>
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
                {% for item in extra_tests %}
                  {{ render_test_row(item.test, item.index, report_idx) }}
                {% endfor %}
              </tbody>
            </table>
          </div>
        </div>
        {% endif %}
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
                {{ render_test_row(t, loop.index, report_idx) }}
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

/// A ranked row in the contest standings leaderboard.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContestStandingRow {
    pub rank: usize,
    pub participant: String,
    pub total_score: f64,
    pub total_time_ms: u64,
    pub verdict: uj_core::verdict::Verdict,
}

/// Computes ranked contest standings by aggregating per-submission reports when explicit standings
/// are not supplied by the caller.
pub fn compute_standings_from_reports(
    reports: &[SubmissionJudgeReport],
) -> Vec<ContestStandingRow> {
    use std::collections::BTreeMap;
    use uj_core::verdict::Verdict;

    let mut by_participant: BTreeMap<String, (f64, u64, Option<Verdict>)> = BTreeMap::new();
    for r in reports {
        let suffix = format!(" - {}", r.problem);
        let participant = r
            .submission
            .strip_suffix(&suffix)
            .unwrap_or(&r.submission)
            .to_string();
        let entry = by_participant.entry(participant).or_insert((0.0, 0, None));
        entry.0 += r.score;
        entry.1 += r.max_time_ms;
        entry.2 = Some(match entry.2 {
            Some(prev) => prev.merge(r.verdict),
            None => r.verdict,
        });
    }

    let mut rows: Vec<ContestStandingRow> = by_participant
        .into_iter()
        .map(
            |(participant, (total_score, total_time_ms, verdict))| ContestStandingRow {
                rank: 1,
                participant,
                total_score,
                total_time_ms,
                verdict: verdict.unwrap_or(Verdict::Skipped),
            },
        )
        .collect();

    rows.sort_by(|a, b| {
        b.total_score
            .partial_cmp(&a.total_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.total_time_ms.cmp(&b.total_time_ms))
            .then_with(|| a.participant.cmp(&b.participant))
    });

    let mut current_rank = 1;
    for i in 0..rows.len() {
        if i > 0 {
            let prev = &rows[i - 1];
            let curr = &rows[i];
            let same_score = (curr.total_score - prev.total_score).abs() < 1e-6;
            let same_time = curr.total_time_ms == prev.total_time_ms;
            if !(same_score && same_time) {
                current_rank = i + 1;
            }
        }
        rows[i].rank = current_rank;
    }

    rows
}

#[derive(Serialize)]
struct ReportContext<'a> {
    title: &'a str,
    generated_at: String,
    uj_version: &'a str,
    is_contest: bool,
    reports: &'a [SubmissionJudgeReport],
    standings: &'a [ContestStandingRow],
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
    if is_contest {
        let standings = compute_standings_from_reports(reports);
        generate_contest_html(reports, &standings)
    } else {
        render_html_impl(reports, false, &[])
    }
}

/// Generates a standalone HTML contest report using an explicit ranked standings leaderboard.
pub fn generate_contest_html(
    reports: &[SubmissionJudgeReport],
    standings: &[ContestStandingRow],
) -> Result<String, String> {
    render_html_impl(reports, true, standings)
}

fn render_html_impl(
    reports: &[SubmissionJudgeReport],
    is_contest: bool,
    standings: &[ContestStandingRow],
) -> Result<String, String> {
    let mut env = Environment::new();

    // Helper functions for template
    env.add_function("badge_class", |verdict: String| -> String {
        map_verdict_badge(&verdict).to_string()
    });

    env.add_function(
        "unassigned_tests",
        |tests: minijinja::Value, subtasks: minijinja::Value| -> Vec<minijinja::Value> {
            let mut known_ids = std::collections::HashSet::new();
            if let Ok(iter) = subtasks.try_iter() {
                for st in iter {
                    if let Ok(id_val) = st.get_attr("id")
                        && let Some(id) = id_val.as_i64()
                    {
                        known_ids.insert(id);
                    }
                }
            }
            let mut out = Vec::new();
            if let Ok(iter) = tests.try_iter() {
                for (i, t) in iter.enumerate() {
                    let matched = t
                        .get_attr("subtask")
                        .ok()
                        .filter(|v| !v.is_undefined() && !v.is_none())
                        .and_then(|v| v.as_i64())
                        .is_some_and(|st_id| known_ids.contains(&st_id));
                    if !matched {
                        out.push(context! {
                            index => i + 1,
                            test => t,
                        });
                    }
                }
            }
            out
        },
    );

    // Custom test row macro renderer
    env.add_function(
        "render_test_row",
        |test: minijinja::Value,
         idx: usize,
         report_idx: Option<usize>|
         -> minijinja::Value {
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
                .ok()
                .and_then(|v| f64::try_from(v).ok())
                .map(|s| format!("{s:.1}"))
                .unwrap_or_else(|| "0.0".to_string());

            let badge_cls = map_verdict_badge(&verdict);
            let r_idx = report_idx.unwrap_or(1);
            let row_detail_id = format!("test-detail-row-{r_idx}-{idx}");
            let escaped_id = html_escape(&id);
            let escaped_verdict = html_escape(&verdict);

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
            let diff_val = test
                .get_attr("diff")
                .ok()
                .filter(|v| !v.is_undefined() && !v.is_none());

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

            let timing_info = test
                .get_attr("timing")
                .ok()
                .filter(|v| !v.is_undefined() && !v.is_none());
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
                    <td><strong>{escaped_id}</strong></td>
                    <td><span class="badge {badge_cls}">{escaped_verdict}</span></td>
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

    let title = if reports.len() == 1 && !is_contest {
        format!("{}: {}", reports[0].problem, reports[0].submission)
    } else {
        "Contest Report".to_string()
    };

    let ctx = ReportContext {
        title: &title,
        generated_at: format_system_time_utc(std::time::SystemTime::now()),
        uj_version: env!("CARGO_PKG_VERSION"),
        is_contest,
        reports,
        standings,
    };

    tmpl.render(context! {
        title => ctx.title,
        generated_at => ctx.generated_at,
        uj_version => ctx.uj_version,
        is_contest => ctx.is_contest,
        reports => ctx.reports,
        standings => ctx.standings,
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

fn format_system_time_utc(st: std::time::SystemTime) -> String {
    let dur = st
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = dur.as_secs();
    let secs = total_secs % 60;
    let total_mins = total_secs / 60;
    let mins = total_mins % 60;
    let total_hours = total_mins / 60;
    let hours = total_hours % 24;
    let mut days = total_hours / 24;

    let mut year = 1970;
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
        let days_in_year = if leap { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let days_in_months = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1;
    for &dim in &days_in_months {
        if days < dim {
            break;
        }
        days -= dim;
        month += 1;
    }
    let day = days + 1;
    format!("{year:04}-{month:02}-{day:02} {hours:02}:{mins:02}:{secs:02} UTC")
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
                message: None,
                checker: Some("ok answer matches".to_string()),
                diff: None,
                timing: Default::default(),
                sandbox_tier: None,
            }],
        };

        let html = generate_html(&report).expect("HTML generation failed");
        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("aplusb: ac.cpp"));
        assert!(html.contains("ok answer matches"));
        assert!(html.contains("badge-ac"));
        // Tests without a diff must NOT render a bogus "Mismatch at Line , Column" block
        assert!(!html.contains("Mismatch at Line"));
        assert!(!html.contains("<div class=\"diff-block\">"));
    }

    #[test]
    fn test_html_score_precision_and_unassigned_subtask_tests() {
        let report = SubmissionJudgeReport {
            schema_version: 1,
            problem: "prob".to_string(),
            submission: "sub.cpp".to_string(),
            verdict: Verdict::PartiallyCorrect,
            score: 100.0 / 3.0,
            max_time_ms: 10,
            max_memory_kb: 512,
            subtasks: vec![crate::result::SubtaskResult {
                id: 1,
                score: 100.0 / 3.0,
                max_score: 50.0,
                verdict: Verdict::PartiallyCorrect,
                test_ids: vec!["1".to_string()],
            }],
            tests: vec![
                crate::result::SingleTestResult {
                    id: "1".to_string(),
                    subtask: Some(1),
                    verdict: Verdict::PartiallyCorrect,
                    score: 100.0 / 3.0,
                    time_ms: 5,
                    wall_ms: 6,
                    memory_kb: 512,
                    exit: Default::default(),
                    message: None,
                    checker: None,
                    diff: None,
                    timing: Default::default(),
                    sandbox_tier: None,
                },
                crate::result::SingleTestResult {
                    id: "orphan_case_99".to_string(),
                    subtask: None,
                    verdict: Verdict::Accepted,
                    score: 50.0,
                    time_ms: 5,
                    wall_ms: 6,
                    memory_kb: 512,
                    exit: Default::default(),
                    message: None,
                    checker: None,
                    diff: None,
                    timing: Default::default(),
                    sandbox_tier: None,
                },
            ],
        };

        let html = generate_html(&report).unwrap();
        assert!(html.contains("33.3"));
        assert!(!html.contains("33.3333"));
        assert!(html.contains("orphan_case_99"));
        assert!(html.contains("Other Tests"));
    }

    #[test]
    fn test_contest_html_standings_order_and_unique_row_ids() {
        let rep_bob = SubmissionJudgeReport {
            schema_version: 1,
            problem: "p1".to_string(),
            submission: "bob - p1".to_string(),
            verdict: Verdict::WrongAnswer,
            score: 0.0,
            max_time_ms: 50,
            max_memory_kb: 512,
            subtasks: vec![],
            tests: vec![crate::result::SingleTestResult {
                id: "1".to_string(),
                subtask: None,
                verdict: Verdict::WrongAnswer,
                score: 0.0,
                time_ms: 50,
                wall_ms: 55,
                memory_kb: 512,
                exit: Default::default(),
                message: None,
                checker: None,
                diff: None,
                timing: Default::default(),
                sandbox_tier: None,
            }],
        };
        let rep_alice = SubmissionJudgeReport {
            schema_version: 1,
            problem: "p1".to_string(),
            submission: "alice - p1".to_string(),
            verdict: Verdict::Accepted,
            score: 100.0,
            max_time_ms: 10,
            max_memory_kb: 512,
            subtasks: vec![],
            tests: vec![crate::result::SingleTestResult {
                id: "1".to_string(),
                subtask: None,
                verdict: Verdict::Accepted,
                score: 100.0,
                time_ms: 10,
                wall_ms: 12,
                memory_kb: 512,
                exit: Default::default(),
                message: None,
                checker: None,
                diff: None,
                timing: Default::default(),
                sandbox_tier: None,
            }],
        };

        // Pass bob first, alice second; standings must rank alice #1 before bob #2
        let html = generate_multi_html(&[rep_bob, rep_alice], true).unwrap();
        let standings_end = html.find("id=\"report-1\"").unwrap();
        let standings_slice = &html[..standings_end];
        let alice_pos = standings_slice.find("alice").unwrap();
        let bob_pos = standings_slice.find("bob").unwrap();
        assert!(
            alice_pos < bob_pos,
            "Alice (100 pts) must appear before Bob (0 pts) in Contest Standings"
        );
        // Unique expandable row IDs per report
        assert!(html.contains("id=\"test-detail-row-1-1\""));
        assert!(html.contains("id=\"test-detail-row-2-1\""));
    }
}
