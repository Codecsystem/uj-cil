//! Terminal User Interface (TUI) for real-time judge monitoring using Ratatui.

use crossbeam_channel::Receiver;
use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table},
};
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    time::{Duration, Instant},
};
use uj_core::verdict::Verdict;
use uj_report::result::{JudgeEvent, SingleTestResult, SubmissionJudgeReport, SubtaskResult};
use uj_sandbox::CancelToken;

pub mod multi;
pub use multi::{SubmissionMeta, run_multi_tui};

pub mod pipeline;
pub use pipeline::{CaseStageEvent, Stage, run_pipeline_tui};

/// Test case execution status in the live UI.
#[derive(Debug, Clone)]
pub enum TestCaseStatus {
    Pending,
    Running,
    Done(Box<SingleTestResult>),
}

/// Slot / Worker state.
#[derive(Debug, Clone)]
pub struct SlotInfo {
    pub id: usize,
    pub active_test: Option<String>,
}

/// Application state for the TUI monitor.
pub struct TuiApp {
    pub problem_name: String,
    pub submission_name: String,
    pub total_tests: usize,
    pub test_entries: Vec<(String, TestCaseStatus)>,
    pub slots: Vec<SlotInfo>,
    pub subtasks: BTreeMap<u32, SubtaskResult>,
    pub logs: VecDeque<String>,
    pub start_time: Instant,
    pub final_report: Option<SubmissionJudgeReport>,
    pub is_completed: bool,
    pub selected_idx: usize,
    pub scroll_offset: std::cell::Cell<usize>,
    pub cancel_token: CancelToken,
}

impl TuiApp {
    pub fn new(
        problem_name: &str,
        submission_name: &str,
        test_names: &[String],
        slot_count: usize,
        cancel_token: CancelToken,
    ) -> Self {
        let test_entries = test_names
            .iter()
            .map(|name| (name.clone(), TestCaseStatus::Pending))
            .collect();

        let slots = (0..slot_count.max(1))
            .map(|id| SlotInfo {
                id,
                active_test: None,
            })
            .collect();

        let mut logs = VecDeque::with_capacity(50);
        logs.push_back("Judge session initialized.".to_string());

        Self {
            problem_name: problem_name.to_string(),
            submission_name: submission_name.to_string(),
            total_tests: test_names.len(),
            test_entries,
            slots,
            subtasks: BTreeMap::new(),
            logs,
            start_time: Instant::now(),
            final_report: None,
            is_completed: false,
            selected_idx: 0,
            scroll_offset: std::cell::Cell::new(0),
            cancel_token,
        }
    }

    pub fn handle_event(&mut self, event: JudgeEvent) {
        let elapsed = self.start_time.elapsed().as_secs_f64();
        let timestamp = format!("[{:05.2}s]", elapsed);

        match event {
            JudgeEvent::CompileStart { target, compiler } => {
                self.add_log(format!("{timestamp} Compiling {target} ({compiler})..."));
            }
            JudgeEvent::CompileDone {
                target,
                success,
                duration_ms,
                ..
            } => {
                if success {
                    self.add_log(format!("{timestamp} Compiled {target} in {duration_ms}ms"));
                } else {
                    self.add_log(format!("{timestamp} FAILED compiling {target}"));
                }
            }
            JudgeEvent::TestStart { id, slot, .. } => {
                if let Some(entry) = self.test_entries.iter_mut().find(|(name, _)| *name == id) {
                    entry.1 = TestCaseStatus::Running;
                }
                if let Some(s) = slot.filter(|&s| s < self.slots.len()) {
                    self.slots[s].active_test = Some(id.clone());
                } else {
                    // Assign to first idle slot
                    for slot in &mut self.slots {
                        if slot.active_test.is_none() {
                            slot.active_test = Some(id.clone());
                            break;
                        }
                    }
                }
            }
            JudgeEvent::TestDone(res) => {
                let id = res.id.clone();
                let verd = res.verdict.short_code();
                let time = res.time_ms;

                let mem_kb = res.memory_kb;

                if let Some(entry) = self.test_entries.iter_mut().find(|(name, _)| *name == id) {
                    entry.1 = TestCaseStatus::Done(Box::new(res));
                }
                // Free slot
                for slot in &mut self.slots {
                    if slot.active_test.as_deref() == Some(&id) {
                        slot.active_test = None;
                        break;
                    }
                }
                let mem_str = if mem_kb > 1024 {
                    format!("{:.1}M", mem_kb as f64 / 1024.0)
                } else {
                    format!("{}K", mem_kb)
                };
                self.add_log(format!(
                    "{timestamp} Test {id}: {verd} ({time}ms, {mem_str})"
                ));
            }
            JudgeEvent::SubtaskDone(st) => {
                let id = st.id;
                let score = st.score;
                let max = st.max_score;
                let verd = st.verdict.short_code();
                self.subtasks.insert(st.id, st);
                self.add_log(format!(
                    "{timestamp} Subtask {id} finished: {verd} ({score:.1}/{max:.1})"
                ));
            }
            JudgeEvent::SubmissionDone(report) => {
                let verd = report.verdict.display_name();
                let score = report.score;
                self.final_report = Some(report);
                self.is_completed = true;
                // Mark all slots idle
                for slot in &mut self.slots {
                    slot.active_test = None;
                }
                self.add_log(format!(
                    "{timestamp} All done! Final verdict: {verd} ({score:.1} pts)"
                ));
            }
            JudgeEvent::Warning { message, .. } => {
                self.add_log(format!("{timestamp} WARN: {message}"));
            }
            JudgeEvent::Summary { .. } => {}
        }
    }

    fn add_log(&mut self, msg: String) {
        if self.logs.len() >= 50 {
            self.logs.pop_front();
        }
        self.logs.push_back(msg);
    }
}

/// RAII Guard ensuring terminal restoration on exit or panic.
pub struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stderr(), LeaveAlternateScreen, Show);
    }
}

/// Launches the live Ratatui TUI monitor.
///
/// Runs until the judging session is complete or the user cancels with `q` or `Esc`.
pub fn run_tui(
    problem_name: &str,
    submission_name: &str,
    test_names: &[String],
    slot_count: usize,
    event_rx: Receiver<JudgeEvent>,
    cancel_token: CancelToken,
    auto_exit_on_done: bool,
) -> Result<Option<SubmissionJudgeReport>, Box<dyn std::error::Error>> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stderr = io::stderr();
    execute!(stderr, EnterAlternateScreen, Hide)?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stderr);
    let mut terminal = Terminal::new(backend)?;

    let mut app = TuiApp::new(
        problem_name,
        submission_name,
        test_names,
        slot_count,
        cancel_token,
    );

    let mut completion_time: Option<Instant> = None;
    let mut all_done_time: Option<Instant> = None;

    loop {
        // Drain pending events
        let mut disconnected = false;
        loop {
            match event_rx.try_recv() {
                Ok(ev) => {
                    all_done_time = None;
                    app.handle_event(ev);
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        if disconnected {
            app.is_completed = true;
            for slot in &mut app.slots {
                slot.active_test = None;
            }
        }

        // If all tests are done and all slots are idle, but SubmissionDone didn't arrive,
        // mark completed after 800ms
        if !app.is_completed
            && app
                .test_entries
                .iter()
                .all(|(_, st)| matches!(st, TestCaseStatus::Done(_)))
            && app.slots.iter().all(|s| s.active_test.is_none())
        {
            let t = all_done_time.get_or_insert_with(Instant::now);
            if t.elapsed() >= Duration::from_millis(800) {
                app.is_completed = true;
            }
        } else if !app.slots.iter().all(|s| s.active_test.is_none()) {
            all_done_time = None;
        }

        // Draw UI
        terminal.draw(|f| draw_ui(f, &app))?;

        if app.is_completed && completion_time.is_none() {
            completion_time = Some(Instant::now());
        }

        if auto_exit_on_done
            && let Some(comp_t) = completion_time
            && comp_t.elapsed() >= Duration::from_millis(600)
        {
            break;
        }

        // Handle user input
        if event::poll(Duration::from_millis(40))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind == crossterm::event::KeyEventKind::Release {
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    app.cancel_token.cancel();
                    break;
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.cancel_token.cancel();
                    break;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if app.selected_idx > 0 {
                        app.selected_idx -= 1;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if app.selected_idx + 1 < app.test_entries.len() {
                        app.selected_idx += 1;
                    }
                }
                KeyCode::PageUp => {
                    app.selected_idx = app.selected_idx.saturating_sub(15);
                }
                KeyCode::PageDown => {
                    app.selected_idx =
                        (app.selected_idx + 15).min(app.test_entries.len().saturating_sub(1));
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    app.selected_idx = 0;
                }
                KeyCode::End | KeyCode::Char('G') => {
                    app.selected_idx = app.test_entries.len().saturating_sub(1);
                }
                KeyCode::Enter
                    if app.is_completed
                        || app
                            .test_entries
                            .iter()
                            .all(|(_, st)| matches!(st, TestCaseStatus::Done(_))) =>
                {
                    break;
                }
                _ => {}
            }
        }
    }

    Ok(app.final_report)
}

fn draw_ui(f: &mut Frame, app: &TuiApp) {
    let main_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(10),   // Content
            Constraint::Length(3), // Footer
        ])
        .split(f.area());

    draw_header(f, app, main_layout[0]);
    draw_body(f, app, main_layout[1]);
    draw_footer(f, app, main_layout[2]);
}

fn draw_header(f: &mut Frame, app: &TuiApp, area: Rect) {
    let elapsed = app.start_time.elapsed();
    let elapsed_str = format!(
        "{:02}:{:02}.{:02}",
        elapsed.as_secs() / 60,
        elapsed.as_secs() % 60,
        elapsed.subsec_millis() / 10
    );

    let status_span = if app.cancel_token.is_cancelled() {
        Span::styled(
            "CANCELLED",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else if let Some(ref r) = app.final_report {
        let (color, name) = match r.verdict {
            Verdict::Accepted => (Color::Green, "ACCEPTED"),
            Verdict::WrongAnswer => (Color::Red, "WRONG ANSWER"),
            Verdict::TimeLimitExceeded => (Color::Yellow, "TIME LIMIT EXCEEDED"),
            Verdict::MemoryLimitExceeded => (Color::Magenta, "MEMORY LIMIT EXCEEDED"),
            Verdict::RuntimeError => (Color::LightRed, "RUNTIME ERROR"),
            Verdict::CompileError => (Color::Yellow, "COMPILE ERROR"),
            _ => (Color::Red, "JUDGE FAILED"),
        };
        Span::styled(
            format!("{name} ({:.1} pts)", r.score),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )
    } else if app.is_completed {
        let mut worst_verdict = Verdict::Accepted;
        let mut total_score = 0.0;
        let mut any_done = false;
        for (_, st) in &app.test_entries {
            if let TestCaseStatus::Done(res) = st {
                any_done = true;
                total_score += res.score;
                if res.verdict.priority() > worst_verdict.priority() {
                    worst_verdict = res.verdict;
                }
            }
        }
        if any_done {
            let (color, name) = match worst_verdict {
                Verdict::Accepted => (Color::Green, "ACCEPTED"),
                Verdict::WrongAnswer => (Color::Red, "WRONG ANSWER"),
                Verdict::TimeLimitExceeded => (Color::Yellow, "TIME LIMIT EXCEEDED"),
                Verdict::MemoryLimitExceeded => (Color::Magenta, "MEMORY LIMIT EXCEEDED"),
                Verdict::RuntimeError => (Color::LightRed, "RUNTIME ERROR"),
                Verdict::CompileError => (Color::Yellow, "COMPILE ERROR"),
                _ => (Color::Red, "JUDGE FAILED"),
            };
            Span::styled(
                format!("{name} ({total_score:.1} pts)"),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                "COMPLETED",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )
        }
    } else {
        let done_count = app
            .test_entries
            .iter()
            .filter(|(_, st)| matches!(st, TestCaseStatus::Done(_)))
            .count();
        if done_count == app.total_tests && app.slots.iter().any(|s| s.active_test.is_some()) {
            Span::styled(
                "PRECISE REJUDGING",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                format!("RUNNING ({done_count}/{} done)", app.total_tests),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
        }
    };

    let (peak_time, peak_mem) = if let Some(ref r) = app.final_report {
        (r.max_time_ms, r.max_memory_kb)
    } else {
        let mut max_t = 0;
        let mut max_m = 0;
        for (_, st) in &app.test_entries {
            if let TestCaseStatus::Done(res) = st {
                max_t = max_t.max(res.time_ms);
                max_m = max_m.max(res.memory_kb);
            }
        }
        (max_t, max_m)
    };

    let peak_str = if peak_mem > 0 || peak_time > 0 {
        let mem_str = if peak_mem > 1024 {
            format!("{:.1}M", peak_mem as f64 / 1024.0)
        } else {
            format!("{}K", peak_mem)
        };
        format!(" | Peak: {peak_time}ms, {mem_str}")
    } else {
        String::new()
    };

    let mut spans = vec![
        Span::styled(
            " uj ",
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" Problem: "),
        Span::styled(
            &app.problem_name,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" | Submission: "),
        Span::styled(&app.submission_name, Style::default().fg(Color::White)),
        Span::raw(" | Status: "),
        status_span,
    ];
    if !peak_str.is_empty() {
        spans.push(Span::styled(peak_str, Style::default().fg(Color::Cyan)));
    }
    spans.push(Span::raw(" | Elapsed: "));
    spans.push(Span::styled(
        elapsed_str,
        Style::default().fg(Color::DarkGray),
    ));
    let text = Line::from(spans);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));

    let paragraph = Paragraph::new(text).block(block);
    f.render_widget(paragraph, area);
}

fn draw_body(f: &mut Frame, app: &TuiApp, area: Rect) {
    let body_split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(55), // Tests table
            Constraint::Percentage(45), // Right panel (Slots + Subtasks + Logs)
        ])
        .split(area);

    draw_tests_table(
        f,
        &app.test_entries,
        app.selected_idx,
        &app.scroll_offset,
        body_split[0],
    );
    draw_right_panel(f, app, body_split[1]);
}

pub fn draw_tests_table(
    f: &mut Frame,
    test_entries: &[(String, TestCaseStatus)],
    selected_idx: usize,
    scroll_offset: &std::cell::Cell<usize>,
    area: Rect,
) {
    let header_row = Row::new(vec![
        Cell::from("#").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Test").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Verdict").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Time").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Memory").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Score").style(Style::default().add_modifier(Modifier::BOLD)),
    ])
    .style(Style::default().fg(Color::DarkGray));

    let visible_count = (area.height as usize).saturating_sub(4).max(1);
    let mut offset = scroll_offset.get();
    if selected_idx < offset {
        offset = selected_idx;
    } else if selected_idx >= offset + visible_count {
        offset = selected_idx + 1 - visible_count;
    }
    scroll_offset.set(offset);

    let total_tests = test_entries.len();
    let rows: Vec<Row> = test_entries
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible_count)
        .map(|(i, (name, status))| {
            let num = (i + 1).to_string();
            let (verd_cell, time_str, mem_str, score_str) = match status {
                TestCaseStatus::Pending => (
                    Cell::from(Span::styled("PEND", Style::default().fg(Color::DarkGray))),
                    "-".to_string(),
                    "-".to_string(),
                    "-".to_string(),
                ),
                TestCaseStatus::Running => (
                    Cell::from(Span::styled(
                        "RUN ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )),
                    "...".to_string(),
                    "...".to_string(),
                    "...".to_string(),
                ),
                TestCaseStatus::Done(res) => {
                    let v = res.verdict;
                    let color = match v {
                        Verdict::Accepted => Color::Green,
                        Verdict::WrongAnswer => Color::Red,
                        Verdict::TimeLimitExceeded => Color::Yellow,
                        Verdict::MemoryLimitExceeded => Color::Magenta,
                        Verdict::RuntimeError => Color::LightRed,
                        _ => Color::DarkGray,
                    };
                    let v_cell = Cell::from(Span::styled(
                        v.short_code(),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ));
                    let time = format!("{} ms", res.time_ms);
                    let mem = if res.memory_kb > 1024 {
                        format!("{:.1}M", res.memory_kb as f64 / 1024.0)
                    } else {
                        format!("{}K", res.memory_kb)
                    };
                    let sc = format!("{:.1}", res.score);
                    (v_cell, time, mem, sc)
                }
            };

            let row_style = if i == selected_idx {
                Style::default()
                    .bg(Color::Rgb(40, 50, 75))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            Row::new(vec![
                Cell::from(num),
                Cell::from(name.as_str()),
                verd_cell,
                Cell::from(time_str),
                Cell::from(mem_str),
                Cell::from(score_str),
            ])
            .style(row_style)
        })
        .collect();

    let title = if total_tests > visible_count {
        format!(
            " Tests [{}-{} of {}] (▲/▼ to scroll) ",
            offset + 1,
            (offset + visible_count).min(total_tests),
            total_tests
        )
    } else {
        format!(" Tests [{total_tests}] ")
    };

    let table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(8),
        ],
    )
    .header(header_row)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(title)
            .border_style(Style::default().fg(Color::DarkGray)),
    );

    f.render_widget(table, area);
}

fn draw_right_panel(f: &mut Frame, app: &TuiApp, area: Rect) {
    let right_split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6), // Slots / Cores
            Constraint::Length(6), // Subtasks
            Constraint::Min(6),    // Logs
        ])
        .split(area);

    // 1. Slots Panel
    let slot_lines: Vec<Line> = app
        .slots
        .iter()
        .map(|s| {
            let (status_text, style) = match &s.active_test {
                Some(test) => (format!("Judging {test}"), Style::default().fg(Color::Cyan)),
                None => ("Idle".to_string(), Style::default().fg(Color::DarkGray)),
            };
            Line::from(vec![
                Span::styled(
                    format!(" Core #{}: ", s.id),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(status_text, style),
            ])
        })
        .collect();

    let slots_widget = Paragraph::new(slot_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" CPU Slots ")
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(slots_widget, right_split[0]);

    // 2. Subtasks Panel
    let subtask_lines: Vec<Line> = if app.subtasks.is_empty() {
        vec![Line::from(Span::styled(
            " (No subtasks configured)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        app.subtasks
            .values()
            .map(|st| {
                let color = if st.verdict.is_accepted() {
                    Color::Green
                } else {
                    Color::Red
                };
                Line::from(vec![
                    Span::styled(
                        format!(" Subtask {}: ", st.id),
                        Style::default().fg(Color::White),
                    ),
                    Span::styled(
                        st.verdict.short_code(),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(" ({:.1} / {:.1} pts)", st.score, st.max_score)),
                ])
            })
            .collect()
    };

    let subtasks_widget = Paragraph::new(subtask_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Subtasks ")
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(subtasks_widget, right_split[1]);

    // 3. Log Panel
    let visible_log_count = (right_split[2].height as usize).saturating_sub(2);
    let log_lines: Vec<Line> = app
        .logs
        .iter()
        .rev()
        .take(visible_log_count)
        .rev()
        .map(|l| Line::from(Span::styled(l, Style::default().fg(Color::DarkGray))))
        .collect();

    let logs_widget = Paragraph::new(log_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Event Logs ")
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(logs_widget, right_split[2]);
}

fn draw_footer(f: &mut Frame, _app: &TuiApp, area: Rect) {
    let help_line = Line::from(vec![
        Span::styled(
            " [q/Esc] ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("Quit/Cancel   "),
        Span::styled(
            " [↑/↓] ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("Navigate Tests   "),
        Span::styled(
            " [Enter] ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("Dismiss when done"),
    ]);

    let footer = Paragraph::new(help_line).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(footer, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tui_app_lifecycle() {
        let cancel = CancelToken::new();
        let test_names = vec!["1".to_string(), "2".to_string()];
        let mut app = TuiApp::new("aplusb", "sol.cpp", &test_names, 2, cancel);

        assert_eq!(app.total_tests, 2);
        assert_eq!(app.slots.len(), 2);
        assert!(!app.is_completed);

        // Compile events
        app.handle_event(JudgeEvent::CompileStart {
            target: "sol.cpp".to_string(),
            compiler: "g++".to_string(),
        });
        app.handle_event(JudgeEvent::CompileDone {
            target: "sol.cpp".to_string(),
            success: true,
            duration_ms: 120,
            error_output: None,
        });

        // Test start
        app.handle_event(JudgeEvent::TestStart {
            id: "1".to_string(),
            subtask: None,
            slot: None,
        });
        assert_eq!(app.slots[0].active_test.as_deref(), Some("1"));
        assert!(matches!(app.test_entries[0].1, TestCaseStatus::Running));

        // Test done
        let res1 = SingleTestResult {
            id: "1".to_string(),
            subtask: None,
            verdict: Verdict::Accepted,
            score: 50.0,
            time_ms: 4,
            wall_ms: 5,
            memory_kb: 1024,
            exit: Default::default(),
            message: None,
            checker: None,
            diff: None,
            timing: Default::default(),
            sandbox_tier: None,
        };
        app.handle_event(JudgeEvent::TestDone(res1));
        assert!(app.slots[0].active_test.is_none());
        assert!(matches!(app.test_entries[0].1, TestCaseStatus::Done(_)));

        // Submission done
        let report = SubmissionJudgeReport {
            schema_version: 1,
            problem: "aplusb".to_string(),
            submission: "sol.cpp".to_string(),
            verdict: Verdict::Accepted,
            score: 100.0,
            max_time_ms: 4,
            max_memory_kb: 1024,
            subtasks: vec![],
            tests: vec![],
        };
        app.handle_event(JudgeEvent::SubmissionDone(report));
        assert!(app.is_completed);
    }
}
