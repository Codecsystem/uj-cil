//! Master-Detail Multi-Submission TUI for concurrent contest and verify sessions.

use std::cell::Cell;
use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell as TableCell, Paragraph, Row, Table},
};
use uj_core::verdict::Verdict;
use uj_report::result::{JudgeEvent, PoolEvent};
use uj_sandbox::CancelToken;

use crate::{SystemMonitor, TerminalGuard, TestCaseStatus, draw_tests_table};

pub fn format_time_ms(ms: u64) -> String {
    if ms == 0 {
        "-".to_string()
    } else if ms >= 1000 {
        format!("{:.2}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

pub fn format_memory_kb(kb: u64) -> String {
    if kb == 0 {
        "-".to_string()
    } else if kb >= 1024 * 1024 {
        format!("{:.2}GB", kb as f64 / (1024.0 * 1024.0))
    } else if kb >= 1024 {
        format!("{:.1}MB", kb as f64 / 1024.0)
    } else {
        format!("{kb}KB")
    }
}

#[derive(Debug, Clone)]
pub struct SubmissionMeta {
    pub id: usize,
    pub problem: String,
    pub name: String,
    pub test_names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SubStatus {
    Pending,
    Running,
    Done(Verdict, f64),
}

pub struct SubState {
    pub meta: SubmissionMeta,
    pub status: SubStatus,
    pub tests: Vec<(String, TestCaseStatus)>,
    pub max_time_ms: u64,
    pub max_memory_kb: u64,
    pub score: Option<f64>,
}

pub struct MultiApp {
    pub title: String,
    pub subs: Vec<SubState>,
    pub slots: Vec<Option<(usize, String, Instant)>>,
    pub cpu_cores: Vec<Option<usize>>,
    pub recent_done_times: VecDeque<Instant>,
    pub selected_sub_idx: usize,
    pub sub_scroll_offset: Cell<usize>,
    pub test_scroll_offset: Cell<usize>,
    pub selected_test_idx: usize,
    pub fullscreen_sub_idx: Option<usize>,
    pub entered_fullscreen_at: Option<Instant>,
    pub start_time: Instant,
    pub total_tests: usize,
    pub completed_tests: usize,
    pub is_completed: bool,
    pub cancel_token: CancelToken,
    pub sys_monitor: SystemMonitor,
}

impl MultiApp {
    pub fn new(
        title: &str,
        metas: &[SubmissionMeta],
        slot_count: usize,
        cpu_cores: Vec<Option<usize>>,
        cancel_token: CancelToken,
    ) -> Self {
        let subs: Vec<SubState> = metas
            .iter()
            .map(|m| {
                let tests = m
                    .test_names
                    .iter()
                    .map(|n| (n.clone(), TestCaseStatus::Pending))
                    .collect();
                SubState {
                    meta: m.clone(),
                    status: SubStatus::Pending,
                    tests,
                    max_time_ms: 0,
                    max_memory_kb: 0,
                    score: None,
                }
            })
            .collect();

        let total_tests: usize = metas.iter().map(|m| m.test_names.len()).sum();
        let slots = vec![None; slot_count.max(1)];

        Self {
            title: title.to_string(),
            subs,
            slots,
            cpu_cores,
            recent_done_times: VecDeque::with_capacity(200),
            selected_sub_idx: 0,
            sub_scroll_offset: Cell::new(0),
            test_scroll_offset: Cell::new(0),
            selected_test_idx: 0,
            fullscreen_sub_idx: None,
            entered_fullscreen_at: None,
            start_time: Instant::now(),
            total_tests,
            completed_tests: 0,
            is_completed: metas.is_empty(),
            cancel_token,
            sys_monitor: SystemMonitor::new(),
        }
    }

    pub fn handle_event(&mut self, event: PoolEvent) {
        let sub_id = event.submission.id;
        let sub_idx = self.subs.iter().position(|s| s.meta.id == sub_id);
        let Some(s_idx) = sub_idx else {
            return;
        };

        match event.inner {
            JudgeEvent::TestStart { id, slot, .. } => {
                let sub = &mut self.subs[s_idx];
                if sub.status == SubStatus::Pending {
                    sub.status = SubStatus::Running;
                }
                if let Some(entry) = sub.tests.iter_mut().find(|(name, _)| *name == id) {
                    entry.1 = TestCaseStatus::Running;
                }
                if let Some(s) = slot.filter(|&s| s < self.slots.len()) {
                    self.slots[s] = Some((sub_id, id, Instant::now()));
                } else {
                    for slot_opt in &mut self.slots {
                        if slot_opt.is_none() {
                            *slot_opt = Some((sub_id, id, Instant::now()));
                            break;
                        }
                    }
                }
            }
            JudgeEvent::TestDone(res) => {
                let id = res.id.clone();
                let sub = &mut self.subs[s_idx];
                sub.max_time_ms = sub.max_time_ms.max(res.time_ms);
                sub.max_memory_kb = sub.max_memory_kb.max(res.memory_kb);
                if let Some(entry) = sub.tests.iter_mut().find(|(name, _)| *name == id) {
                    entry.1 = TestCaseStatus::Done(Box::new(res));
                }
                let all_done = sub
                    .tests
                    .iter()
                    .all(|(_, st)| matches!(st, TestCaseStatus::Done(..)));
                if all_done && matches!(sub.status, SubStatus::Running | SubStatus::Pending) {
                    let mut agg_verdict = Verdict::Accepted;
                    let mut total_score = 0.0;
                    for (_, st) in &sub.tests {
                        if let TestCaseStatus::Done(r) = st {
                            agg_verdict = agg_verdict.merge(r.verdict);
                            total_score += r.score;
                        }
                    }
                    sub.status = SubStatus::Done(agg_verdict, total_score);
                    if sub.score.is_none() {
                        sub.score = Some(total_score);
                    }
                    if self
                        .subs
                        .iter()
                        .all(|s| matches!(s.status, SubStatus::Done(..)))
                    {
                        self.is_completed = true;
                    }
                }
                self.completed_tests += 1;
                let now = Instant::now();
                self.recent_done_times.push_back(now);
                while let Some(front) = self.recent_done_times.front() {
                    if now.duration_since(*front) > Duration::from_secs(5) {
                        self.recent_done_times.pop_front();
                    } else {
                        break;
                    }
                }
                for slot in &mut self.slots {
                    if let Some((s_id, t_id, _)) = slot
                        && *s_id == sub_id
                        && t_id == &id
                    {
                        *slot = None;
                        break;
                    }
                }
            }
            JudgeEvent::SubmissionDone(report) => {
                let sub = &mut self.subs[s_idx];
                sub.status = SubStatus::Done(report.verdict, report.score);
                sub.max_time_ms = sub.max_time_ms.max(report.max_time_ms);
                sub.max_memory_kb = sub.max_memory_kb.max(report.max_memory_kb);
                sub.score = Some(report.score);
                if self
                    .subs
                    .iter()
                    .all(|s| matches!(s.status, SubStatus::Done(..)))
                {
                    self.is_completed = true;
                }
            }
            JudgeEvent::Summary {
                final_verdict,
                final_score,
                peak_memory_kb,
                ..
            } => {
                let sub = &mut self.subs[s_idx];
                sub.max_memory_kb = sub.max_memory_kb.max(peak_memory_kb);
                sub.score = Some(final_score);
                if !matches!(sub.status, SubStatus::Done(..)) {
                    sub.status = SubStatus::Done(final_verdict, final_score);
                }
                if self
                    .subs
                    .iter()
                    .all(|s| matches!(s.status, SubStatus::Done(..)))
                {
                    self.is_completed = true;
                }
            }
            _ => {}
        }
    }

    pub fn throughput(&self) -> f64 {
        let count = self.recent_done_times.len();
        if count == 0 {
            0.0
        } else {
            let elapsed = self.start_time.elapsed().as_secs_f64().clamp(0.1, 5.0);
            count as f64 / elapsed
        }
    }
}

pub fn run_multi_tui(
    title: &str,
    subs: &[SubmissionMeta],
    slot_count: usize,
    cpu_cores: Vec<Option<usize>>,
    rx: Receiver<PoolEvent>,
    cancel: CancelToken,
    auto_exit_on_done: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let _guard = TerminalGuard::enter()?;

    let backend = CrosstermBackend::new(io::stderr());
    let mut terminal = Terminal::new(backend)?;

    let mut app = MultiApp::new(title, subs, slot_count, cpu_cores, cancel);
    let mut exit_time: Option<Instant> = None;

    loop {
        // Refresh system monitor periodically
        app.sys_monitor.tick();

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

        if (app.is_completed || disconnected) && auto_exit_on_done && exit_time.is_none() {
            exit_time = Some(Instant::now() + Duration::from_millis(600));
        }

        if exit_time.is_some_and(|t| Instant::now() >= t) {
            break;
        }

        terminal.draw(|f| {
            draw_multi_ui(f, &mut app);
        })?;

        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind == crossterm::event::KeyEventKind::Release {
                continue;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                app.cancel_token.cancel();
                break;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Char('Q') => {
                    app.cancel_token.cancel();
                    break;
                }
                KeyCode::Esc | KeyCode::Backspace => {
                    if app.fullscreen_sub_idx.is_some() {
                        app.fullscreen_sub_idx = None;
                    } else {
                        break;
                    }
                }
                KeyCode::Enter => {
                    if app.fullscreen_sub_idx.is_some() {
                        if app
                            .entered_fullscreen_at
                            .is_some_and(|t| t.elapsed() > Duration::from_millis(250))
                        {
                            app.fullscreen_sub_idx = None;
                        }
                    } else if !app.subs.is_empty() {
                        app.fullscreen_sub_idx = Some(app.selected_sub_idx);
                        app.entered_fullscreen_at = Some(Instant::now());
                        app.selected_test_idx = 0;
                        app.test_scroll_offset.set(0);
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if app.fullscreen_sub_idx.is_some() {
                        if app.selected_test_idx > 0 {
                            app.selected_test_idx -= 1;
                        }
                    } else if app.selected_sub_idx > 0 {
                        app.selected_sub_idx -= 1;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if let Some(fs_idx) = app.fullscreen_sub_idx {
                        let total_t = app.subs[fs_idx].tests.len();
                        if app.selected_test_idx + 1 < total_t {
                            app.selected_test_idx += 1;
                        }
                    } else if app.selected_sub_idx + 1 < app.subs.len() {
                        app.selected_sub_idx += 1;
                    }
                }
                KeyCode::PageUp => {
                    if app.fullscreen_sub_idx.is_some() {
                        app.selected_test_idx = app.selected_test_idx.saturating_sub(10);
                    } else {
                        app.selected_sub_idx = app.selected_sub_idx.saturating_sub(10);
                    }
                }
                KeyCode::PageDown => {
                    if let Some(fs_idx) = app.fullscreen_sub_idx {
                        let total_t = app.subs[fs_idx].tests.len();
                        app.selected_test_idx =
                            (app.selected_test_idx + 10).min(total_t.saturating_sub(1));
                    } else {
                        let total_s = app.subs.len();
                        app.selected_sub_idx =
                            (app.selected_sub_idx + 10).min(total_s.saturating_sub(1));
                    }
                }
                _ => {}
            }
        }
    }

    Ok(())
}

fn draw_multi_ui(f: &mut Frame, app: &mut MultiApp) {
    let area = f.area();

    // If fullscreen mode is active for selected submission:
    if let Some(fs_idx) = app.fullscreen_sub_idx
        && fs_idx < app.subs.len()
    {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Header stats
                Constraint::Min(3),    // Tests table
                Constraint::Length(1), // Footer help & CPU/Mem stats
            ])
            .split(area);

        let sub = &app.subs[fs_idx];
        draw_fullscreen_header(f, sub, chunks[0]);
        draw_tests_table(
            f,
            &sub.tests,
            app.selected_test_idx,
            &app.test_scroll_offset,
            chunks[1],
        );
        draw_fullscreen_footer(f, &app.sys_monitor, chunks[2]);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header stats
            Constraint::Min(8),    // Body: Submissions (left) + Slots (right)
            Constraint::Length(4), // Selected submission tests chip preview
            Constraint::Length(1), // Footer keybindings & CPU/Mem stats
        ])
        .split(area);

    // 1. Header Bar
    draw_header(f, app, chunks[0]);

    // 2. Middle Body
    if area.width < 100 {
        draw_submissions_list(f, app, chunks[1]);
    } else {
        let body_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(62), // Submissions list
                Constraint::Percentage(38), // Slots list
            ])
            .split(chunks[1]);

        draw_submissions_list(f, app, body_chunks[0]);
        draw_slots_list(f, app, body_chunks[1]);
    }

    // 3. Detail Preview (bottom chip view)
    draw_detail_preview(f, app, chunks[2]);

    // 4. Footer Help & System monitor
    draw_footer(f, &app.sys_monitor, chunks[3]);
}

fn draw_header(f: &mut Frame, app: &MultiApp, area: Rect) {
    let elapsed = app.start_time.elapsed().as_secs();
    let mins = elapsed / 60;
    let secs = elapsed % 60;

    let done_subs = app
        .subs
        .iter()
        .filter(|s| {
            matches!(s.status, SubStatus::Done(..))
                || (!s.tests.is_empty()
                    && s.tests
                        .iter()
                        .all(|(_, st)| matches!(st, TestCaseStatus::Done(..))))
        })
        .count();
    let total_subs = app.subs.len();

    let busy_slots = app.slots.iter().filter(|s| s.is_some()).count();
    let total_slots = app.slots.len();

    let text = Line::from(vec![
        Span::styled(
            format!("Submissions {}/{} ", done_subs, total_subs),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled("| ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("Tests {}/{} ", app.completed_tests, app.total_tests),
            Style::default().fg(Color::Cyan),
        ),
        Span::styled("| ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{:.0} tests/s ", app.throughput()),
            Style::default().fg(Color::Green),
        ),
        Span::styled("| ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("Slots {}/{} busy ", busy_slots, total_slots),
            Style::default().fg(Color::Yellow),
        ),
        Span::styled("| ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("Elapsed {:02}:{:02}", mins, secs),
            Style::default().fg(Color::White),
        ),
    ]);

    let block = Block::default()
        .title(format!(" uj {} ", app.title))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan));

    let p = Paragraph::new(text).block(block);
    f.render_widget(p, area);
}

fn draw_submissions_list(f: &mut Frame, app: &MultiApp, area: Rect) {
    let header = Row::new(vec![
        TableCell::from("Status").style(Style::default().add_modifier(Modifier::BOLD)),
        TableCell::from("Contestant").style(Style::default().add_modifier(Modifier::BOLD)),
        TableCell::from("Problem").style(Style::default().add_modifier(Modifier::BOLD)),
        TableCell::from("Time").style(Style::default().add_modifier(Modifier::BOLD)),
        TableCell::from("Memory").style(Style::default().add_modifier(Modifier::BOLD)),
        TableCell::from("Score").style(Style::default().add_modifier(Modifier::BOLD)),
        TableCell::from("Progress").style(Style::default().add_modifier(Modifier::BOLD)),
        TableCell::from("Done").style(Style::default().add_modifier(Modifier::BOLD)),
    ])
    .style(Style::default().fg(Color::DarkGray));

    let visible_count = (area.height as usize).saturating_sub(4).max(1);
    let mut offset = app.sub_scroll_offset.get();
    if app.selected_sub_idx < offset {
        offset = app.selected_sub_idx;
    } else if app.selected_sub_idx >= offset + visible_count {
        offset = app.selected_sub_idx + 1 - visible_count;
    }
    app.sub_scroll_offset.set(offset);

    let rows: Vec<Row> = app
        .subs
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible_count)
        .map(|(idx, sub)| {
            let total_t = sub.tests.len();
            let done_t = sub
                .tests
                .iter()
                .filter(|(_, st)| matches!(st, TestCaseStatus::Done(..)))
                .count();

            let (status_cell, verd_color) = match sub.status {
                SubStatus::Pending => (
                    TableCell::from(Span::styled("PEND", Style::default().fg(Color::DarkGray))),
                    Color::DarkGray,
                ),
                SubStatus::Running => {
                    if done_t == total_t && total_t > 0 {
                        let mut agg_verdict = Verdict::Accepted;
                        for (_, st) in &sub.tests {
                            if let TestCaseStatus::Done(r) = st {
                                agg_verdict = agg_verdict.merge(r.verdict);
                            }
                        }
                        let color = match agg_verdict {
                            Verdict::Accepted => Color::Green,
                            Verdict::WrongAnswer => Color::Red,
                            Verdict::TimeLimitExceeded => Color::Yellow,
                            Verdict::MemoryLimitExceeded => Color::Magenta,
                            _ => Color::LightRed,
                        };
                        (
                            TableCell::from(Span::styled(
                                format!("{:<4}", agg_verdict.short_code()),
                                Style::default().fg(color).add_modifier(Modifier::BOLD),
                            )),
                            color,
                        )
                    } else {
                        (
                            TableCell::from(Span::styled(
                                "RUN ",
                                Style::default()
                                    .fg(Color::Cyan)
                                    .add_modifier(Modifier::BOLD),
                            )),
                            Color::Cyan,
                        )
                    }
                }
                SubStatus::Done(v, _) => {
                    let color = match v {
                        Verdict::Accepted => Color::Green,
                        Verdict::WrongAnswer => Color::Red,
                        Verdict::TimeLimitExceeded => Color::Yellow,
                        Verdict::MemoryLimitExceeded => Color::Magenta,
                        _ => Color::LightRed,
                    };
                    (
                        TableCell::from(Span::styled(
                            format!("{:<4}", v.short_code()),
                            Style::default().fg(color).add_modifier(Modifier::BOLD),
                        )),
                        color,
                    )
                }
            };

            let time_str = format_time_ms(sub.max_time_ms);
            let mem_str = format_memory_kb(sub.max_memory_kb);
            let score_str = match sub.score {
                Some(s) => format!("{:.1}", s),
                None => match sub.status {
                    SubStatus::Done(_, s) => format!("{:.1}", s),
                    _ => {
                        if done_t == total_t && total_t > 0 {
                            let total_score: f64 = sub
                                .tests
                                .iter()
                                .filter_map(|(_, st)| {
                                    if let TestCaseStatus::Done(r) = st {
                                        Some(r.score)
                                    } else {
                                        None
                                    }
                                })
                                .sum();
                            format!("{:.1}", total_score)
                        } else {
                            "-".to_string()
                        }
                    }
                },
            };

            // Progress bar
            let bar_len = 8;
            let filled = (done_t * bar_len).checked_div(total_t).unwrap_or(bar_len);
            let bar_str = format!("[{}{}]", "█".repeat(filled), "░".repeat(bar_len - filled));
            let bar_cell = TableCell::from(Span::styled(bar_str, Style::default().fg(verd_color)));

            let done_str = format!("{}/{}", done_t, total_t);

            let row_style = if idx == app.selected_sub_idx {
                Style::default()
                    .bg(Color::Rgb(40, 50, 75))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            Row::new(vec![
                status_cell,
                TableCell::from(sub.meta.name.as_str()),
                TableCell::from(sub.meta.problem.as_str()),
                TableCell::from(Span::styled(time_str, Style::default().fg(Color::Cyan))),
                TableCell::from(Span::styled(mem_str, Style::default().fg(Color::Magenta))),
                TableCell::from(Span::styled(score_str, Style::default().fg(Color::Yellow))),
                bar_cell,
                TableCell::from(done_str),
            ])
            .style(row_style)
        })
        .collect();

    let widths = [
        Constraint::Length(6),
        Constraint::Min(10),
        Constraint::Min(8),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(6),
        Constraint::Length(10),
        Constraint::Length(7),
    ];

    let title = format!(
        " Submissions ({}/{}) ",
        app.selected_sub_idx + 1,
        app.subs.len()
    );
    let table = Table::new(rows, widths).header(header).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray)),
    );

    f.render_widget(table, area);
}

fn draw_slots_list(f: &mut Frame, app: &MultiApp, area: Rect) {
    let now = Instant::now();
    let rows: Vec<Row> = app
        .slots
        .iter()
        .enumerate()
        .map(|(slot_id, slot_info)| {
            let cpu_str = app
                .cpu_cores
                .get(slot_id)
                .and_then(|c| *c)
                .map(|c| format!("(cpu {c})"))
                .unwrap_or_default();

            let label = format!("#{slot_id} {:<8}", cpu_str);

            let (desc, time_str, color) = match slot_info {
                Some((sub_id, test_id, start)) => {
                    let sub_name = app
                        .subs
                        .iter()
                        .find(|s| s.meta.id == *sub_id)
                        .map(|s| format!("{}/{}", s.meta.name, s.meta.problem))
                        .unwrap_or_else(|| format!("sub #{sub_id}"));
                    let elapsed = now.duration_since(*start).as_secs_f64();
                    (
                        format!("{sub_name} #{test_id}"),
                        format!("{:.2}s", elapsed),
                        Color::Cyan,
                    )
                }
                None => ("idle".to_string(), "".to_string(), Color::DarkGray),
            };

            Row::new(vec![
                TableCell::from(Span::styled(label, Style::default().fg(Color::White))),
                TableCell::from(Span::styled(desc, Style::default().fg(color))),
                TableCell::from(Span::styled(time_str, Style::default().fg(Color::Yellow))),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(16),
        Constraint::Min(16),
        Constraint::Length(8),
    ];

    let table = Table::new(rows, widths).block(
        Block::default()
            .title(format!(" Slots ({}) ", app.slots.len()))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray)),
    );

    f.render_widget(table, area);
}

fn draw_detail_preview(f: &mut Frame, app: &MultiApp, area: Rect) {
    if app.subs.is_empty() {
        return;
    }

    let sub = &app.subs[app.selected_sub_idx];
    let total_t = sub.tests.len();
    let done_t = sub
        .tests
        .iter()
        .filter(|(_, st)| matches!(st, TestCaseStatus::Done(..)))
        .count();

    let time_str = format_time_ms(sub.max_time_ms);
    let mem_str = format_memory_kb(sub.max_memory_kb);
    let score_info = match sub.status {
        SubStatus::Done(v, s) => format!("Score: {:.1} ({}) | ", s, v.short_code()),
        _ => String::new(),
    };
    let title = format!(
        " {} / {} — {}{}/{} done | Max: {}, {} ",
        sub.meta.name, sub.meta.problem, score_info, done_t, total_t, time_str, mem_str
    );

    let mut spans = Vec::new();
    for (i, (test_name, status)) in sub.tests.iter().enumerate().take(12) {
        let (label, color) = match status {
            TestCaseStatus::Pending => ("PEND".to_string(), Color::DarkGray),
            TestCaseStatus::Running => ("RUN".to_string(), Color::Cyan),
            TestCaseStatus::Done(res) => (
                format!("{} {}ms", res.verdict.short_code(), res.time_ms),
                match res.verdict {
                    Verdict::Accepted => Color::Green,
                    Verdict::WrongAnswer => Color::Red,
                    Verdict::TimeLimitExceeded => Color::Yellow,
                    _ => Color::LightRed,
                },
            ),
        };

        spans.push(Span::styled(
            format!("#{test_name} "),
            Style::default().fg(Color::White),
        ));
        spans.push(Span::styled(
            format!("{label}  "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
        if i == 11 && total_t > 12 {
            spans.push(Span::styled("…", Style::default().fg(Color::DarkGray)));
        }
    }

    let p = Paragraph::new(Line::from(spans)).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray)),
    );

    f.render_widget(p, area);
}

fn draw_fullscreen_header(f: &mut Frame, sub: &SubState, area: Rect) {
    let (status_text, status_color) = match sub.status {
        SubStatus::Pending => ("PENDING", Color::DarkGray),
        SubStatus::Running => ("RUNNING", Color::Cyan),
        SubStatus::Done(v, _) => (
            v.short_code(),
            match v {
                Verdict::Accepted => Color::Green,
                Verdict::WrongAnswer => Color::Red,
                Verdict::TimeLimitExceeded => Color::Yellow,
                Verdict::MemoryLimitExceeded => Color::Magenta,
                _ => Color::LightRed,
            },
        ),
    };

    let done_t = sub
        .tests
        .iter()
        .filter(|(_, st)| matches!(st, TestCaseStatus::Done(..)))
        .count();
    let total_t = sub.tests.len();

    let mut spans = vec![
        Span::styled(
            " uj ",
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Contestant: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            &sub.meta.name,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" | Problem: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            &sub.meta.problem,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" | Status: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            status_text,
            Style::default()
                .fg(status_color)
                .add_modifier(Modifier::BOLD),
        ),
    ];

    if let Some(score) = sub.score {
        spans.push(Span::styled(
            " | Score: ",
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::styled(
            format!("{:.1}", score),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    let time_str = format_time_ms(sub.max_time_ms);
    let mem_str = format_memory_kb(sub.max_memory_kb);
    spans.push(Span::styled(
        " | Max Time: ",
        Style::default().fg(Color::DarkGray),
    ));
    spans.push(Span::styled(time_str, Style::default().fg(Color::Cyan)));
    spans.push(Span::styled(
        " | Max Mem: ",
        Style::default().fg(Color::DarkGray),
    ));
    spans.push(Span::styled(mem_str, Style::default().fg(Color::Magenta)));
    spans.push(Span::styled(
        " | Tests: ",
        Style::default().fg(Color::DarkGray),
    ));
    spans.push(Span::styled(
        format!("{}/{}", done_t, total_t),
        Style::default().fg(Color::White),
    ));

    let block = Block::default()
        .title(" Submission Detail ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan));

    f.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

fn draw_fullscreen_footer(f: &mut Frame, sys_monitor: &SystemMonitor, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(40), Constraint::Length(38)])
        .split(area);

    let help = Line::from(vec![
        Span::styled(
            " [Esc/Backspace]",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" Back to Overview  "),
        Span::styled("[↑↓/jk]", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" Select Test  "),
        Span::styled("[PgUp/PgDn]", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" Page  "),
        Span::styled("[q]", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" Cancel"),
    ]);
    f.render_widget(
        Paragraph::new(help).style(Style::default().fg(Color::DarkGray)),
        chunks[0],
    );

    let stats = Line::from(vec![Span::styled(
        sys_monitor.format_stats(),
        Style::default().fg(Color::Cyan),
    )]);
    f.render_widget(
        Paragraph::new(stats)
            .alignment(ratatui::layout::Alignment::Right)
            .style(Style::default().fg(Color::DarkGray)),
        chunks[1],
    );
}

fn draw_footer(f: &mut Frame, sys_monitor: &SystemMonitor, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(40), Constraint::Length(38)])
        .split(area);

    let line = Line::from(vec![
        Span::styled(" ↑↓/jk", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" Select  "),
        Span::styled("PgUp/PgDn", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" Page  "),
        Span::styled("Enter", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" Detail  "),
        Span::styled("Esc", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" Exit  "),
        Span::styled("q", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" Cancel"),
    ]);
    let p = Paragraph::new(line).style(Style::default().fg(Color::DarkGray));
    f.render_widget(p, chunks[0]);

    let stats_line = Line::from(vec![Span::styled(
        sys_monitor.format_stats(),
        Style::default().fg(Color::Cyan),
    )]);
    let stats_p = Paragraph::new(stats_line)
        .alignment(ratatui::layout::Alignment::Right)
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(stats_p, chunks[1]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use uj_report::result::{SingleTestResult, SubmissionRef, TimingInfo};

    #[test]
    fn test_multi_tui_state() {
        let metas = vec![
            SubmissionMeta {
                id: 1,
                problem: "aplusb".to_string(),
                name: "alice".to_string(),
                test_names: vec!["1".to_string(), "2".to_string()],
            },
            SubmissionMeta {
                id: 2,
                problem: "aplusb".to_string(),
                name: "bob".to_string(),
                test_names: vec!["1".to_string(), "2".to_string()],
            },
        ];

        let cancel = CancelToken::new();
        let mut app = MultiApp::new("test_session", &metas, 2, vec![Some(2), Some(4)], cancel);

        assert_eq!(app.subs[0].status, SubStatus::Pending);
        assert_eq!(app.subs[1].status, SubStatus::Pending);

        // 1. TestStart on sub 1, test 1 in slot 0
        app.handle_event(PoolEvent {
            submission: SubmissionRef {
                id: 1,
                problem: "aplusb".to_string(),
                name: "alice".to_string(),
            },
            inner: JudgeEvent::TestStart {
                id: "1".to_string(),
                subtask: None,
                slot: Some(0),
            },
        });

        assert_eq!(app.subs[0].status, SubStatus::Running);
        assert!(app.slots[0].is_some());
        let (s_id, t_id, _) = app.slots[0].as_ref().unwrap();
        assert_eq!(*s_id, 1);
        assert_eq!(t_id, "1");

        // 2. TestDone on sub 1, test 1
        let test_res = SingleTestResult {
            id: "1".to_string(),
            subtask: None,
            verdict: Verdict::Accepted,
            score: 50.0,
            time_ms: 10,
            wall_ms: 12,
            memory_kb: 1024,
            exit: Default::default(),
            message: None,
            checker: None,
            diff: None,
            timing: TimingInfo::default(),
            sandbox_tier: None,
        };

        app.handle_event(PoolEvent {
            submission: SubmissionRef {
                id: 1,
                problem: "aplusb".to_string(),
                name: "alice".to_string(),
            },
            inner: JudgeEvent::TestDone(test_res),
        });

        // Slot 0 freed
        assert!(app.slots[0].is_none());
        assert_eq!(app.completed_tests, 1);
        assert_eq!(app.subs[0].max_time_ms, 10);
        assert_eq!(app.subs[0].max_memory_kb, 1024);

        // 3. SubmissionDone on sub 1
        app.handle_event(PoolEvent {
            submission: SubmissionRef {
                id: 1,
                problem: "aplusb".to_string(),
                name: "alice".to_string(),
            },
            inner: JudgeEvent::Summary {
                total_time_ms: 20,
                peak_memory_kb: 2048,
                final_verdict: Verdict::Accepted,
                final_score: 100.0,
            },
        });

        assert_eq!(
            app.subs[0].status,
            SubStatus::Done(Verdict::Accepted, 100.0)
        );
        assert_eq!(app.subs[0].max_memory_kb, 2048);
        assert_eq!(app.subs[0].score, Some(100.0));
        assert!(!app.is_completed); // sub 2 still pending

        // 4. Sub 2 finishes
        app.handle_event(PoolEvent {
            submission: SubmissionRef {
                id: 2,
                problem: "aplusb".to_string(),
                name: "bob".to_string(),
            },
            inner: JudgeEvent::Summary {
                total_time_ms: 25,
                peak_memory_kb: 1024,
                final_verdict: Verdict::WrongAnswer,
                final_score: 50.0,
            },
        });

        assert_eq!(
            app.subs[1].status,
            SubStatus::Done(Verdict::WrongAnswer, 50.0)
        );
        assert_eq!(app.subs[1].score, Some(50.0));
        assert!(app.is_completed);
    }

    #[test]
    fn test_format_helpers() {
        assert_eq!(format_time_ms(0), "-");
        assert_eq!(format_time_ms(45), "45ms");
        assert_eq!(format_time_ms(1500), "1.50s");

        assert_eq!(format_memory_kb(0), "-");
        assert_eq!(format_memory_kb(512), "512KB");
        assert_eq!(format_memory_kb(2048), "2.0MB");
        assert_eq!(format_memory_kb(2 * 1024 * 1024), "2.00GB");
    }

    #[test]
    fn test_sysinfo_api() {
        let mut sys = sysinfo::System::new();
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        let _cpu = sys.global_cpu_usage();
        let _used = sys.used_memory();
        let _total = sys.total_memory();
    }

    #[test]
    fn test_narrow_terminal_degradation() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let metas = vec![SubmissionMeta {
            id: 1,
            problem: "aplusb".to_string(),
            name: "alice".to_string(),
            test_names: vec!["1".to_string()],
        }];
        let cancel = CancelToken::new();
        let mut app = MultiApp::new("narrow_test", &metas, 2, vec![Some(0), Some(1)], cancel);

        terminal
            .draw(|f| {
                draw_multi_ui(f, &mut app);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(!content.contains("Slots (2)"));
    }
}
