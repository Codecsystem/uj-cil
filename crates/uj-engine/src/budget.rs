//! Memory budget management and FIFO ticket-based allocation.

use std::collections::HashSet;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};
use uj_core::config::{ProblemConfig, ProblemType};
use uj_core::run::Limits;
use uj_sandbox::CancelToken;

#[derive(Default)]
struct BudgetState {
    used: u64,
    peak: u64,
    next_ticket: u64,
    serving: u64,
    cancelled_tickets: HashSet<u64>,
}

impl BudgetState {
    fn advance_serving(&mut self) {
        self.serving += 1;
        while self.cancelled_tickets.remove(&self.serving) {
            self.serving += 1;
        }
    }
}

/// FIFO memory budget that prevents starvation of large memory requests.
pub struct MemoryBudget {
    total: u64,
    state: Mutex<BudgetState>,
    cv: Condvar,
}

impl MemoryBudget {
    pub fn new(total: u64) -> Self {
        Self {
            total,
            state: Mutex::new(BudgetState::default()),
            cv: Condvar::new(),
        }
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn used(&self) -> u64 {
        self.state.lock().used
    }

    pub fn peak(&self) -> u64 {
        self.state.lock().peak
    }

    /// Block until memory budget is acquired.
    /// If `want` exceeds total budget, it is capped to total (running exclusively).
    /// Checks cancellation every 100ms; returns None if cancelled.
    pub fn acquire(&self, want: u64, cancel: &CancelToken) -> Option<BudgetLease<'_>> {
        let effective_want = if self.total == 0 {
            0
        } else {
            want.min(self.total)
        };

        let my_ticket = {
            let mut st = self.state.lock();
            let ticket = st.next_ticket;
            st.next_ticket += 1;
            ticket
        };

        let mut st = self.state.lock();
        loop {
            if cancel.is_cancelled() {
                if st.serving == my_ticket {
                    st.advance_serving();
                    self.cv.notify_all();
                } else {
                    st.cancelled_tickets.insert(my_ticket);
                }
                return None;
            }

            // It is our turn if serving == my_ticket and we can fit into total budget.
            if st.serving == my_ticket && st.used + effective_want <= self.total {
                st.used += effective_want;
                st.peak = st.peak.max(st.used);
                st.advance_serving();
                self.cv.notify_all();
                return Some(BudgetLease {
                    budget: self,
                    amount: effective_want,
                });
            }

            self.cv.wait_for(&mut st, Duration::from_millis(100));
        }
    }
}

/// RAII lease that returns acquired memory budget upon drop.
pub struct BudgetLease<'a> {
    budget: &'a MemoryBudget,
    amount: u64,
}

impl BudgetLease<'_> {
    pub fn amount(&self) -> u64 {
        self.amount
    }
}

impl Drop for BudgetLease<'_> {
    fn drop(&mut self) {
        let mut st = self.budget.state.lock();
        st.used = st.used.saturating_sub(self.amount);
        self.budget.cv.notify_all();
    }
}

/// Compute memory budget required for a test case based on problem type and limits.
pub fn task_memory_cost(config: &ProblemConfig, limits: &Limits) -> u64 {
    let base = limits.memory_bytes;
    if base == 0 {
        return 0;
    }
    match config.problem_type {
        ProblemType::Communication => base.saturating_mul(config.processes.unwrap_or(1) as u64),
        _ => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    #[test]
    fn test_budget_within_limit() {
        let budget = Arc::new(MemoryBudget::new(1000));
        let cancel = CancelToken::new();

        let l1 = budget.acquire(400, &cancel).unwrap();
        assert_eq!(budget.used(), 400);

        let l2 = budget.acquire(400, &cancel).unwrap();
        assert_eq!(budget.used(), 800);

        drop(l1);
        assert_eq!(budget.used(), 400);

        drop(l2);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn test_budget_fifo_order() {
        let budget = Arc::new(MemoryBudget::new(100));
        let cancel = CancelToken::new();

        // l1 takes 80
        let l1 = budget.acquire(80, &cancel).unwrap();

        let order = Arc::new(parking_lot::Mutex::new(Vec::new()));

        // T1 requests 50 (must wait for l1 to release, needs 80 freed)
        let b1 = Arc::clone(&budget);
        let c1 = cancel.clone();
        let o1 = Arc::clone(&order);
        let h1 = thread::spawn(move || {
            let lease = b1.acquire(50, &c1).unwrap();
            o1.lock().push(1);
            drop(lease);
        });

        // Sleep briefly to ensure T1 is in queue before T2
        thread::sleep(Duration::from_millis(50));

        // T2 requests 10 (could theoretically fit if not for T1 in front!)
        let b2 = Arc::clone(&budget);
        let c2 = cancel.clone();
        let o2 = Arc::clone(&order);
        let h2 = thread::spawn(move || {
            let lease = b2.acquire(10, &c2).unwrap();
            o2.lock().push(2);
            drop(lease);
        });

        thread::sleep(Duration::from_millis(50));
        // Drop l1 so T1 can proceed
        drop(l1);

        h1.join().unwrap();
        h2.join().unwrap();

        assert_eq!(*order.lock(), vec![1, 2]);
    }

    #[test]
    fn test_budget_oversized_exclusive() {
        let budget = Arc::new(MemoryBudget::new(100));
        let cancel = CancelToken::new();

        // First hold 50
        let l1 = budget.acquire(50, &cancel).unwrap();

        let oversized_done = Arc::new(AtomicUsize::new(0));
        let b_clone = Arc::clone(&budget);
        let c_clone = cancel.clone();
        let done_clone = Arc::clone(&oversized_done);

        let h = thread::spawn(move || {
            // Want 500 (> 100 total), should acquire 100 exclusively
            let lease = b_clone.acquire(500, &c_clone).unwrap();
            assert_eq!(lease.amount(), 100);
            done_clone.store(1, Ordering::SeqCst);
            drop(lease);
        });

        thread::sleep(Duration::from_millis(50));
        assert_eq!(oversized_done.load(Ordering::SeqCst), 0);

        // Drop l1, freeing all memory
        drop(l1);

        h.join().unwrap();
        assert_eq!(oversized_done.load(Ordering::SeqCst), 1);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn test_budget_cancellation() {
        let budget = Arc::new(MemoryBudget::new(100));
        let cancel = CancelToken::new();

        let l1 = budget.acquire(100, &cancel).unwrap();

        let b_clone = Arc::clone(&budget);
        let c_clone = CancelToken::new();
        let c_ref = c_clone.clone();

        let h = thread::spawn(move || {
            let res = b_clone.acquire(50, &c_ref);
            assert!(res.is_none());
        });

        thread::sleep(Duration::from_millis(50));
        c_clone.cancel();
        h.join().unwrap();

        // Ensure next task can still acquire after cancelled task
        drop(l1);
        let l2 = budget.acquire(50, &cancel);
        assert!(l2.is_some());
    }

    #[test]
    fn test_budget_peak_never_exceeds_total() {
        let total = 1000u64;
        let budget = Arc::new(MemoryBudget::new(total));
        let cancel = CancelToken::new();

        let mut handles = Vec::new();
        for seed in 0..16u64 {
            let b = Arc::clone(&budget);
            let c = cancel.clone();
            handles.push(thread::spawn(move || {
                let mut rng = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                for _ in 0..50 {
                    rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let want = 1 + (rng % total);
                    let lease = b.acquire(want, &c).unwrap();
                    let sleep_us = (rng >> 16) % 2000;
                    if sleep_us > 0 {
                        thread::sleep(Duration::from_micros(sleep_us));
                    }
                    drop(lease);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert!(
            budget.peak() <= budget.total(),
            "Peak {} exceeded total {}",
            budget.peak(),
            budget.total()
        );
        assert_eq!(budget.used(), 0);
    }
}
