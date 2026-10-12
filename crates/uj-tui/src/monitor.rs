//! System monitor for real-time CPU and Memory tracking of the uj process tree.

use std::collections::HashSet;
use std::time::{Duration, Instant};

/// Periodic process tree resource monitor based on `sysinfo`.
pub struct SystemMonitor {
    sys: sysinfo::System,
    logical_cores: usize,
    last_refresh: Instant,
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
        let sys = sysinfo::System::new();
        let logical_cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .max(1);
        let mut monitor = Self {
            sys,
            logical_cores,
            last_refresh: Instant::now() - Duration::from_secs(1),
            proc_cpu_norm: 0.0,
            proc_cpu_raw: 0.0,
            proc_mem_bytes: 0,
            main_mem_bytes: 0,
            proc_count: 1,
        };
        monitor.tick();
        monitor
    }

    /// Refresh process tree stats if sufficient time has elapsed since last refresh (750ms).
    pub fn tick(&mut self) {
        if self.last_refresh.elapsed() < Duration::from_millis(750) {
            return;
        }

        self.sys
            .refresh_processes(sysinfo::ProcessesToUpdate::All, true);
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
                for (pid, proc) in self.sys.processes() {
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
                if let Some(proc) = self.sys.process(*pid) {
                    total_cpu += proc.cpu_usage();
                    total_mem += proc.memory();
                    if *pid == my_pid {
                        main_mem = proc.memory();
                    }
                }
            }
        }

        self.proc_cpu_raw = total_cpu;
        self.proc_cpu_norm = (total_cpu / self.logical_cores as f32).min(100.0);
        self.proc_mem_bytes = total_mem;
        self.main_mem_bytes = main_mem;
        self.proc_count = count;
        self.last_refresh = Instant::now();
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
