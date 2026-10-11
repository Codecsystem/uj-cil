//! CPU topology detection and core slot allocation.

#[derive(Debug, Clone)]
pub struct Slot {
    pub id: usize,
    pub cpu_core: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Topology {
    pub physical_cores: usize,
    pub logical_cores: usize,
    pub slots: Vec<Slot>,
}

impl Topology {
    /// Detect host CPU topology and compute optimal judging slots.
    ///
    /// Respects the constraint:
    /// - 1 physical core reserved for uj scheduler & I/O
    /// - slots = min(physical_cores - 1, floor(available_mem * 0.8 / memory_limit))
    /// - slots minimum is always 1.
    pub fn detect(memory_limit_bytes: u64, user_jobs: Option<usize>) -> Self {
        let logical_cores = num_cpus::get();
        let p_cores = uj_sandbox::detect_p_core_primary_cpus();
        let physical_p_cores = p_cores.len().max(1);

        // Reserve p_cores[0] for uj, allocate worker cores from p_cores[1..]
        let available_worker_cores = if p_cores.len() > 1 {
            p_cores[1..].to_vec()
        } else {
            p_cores.clone()
        };

        let default_slots = available_worker_cores.len().max(1);

        // Determine available memory constraint
        let memory_constrained_slots = if memory_limit_bytes == 0 {
            default_slots
        } else {
            let mut sys = sysinfo::System::new();
            sys.refresh_memory();
            let available_mem = sys.available_memory(); // in bytes
            let usable_mem = (available_mem as f64 * 0.8) as u64;
            let count = usable_mem.checked_div(memory_limit_bytes).unwrap_or(0);
            (count as usize).max(1)
        };

        let calculated_slots = default_slots.min(memory_constrained_slots);
        if let Some(uj) = user_jobs
            && uj > available_worker_cores.len()
        {
            eprintln!(
                "Notice: requested jobs ({uj}) exceeds available physical P-cores ({}); CPU cores will be shared.",
                available_worker_cores.len()
            );
        }
        let final_count = user_jobs.unwrap_or(calculated_slots).max(1);

        let mut slots = Vec::with_capacity(final_count);
        for id in 0..final_count {
            let cpu_core = Some(available_worker_cores[id % available_worker_cores.len()]);
            slots.push(Slot { id, cpu_core });
        }

        Self {
            physical_cores: physical_p_cores,
            logical_cores,
            slots,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_topology_detection() {
        let topo = Topology::detect(256 * 1024 * 1024, None);
        println!(
            "Topology: p_cores={}, logical={}, slots={:?}",
            topo.physical_cores, topo.logical_cores, topo.slots
        );
        assert!(topo.physical_cores >= 1);
        assert!(topo.logical_cores >= 1);
        assert!(!topo.slots.is_empty());
    }

    #[test]
    fn test_user_jobs_override() {
        let topo = Topology::detect(256 * 1024 * 1024, Some(4));
        assert_eq!(topo.slots.len(), 4);
    }
}
