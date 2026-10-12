//! Case-granular streaming pipeline TUI for real-time monitoring of Gen -> Validate -> Ans.

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use crossterm::{
    cursor::Hide,
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};
use uj_sandbox::CancelToken;

use crate::TerminalGuard;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Gen,
    Validate,
    Ans,
}

impl Stage {
    pub fn name(&self) -> &'static str {
        match self {
            Stage::Gen => "gen",
            Stage::Validate => "validate",
            Stage::Ans => "ans",
        }
    }
}

#[derive(Debug, Clone)]
pub enum CaseStageEvent {
    Started {
        idx: usize,
        stage: Stage,
    },
    StageDone {
        idx: usize,
        stage: Stage,
        ms: u64,
        cached: bool,
    },
    Failed {
        idx: usize,
        stage: Stage,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageStatus {
    Pending,
    Running,
    Done { ms: u64, cached: bool },
    Failed(String),
    NoGen,
}

#[derive(Debug, Clone)]
pub struct CaseState {
    pub name: String,
    pub has_gen: bool,
    pub gen_status: StageStatus,
    pub val_status: StageStatus,
    pub ans_status: StageStatus,
}

pub struct PipelineApp {
    pub problem: String,
    pub cases: Vec<CaseState>,
    pub start_time: Instant,
    pub workers: usize,
    pub recent_logs: VecDeque<(Instant, String)>,
    pub failed: Option<(usize, Stage, String)>,
    pub is_completed: bool,
    pub scroll_row: usize,
    pub cancel: CancelToken,

    // Counts
    pub gen_done: usize,
    pub val_done: usize,
    pub ans_done: usize,
    pub ans_cached: usize,
}

impl PipelineApp {
    pub fn new(
        problem: &str,
        case_names: &[String],
        has_gen: &[bool],
        workers: usize,
        cancel: CancelToken,
    ) -> Self {
        let cases = case_names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let hg = has_gen.get(i).copied().unwrap_or(true);
                CaseState {
                    name: name.clone(),
                    has_gen: hg,
                    gen_status: if hg {
                        StageStatus::Pending
                    } else {
                        StageStatus::NoGen
                    },
                    val_status: StageStatus::Pending,
                    ans_status: StageStatus::Pending,
                }
            })
            .collect();

        Self {
            problem: problem.to_string(),
            cases,
            start_time: Instant::now(),
            workers,
            recent_logs: VecDeque::with_capacity(10),
            failed: None,
            is_completed: false,
            scroll_row: 0,
            cancel,
            gen_done: 0,
            val_done: 0,
            ans_done: 0,
            ans_cached: 0,
        }
    }

    pub fn handle_event(&mut self, ev: CaseStageEvent) {
        let now = Instant::now();
        match ev {
            CaseStageEvent::Started { idx, stage } => {
                if let Some(case) = self.cases.get_mut(idx) {
                    match stage {
                        Stage::Gen => case.gen_status = StageStatus::Running,
                        Stage::Validate => case.val_status = StageStatus::Running,
                        Stage::Ans => case.ans_status = StageStatus::Running,
                    }
                }
            }
            CaseStageEvent::StageDone {
                idx,
                stage,
                ms,
                cached,
            } => {
                if let Some(case) = self.cases.get_mut(idx) {
                    let case_name = case.name.clone();
                    match stage {
                        Stage::Gen => {
                            case.gen_status = StageStatus::Done { ms, cached };
                            self.gen_done += 1;
                            let msg = if cached {
                                format!("#{case_name} gen (cached)")
                            } else {
                                format!("#{case_name} gen {ms}ms")
                            };
                            self.push_log(now, msg);
                        }
                        Stage::Validate => {
                            case.val_status = StageStatus::Done { ms, cached };
                            self.val_done += 1;
                            self.push_log(now, format!("#{case_name} validate ok ({ms}ms)"));
                        }
                        Stage::Ans => {
                            case.ans_status = StageStatus::Done { ms, cached };
                            self.ans_done += 1;
                            if cached {
                                self.ans_cached += 1;
                            }
                            let msg = if cached {
                                format!("#{case_name} ans (cached)")
                            } else {
                                format!("#{case_name} ans {ms}ms")
                            };
                            self.push_log(now, msg);
                        }
                    }
                }
                self.check_completion();
            }
            CaseStageEvent::Failed {
                idx,
                stage,
                message,
            } => {
                if let Some(case) = self.cases.get_mut(idx) {
                    let case_name = case.name.clone();
                    match stage {
                        Stage::Gen => case.gen_status = StageStatus::Failed(message.clone()),
                        Stage::Validate => case.val_status = StageStatus::Failed(message.clone()),
                        Stage::Ans => case.ans_status = StageStatus::Failed(message.clone()),
                    }
                    self.push_log(now, format!("#{case_name} {} FAILED", stage.name()));
                }
                if self.failed.is_none() {
                    self.failed = Some((idx, stage, message));
                    // Auto-scroll to the failed row (10 cases per row)
                    self.scroll_row = idx / 10;
                }
            }
        }
    }

    fn push_log(&mut self, time: Instant, log: String) {
        if self.recent_logs.len() >= 6 {
            self.recent_logs.pop_front();
        }
        self.recent_logs.push_back((time, log));
    }

    fn check_completion(&mut self) {
        if self.cases.is_empty() {
            self.is_completed = true;
            return;
        }
        let total = self.cases.len();
        if self.ans_done == total {
            self.is_completed = true;
        }
    }
}

pub fn run_pipeline_tui(
    problem: &str,
    case_names: &[String],
    has_gen: &[bool],
    workers: usize,
    rx: Receiver<CaseStageEvent>,
    cancel: CancelToken,
) -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stderr = io::stderr();
    execute!(stderr, EnterAlternateScreen, Hide)?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stderr);
    let mut terminal = Terminal::new(backend)?;

    let mut app = PipelineApp::new(problem, case_names, has_gen, workers, cancel);
    let mut exit_time: Option<Instant> = None;

    loop {
        // Drain events
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(ev) => app.handle_event(ev),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        if app.is_completed && app.failed.is_none() && exit_time.is_none() {
            exit_time = Some(Instant::now() + Duration::from_millis(600));
        }

        if disconnected && exit_time.is_none() && app.failed.is_none() {
            exit_time = Some(Instant::now() + Duration::from_millis(600));
        }

        if exit_time.is_some_and(|t| Instant::now() >= t) {
            break;
        }

        terminal.draw(|f| {
            draw_pipeline_ui(f, &mut app);
        })?;

        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
        {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                app.cancel.cancel();
                break;
            }
            // When failed, user can inspect error and press any key / Esc / Enter / q to exit
            if app.failed.is_some() {
                match key.code {
                    KeyCode::Char('k') | KeyCode::Up => {
                        if app.scroll_row > 0 {
                            app.scroll_row -= 1;
                        }
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        let total_rows = app.cases.len().div_ceil(10);
                        if app.scroll_row + 1 < total_rows {
                            app.scroll_row += 1;
                        }
                    }
                    _ => break,
                }
                continue;
            }

            match key.code {
                KeyCode::Char('q') | KeyCode::Char('Q') => {
                    app.cancel.cancel();
                    break;
                }
                KeyCode::Esc => {
                    break;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if app.scroll_row > 0 {
                        app.scroll_row -= 1;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let total_rows = app.cases.len().div_ceil(10);
                    if app.scroll_row + 1 < total_rows {
                        app.scroll_row += 1;
                    }
                }
                KeyCode::PageUp => {
                    app.scroll_row = app.scroll_row.saturating_sub(5);
                }
                KeyCode::PageDown => {
                    let total_rows = app.cases.len().div_ceil(10);
                    app.scroll_row = (app.scroll_row + 5).min(total_rows.saturating_sub(1));
                }
                _ => {}
            }
        }
    }

    Ok(())
}

fn draw_pipeline_ui(f: &mut Frame, app: &mut PipelineApp) {
    let area = f.area();
    let elapsed = app.start_time.elapsed();
    let elapsed_str = format!(
        "{:02}:{:02}.{:01}",
        elapsed.as_secs() / 60,
        elapsed.as_secs() % 60,
        elapsed.subsec_millis() / 100
    );

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5), // Progress bars
            Constraint::Min(6),    // Cases grid
            Constraint::Length(3), // Logs ticker
            Constraint::Length(2), // Status / error / help footer
        ])
        .split(area);

    // 1. Header & Progress Bars
    let (border_color, title_text) = if let Some((idx, stage, _)) = &app.failed {
        let case_name = app.cases.get(*idx).map(|c| c.name.as_str()).unwrap_or("?");
        (
            Color::Red,
            format!(
                " uj pipeline: {} ── FAILED at #{} ({}) ",
                app.problem,
                case_name,
                stage.name()
            ),
        )
    } else {
        (
            Color::Cyan,
            format!(
                " uj pipeline: {} ── Elapsed {} | Workers {} ",
                app.problem, elapsed_str, app.workers
            ),
        )
    };

    let total = app.cases.len().max(1);
    let gen_target = app.cases.iter().filter(|c| c.has_gen).count().max(1);

    let gen_pct = ((app.gen_done as f64 / gen_target as f64) * 100.0).clamp(0.0, 100.0);
    let val_pct = ((app.val_done as f64 / total as f64) * 100.0).clamp(0.0, 100.0);
    let ans_pct = ((app.ans_done as f64 / total as f64) * 100.0).clamp(0.0, 100.0);

    let make_bar = |pct: f64, width: usize| -> (String, String) {
        let filled = ((pct / 100.0) * width as f64).round() as usize;
        let filled = filled.min(width);
        ("█".repeat(filled), "░".repeat(width - filled))
    };

    let (g_fill, g_empty) = make_bar(gen_pct, 28);
    let (v_fill, v_empty) = make_bar(val_pct, 28);
    let (a_fill, a_empty) = make_bar(ans_pct, 28);

    let ans_extra = if app.ans_cached > 0 {
        format!(" ({} cached)", app.ans_cached)
    } else {
        String::new()
    };

    let bold_style = Style::default().add_modifier(Modifier::BOLD);

    let p_lines = vec![
        Line::from(vec![
            Span::styled("  Generate  ", bold_style),
            Span::styled("[", Style::default().fg(Color::DarkGray)),
            Span::styled(&g_fill, Style::default().fg(Color::Cyan)),
            Span::styled(&g_empty, Style::default().fg(Color::DarkGray)),
            Span::styled("] ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}/{}", app.gen_done, gen_target),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Validate  ", bold_style),
            Span::styled("[", Style::default().fg(Color::DarkGray)),
            Span::styled(&v_fill, Style::default().fg(Color::Green)),
            Span::styled(&v_empty, Style::default().fg(Color::DarkGray)),
            Span::styled("] ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}/{}", app.val_done, total),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Answer    ", bold_style),
            Span::styled("[", Style::default().fg(Color::DarkGray)),
            Span::styled(&a_fill, Style::default().fg(Color::Blue)),
            Span::styled(&a_empty, Style::default().fg(Color::DarkGray)),
            Span::styled("] ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}/{}{ans_extra}", app.ans_done, total),
                Style::default().fg(Color::White),
            ),
        ]),
    ];

    let header_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(title_text);
    let p_para = Paragraph::new(p_lines).block(header_block);
    f.render_widget(p_para, chunks[0]);

    // 2. Cases Grid (10 per row)
    let grid_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Cases [G/V/A: ✓ Done, ⟳ Run, ░ Pend, ✗ Fail, - N/A] ");

    let inner_height = chunks[1].height.saturating_sub(2) as usize;
    let total_rows = app.cases.len().div_ceil(10);

    let start_row = app.scroll_row.min(total_rows.saturating_sub(inner_height));
    let end_row = (start_row + inner_height).min(total_rows);

    let mut grid_lines = Vec::new();
    for r in start_row..end_row {
        let start_idx = r * 10;
        let end_idx = (start_idx + 10).min(app.cases.len());

        let mut spans = Vec::new();
        // Row label: e.g. "   1-10  "
        let label = format!("{:>4}-{:<4} ", start_idx + 1, end_idx);
        spans.push(Span::styled(label, Style::default().fg(Color::DarkGray)));

        for i in start_idx..end_idx {
            let case = &app.cases[i];
            let g_span = render_stage_status(&case.gen_status);
            let v_span = render_stage_status(&case.val_status);
            let a_span = render_stage_status(&case.ans_status);
            spans.push(g_span);
            spans.push(v_span);
            spans.push(a_span);
            spans.push(Span::raw(" "));
        }
        grid_lines.push(Line::from(spans));
    }

    let grid_para = Paragraph::new(grid_lines).block(grid_block);
    f.render_widget(grid_para, chunks[1]);

    // 3. Recent logs ticker (last 3-4 entries on one or two lines)
    let log_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Activity ");

    let log_spans: Vec<Span> = app
        .recent_logs
        .iter()
        .rev()
        .take(3)
        .rev()
        .flat_map(|(t, text)| {
            let elapsed = t.duration_since(app.start_time);
            let time_str = format!(
                "[{:02}:{:02}.{:02}] ",
                elapsed.as_secs() / 60,
                elapsed.as_secs() % 60,
                (elapsed.subsec_millis() / 10) % 100
            );
            vec![
                Span::styled(time_str, Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{text}   "), Style::default().fg(Color::White)),
            ]
        })
        .collect();

    let log_para = Paragraph::new(Line::from(log_spans)).block(log_block);
    f.render_widget(log_para, chunks[2]);

    // 4. Footer / Help / Error
    if let Some((idx, stage, msg)) = &app.failed {
        let case_name = app.cases.get(*idx).map(|c| c.name.as_str()).unwrap_or("?");
        let err_first_line = msg.lines().next().unwrap_or(msg);
        let footer_text = Line::from(vec![
            Span::styled(
                " [FAILED] ",
                Style::default()
                    .bg(Color::Red)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    " Test #{case_name} ({}) failed: {err_first_line} | Press any key to exit",
                    stage.name()
                ),
                Style::default().fg(Color::Red),
            ),
        ]);
        f.render_widget(Paragraph::new(footer_text), chunks[3]);
    } else {
        let help_text = Line::from(vec![
            Span::styled(" ↑↓/jk", Style::default().fg(Color::Yellow)),
            Span::styled(" 滚动  ", Style::default().fg(Color::DarkGray)),
            Span::styled("PgUp/PgDn", Style::default().fg(Color::Yellow)),
            Span::styled(" 翻页  ", Style::default().fg(Color::DarkGray)),
            Span::styled("q", Style::default().fg(Color::Yellow)),
            Span::styled(" 取消", Style::default().fg(Color::DarkGray)),
        ]);
        f.render_widget(Paragraph::new(help_text), chunks[3]);
    }
}

fn render_stage_status(status: &StageStatus) -> Span<'static> {
    match status {
        StageStatus::Pending => Span::styled("░", Style::default().fg(Color::DarkGray)),
        StageStatus::Running => Span::styled(
            "⟳",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        StageStatus::Done { .. } => Span::styled(
            "✓",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        StageStatus::Failed(_) => Span::styled(
            "✗",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        StageStatus::NoGen => Span::styled("-", Style::default().fg(Color::DarkGray)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pipeline_tui_state() {
        let cancel = CancelToken::new();
        let cases = vec!["1".to_string(), "2".to_string(), "3".to_string()];
        let has_gen = vec![true, true, false];
        let mut app = PipelineApp::new("sample_prob", &cases, &has_gen, 4, cancel);

        assert_eq!(app.cases[0].gen_status, StageStatus::Pending);
        assert_eq!(app.cases[2].gen_status, StageStatus::NoGen);

        app.handle_event(CaseStageEvent::Started {
            idx: 0,
            stage: Stage::Gen,
        });
        assert_eq!(app.cases[0].gen_status, StageStatus::Running);

        app.handle_event(CaseStageEvent::StageDone {
            idx: 0,
            stage: Stage::Gen,
            ms: 15,
            cached: false,
        });
        assert!(matches!(
            app.cases[0].gen_status,
            StageStatus::Done {
                ms: 15,
                cached: false
            }
        ));
        assert_eq!(app.gen_done, 1);

        app.handle_event(CaseStageEvent::Failed {
            idx: 1,
            stage: Stage::Validate,
            message: "Value out of bounds".to_string(),
        });
        assert!(matches!(app.cases[1].val_status, StageStatus::Failed(_)));
        assert_eq!(
            app.failed.as_ref().map(|(idx, s, _)| (*idx, *s)),
            Some((1, Stage::Validate))
        );
    }
}
