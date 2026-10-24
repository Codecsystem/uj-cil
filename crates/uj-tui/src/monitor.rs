//! System monitor for real-time CPU and Memory tracking of the uj process tree.

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded, unbounded};
use std::collections::HashSet;
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
struct MonitorSnapshot {
    proc_cpu_norm: f32,
    proc_cpu_raw: f32,
    proc_mem_bytes: u64,
    main_mem_bytes: u64,
    proc_count: usize,
}

/// Collects a lightweight process-tree snapshot querying only CPU and memory (skipping expensive
/// per-process exe/cmdline/cwd/environ/disk/open-file queries).
fn collect_snapshot(sys: &mut sysinfo::System, logical_cores: usize) -> MonitorSnapshot {
    let refresh_kind = sysinfo::ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory();
    sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, refresh_kind);
    let my_pid = sysinfo::get_current_pid().ok();

    let mut total_cpu = 0.0f32;
    let mut total_mem = 0u64;
    let mut main_mem = 0u64;
    let mut count = 1;

    if let Some(my_pid) = my_pid {
        let mut pids = HashSet::new();
        pids.insert(my_pid);

        let mut changed = true;
        while changed {
            changed = false;
            for (pid, proc) in sys.processes() {
                if let Some(parent) = proc.parent()
                    && pids.contains(&parent)
                    && pids.insert(*pid)
                {
                    changed = true;
                }
            }
        }
        count = pids.len();

        for pid in &pids {
            if let Some(proc) = sys.process(*pid) {
                total_cpu += proc.cpu_usage();
                total_mem += proc.memory();
                if *pid == my_pid {
                    main_mem = proc.memory();
                }
            }
        }
    }

    MonitorSnapshot {
        proc_cpu_raw: total_cpu,
        proc_cpu_norm: (total_cpu / logical_cores as f32).min(100.0),
        proc_mem_bytes: total_mem,
        main_mem_bytes: main_mem,
        proc_count: count,
    }
}

/// Periodic process tree resource monitor based on `sysinfo`.
///
/// Periodic refreshes run on a dedicated background `std::thread` so the UI render loop never
/// blocks on OS process enumeration (`refresh_processes`).
pub struct SystemMonitor {
    pub logical_cores: usize,
    snap_rx: Receiver<MonitorSnapshot>,
    _stop_tx: Sender<()>,
    pub proc_cpu_norm: f32,
    pub proc_cpu_raw: f32,
    pub proc_mem_bytes: u64,
    pub main_mem_bytes: u64,
    pub proc_count: usize,
}

impl Default for SystemMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemMonitor {
    pub fn new() -> Self {
        let mut sys = sysinfo::System::new();
        let logical_cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .max(1);
        let initial = collect_snapshot(&mut sys, logical_cores);

        let (snap_tx, snap_rx) = unbounded();
        let (stop_tx, stop_rx) = bounded::<()>(1);

        let _ = std::thread::Builder::new()
            .name("uj-tui-sysmon".to_string())
            .spawn(move || {
                loop {
                    match stop_rx.recv_timeout(Duration::from_millis(750)) {
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {
                            let snap = collect_snapshot(&mut sys, logical_cores);
                            if snap_tx.send(snap).is_err() {
                                break;
                            }
                        }
                    }
                }
            });

        Self {
            logical_cores,
            snap_rx,
            _stop_tx: stop_tx,
            proc_cpu_norm: initial.proc_cpu_norm,
            proc_cpu_raw: initial.proc_cpu_raw,
            proc_mem_bytes: initial.proc_mem_bytes,
            main_mem_bytes: initial.main_mem_bytes,
            proc_count: initial.proc_count,
        }
    }

    /// Drains the latest process-tree stats snapshot produced by the background monitor thread
    /// without blocking the UI thread.
    pub fn tick(&mut self) {
        while let Ok(snap) = self.snap_rx.try_recv() {
            self.proc_cpu_raw = snap.proc_cpu_raw;
            self.proc_cpu_norm = snap.proc_cpu_norm;
            self.proc_mem_bytes = snap.proc_mem_bytes;
            self.main_mem_bytes = snap.main_mem_bytes;
            self.proc_count = snap.proc_count;
        }
    }

    /// Format CPU and memory stats for single-line footer display.
    pub fn format_stats(&self) -> String {
        let mem_str = if self.proc_mem_bytes >= 1024 * 1024 * 1024 {
            format!(
                "{:.2}GB",
                self.proc_mem_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
            )
        } else if self.proc_mem_bytes >= 1024 * 1024 {
            format!("{:.1}MB", self.proc_mem_bytes as f64 / (1024.0 * 1024.0))
        } else {
            format!("{}KB", self.proc_mem_bytes / 1024)
        };

        format!("uj CPU: {:>4.1}% | Mem: {}", self.proc_cpu_norm, mem_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_tree_metrics() {
        let mut monitor = SystemMonitor::new();
        println!("logical_cores: {}", monitor.logical_cores);
        monitor.tick();
        let stats = monitor.format_stats();
        println!("stats: {stats}");
        assert!(stats.starts_with("uj CPU:"));
        assert!(monitor.proc_mem_bytes > 0);
    }
}
