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
///
/// When `full_scan` is false and `tracked_pids` is non-empty, only the known `uj` process tree PIDs
/// are refreshed in the OS instead of enumerating every process on the system.
fn collect_snapshot(
    sys: &mut sysinfo::System,
    logical_cores: usize,
    tracked_pids: &mut Vec<sysinfo::Pid>,
    full_scan: bool,
) -> MonitorSnapshot {
    let refresh_kind = sysinfo::ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory();
    let my_pid = sysinfo::get_current_pid().ok();

    if full_scan || tracked_pids.is_empty() {
        sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, refresh_kind);
        tracked_pids.clear();
        if let Some(root_pid) = my_pid {
            let mut pids = HashSet::new();
            pids.insert(root_pid);
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
            tracked_pids.extend(pids);
        }
    } else {
        sys.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(tracked_pids.as_slice()),
            true,
            refresh_kind,
        );
        tracked_pids.retain(|pid| sys.process(*pid).is_some());
    }

    let mut total_cpu = 0.0f32;
    let mut total_mem = 0u64;
    let mut main_mem = 0u64;
    let count = tracked_pids.len().max(1);

    for pid in tracked_pids.iter() {
        if let Some(proc) = sys.process(*pid) {
            total_cpu += proc.cpu_usage();
            total_mem += proc.memory();
            if Some(*pid) == my_pid {
                main_mem = proc.memory();
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
        let mut tracked_pids = Vec::new();
        let initial = collect_snapshot(&mut sys, logical_cores, &mut tracked_pids, true);

        let (snap_tx, snap_rx) = unbounded();
        let (stop_tx, stop_rx) = bounded::<()>(1);

        let _ = std::thread::Builder::new()
            .name("uj-tui-sysmon".to_string())
            .spawn(move || {
                let mut tick_idx: u64 = 0;
                loop {
                    match stop_rx.recv_timeout(Duration::from_millis(750)) {
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {
                            tick_idx = tick_idx.wrapping_add(1);
                            // Full child-tree discovery every 3rd tick (~2.25s); targeted PID refresh in between.
                            let full_scan = tick_idx.is_multiple_of(3);
                            let snap = collect_snapshot(
                                &mut sys,
                                logical_cores,
                                &mut tracked_pids,
                                full_scan,
                            );
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
    /// without blocking the UI thread. Returns `true` if new stats arrived.
    pub fn tick(&mut self) -> bool {
        let mut updated = false;
        while let Ok(snap) = self.snap_rx.try_recv() {
            self.proc_cpu_raw = snap.proc_cpu_raw;
            self.proc_cpu_norm = snap.proc_cpu_norm;
            self.proc_mem_bytes = snap.proc_mem_bytes;
            self.main_mem_bytes = snap.main_mem_bytes;
            self.proc_count = snap.proc_count;
            updated = true;
        }
        updated
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
