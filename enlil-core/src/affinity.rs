//! CPU affinity — cross-platform abstraction for core pinning.
//!
//! On Linux, uses `sched_setaffinity` / `sched_getaffinity`.
//! On other platforms, provides a no-op implementation that logs warnings.

use std::collections::HashSet;
use std::fmt;

/// Represents a CPU affinity mask — the set of physical cores a thread may run on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AffinityMask {
    cores: HashSet<u32>,
}

impl AffinityMask {
    /// Create an empty mask (no cores).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            cores: HashSet::new(),
        }
    }

    /// Create a mask with a single core.
    #[must_use]
    pub fn single(core: u32) -> Self {
        let mut cores = HashSet::new();
        cores.insert(core);
        Self { cores }
    }

    /// Create a mask from a list of cores.
    #[must_use]
    pub fn from_cores(cores: &[u32]) -> Self {
        Self {
            cores: cores.iter().copied().collect(),
        }
    }

    /// Add a core to the mask.
    pub fn add(&mut self, core: u32) {
        self.cores.insert(core);
    }

    /// Remove a core from the mask.
    pub fn remove(&mut self, core: u32) {
        self.cores.remove(&core);
    }

    /// Check if a core is in the mask.
    #[must_use]
    pub fn contains(&self, core: u32) -> bool {
        self.cores.contains(&core)
    }

    /// Number of cores in the mask.
    #[must_use]
    pub fn count(&self) -> usize {
        self.cores.len()
    }

    /// Whether the mask is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cores.is_empty()
    }

    /// Get the cores as a sorted vec.
    #[must_use]
    pub fn cores(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.cores.iter().copied().collect();
        v.sort_unstable();
        v
    }
}

impl fmt::Display for AffinityMask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cores = self.cores();
        write!(f, "[")?;
        for (i, c) in cores.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{c}")?;
        }
        write!(f, "]")
    }
}

/// Result of a pin operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinResult {
    /// Successfully pinned to the requested core(s).
    Pinned(AffinityMask),
    /// Platform doesn't support pinning; running unpinned.
    Unsupported,
    /// Pinning failed with an error message.
    Failed(String),
}

/// Pin the current thread to a single physical core.
///
/// On Linux, calls `sched_setaffinity`. On other platforms, returns `Unsupported`.
#[must_use]
pub fn pin_current_thread(core: u32) -> PinResult {
    pin_current_thread_to_mask(&AffinityMask::single(core))
}

/// Pin the current thread to a set of cores.
#[must_use]
pub fn pin_current_thread_to_mask(mask: &AffinityMask) -> PinResult {
    if mask.is_empty() {
        return PinResult::Failed("empty affinity mask".into());
    }

    #[cfg(target_os = "linux")]
    {
        pin_linux(mask)
    }

    #[cfg(not(target_os = "linux"))]
    {
        log::warn!(
            "CPU pinning not supported on this platform; requested cores: {mask}"
        );
        PinResult::Unsupported
    }
}

/// Get the affinity mask of the current thread.
#[must_use]
pub fn get_current_affinity() -> PinResult {
    #[cfg(target_os = "linux")]
    {
        get_affinity_linux()
    }

    #[cfg(not(target_os = "linux"))]
    {
        PinResult::Unsupported
    }
}

/// Query the number of online CPUs.
#[must_use]
pub fn online_cpu_count() -> usize {
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
}

/// Get a list of all online CPU IDs (0-based).
#[must_use]
pub fn online_cpus() -> Vec<u32> {
    (0..u32::try_from(online_cpu_count()).unwrap_or(u32::MAX)).collect()
}

// ---------------------------------------------------------------------------
// Linux implementation
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn pin_linux(mask: &AffinityMask) -> PinResult {
    // cpu_set_t is 1024 bits = 128 bytes on Linux
    const CPU_SET_SIZE: usize = 128;
    let mut cpu_set = [0u8; CPU_SET_SIZE];

    for &core in &mask.cores {
        let byte_idx = (core / 8) as usize;
        let bit_idx = core % 8;
        if byte_idx < CPU_SET_SIZE {
            cpu_set[byte_idx] |= 1 << bit_idx;
        }
    }

    let ret = unsafe {
        libc::sched_setaffinity(
            0, // current thread
            CPU_SET_SIZE,
            cpu_set.as_ptr() as *const libc::cpu_set_t,
        )
    };

    if ret == 0 {
        PinResult::Pinned(mask.clone())
    } else {
        PinResult::Failed(format!(
            "sched_setaffinity failed: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(target_os = "linux")]
fn get_affinity_linux() -> PinResult {
    const CPU_SET_SIZE: usize = 128;
    let mut cpu_set = [0u8; CPU_SET_SIZE];

    let ret = unsafe {
        libc::sched_getaffinity(
            0,
            CPU_SET_SIZE,
            cpu_set.as_mut_ptr() as *mut libc::cpu_set_t,
        )
    };

    if ret == 0 {
        let mut mask = AffinityMask::empty();
        for byte_idx in 0..CPU_SET_SIZE {
            for bit_idx in 0..8 {
                if cpu_set[byte_idx] & (1 << bit_idx) != 0 {
                    mask.add((byte_idx * 8 + bit_idx) as u32);
                }
            }
        }
        PinResult::Pinned(mask)
    } else {
        PinResult::Failed(format!(
            "sched_getaffinity failed: {}",
            std::io::Error::last_os_error()
        ))
    }
}

// ---------------------------------------------------------------------------
// vCPU thread launcher — pins and runs a vCPU on a dedicated core
// ---------------------------------------------------------------------------

/// Configuration for launching a vCPU thread.
#[derive(Debug, Clone)]
pub struct VcpuThreadConfig {
    /// Guest identifier.
    pub guest_id: String,
    /// vCPU index within the guest.
    pub vcpu_id: u32,
    /// Physical core to pin to.
    pub physical_core: u32,
}

/// Handle to a running vCPU thread.
pub struct VcpuThread {
    pub config: VcpuThreadConfig,
    pub handle: Option<std::thread::JoinHandle<()>>,
    pub pin_result: PinResult,
}

/// Launch a vCPU thread pinned to a physical core.
///
/// The `run_fn` closure is the vCPU's main loop (`KVM_RUN` loop on Linux,
/// VMLAUNCH loop on bare-metal). It receives the `guest_id` and `vcpu_id`.
///
/// # Panics
///
/// Panics if the vCPU thread fails to spawn.
#[must_use]
pub fn launch_vcpu_thread<F>(config: &VcpuThreadConfig, run_fn: F) -> VcpuThread
where
    F: FnOnce(&str, u32) + Send + 'static,
{
    let cfg = config.clone();
    let guest_id = config.guest_id.clone();
    let vcpu_id = config.vcpu_id;
    let core = config.physical_core;

    let (tx, rx) = std::sync::mpsc::channel();

    let handle = std::thread::Builder::new()
        .name(format!("{guest_id}-vcpu{vcpu_id}"))
        .spawn(move || {
            let result = pin_current_thread(core);
            match &result {
                PinResult::Pinned(mask) => {
                    log::info!(
                        "[{guest_id}/vcpu{vcpu_id}] pinned to core(s): {mask}"
                    );
                }
                PinResult::Unsupported => {
                    log::warn!(
                        "[{guest_id}/vcpu{vcpu_id}] CPU pinning not supported, running unpinned"
                    );
                }
                PinResult::Failed(e) => {
                    log::error!(
                        "[{guest_id}/vcpu{vcpu_id}] failed to pin to core {core}: {e}"
                    );
                }
            }
            let _ = tx.send(result);
            run_fn(&guest_id, vcpu_id);
        })
        .expect("failed to spawn vCPU thread");

    let pin_result = rx.recv().unwrap_or(PinResult::Failed("channel closed".into()));

    VcpuThread {
        config: cfg,
        handle: Some(handle),
        pin_result,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affinity_mask_basics() {
        let mask = AffinityMask::single(3);
        assert!(mask.contains(3));
        assert!(!mask.contains(0));
        assert_eq!(mask.count(), 1);
    }

    #[test]
    fn affinity_mask_from_cores() {
        let mask = AffinityMask::from_cores(&[0, 2, 4, 6]);
        assert_eq!(mask.count(), 4);
        assert!(mask.contains(0));
        assert!(mask.contains(2));
        assert!(!mask.contains(1));
        assert_eq!(mask.cores(), vec![0, 2, 4, 6]);
    }

    #[test]
    fn affinity_mask_add_remove() {
        let mut mask = AffinityMask::empty();
        assert!(mask.is_empty());
        mask.add(5);
        mask.add(10);
        assert_eq!(mask.count(), 2);
        mask.remove(5);
        assert_eq!(mask.count(), 1);
        assert!(!mask.contains(5));
        assert!(mask.contains(10));
    }

    #[test]
    fn affinity_mask_display() {
        let mask = AffinityMask::from_cores(&[3, 1, 7]);
        assert_eq!(format!("{mask}"), "[1, 3, 7]");
    }

    #[test]
    fn pin_empty_mask_fails() {
        let result = pin_current_thread_to_mask(&AffinityMask::empty());
        assert_eq!(result, PinResult::Failed("empty affinity mask".into()));
    }

    #[test]
    fn online_cpus_nonzero() {
        assert!(online_cpu_count() > 0);
        assert!(!online_cpus().is_empty());
    }

    #[test]
    fn launch_vcpu_thread_runs() {
        use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

        let ran = Arc::new(AtomicBool::new(false));
        let ran2 = ran.clone();

        let config = VcpuThreadConfig {
            guest_id: "test".into(),
            vcpu_id: 0,
            physical_core: 0,
        };

        let mut thread = launch_vcpu_thread(
            &config,
            move |guest_id, vcpu_id| {
                assert_eq!(guest_id, "test");
                assert_eq!(vcpu_id, 0);
                ran2.store(true, Ordering::SeqCst);
            },
        );

        if let Some(h) = thread.handle.take() {
            h.join().unwrap();
        }
        assert!(ran.load(Ordering::SeqCst));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn pin_unsupported_on_non_linux() {
        let result = pin_current_thread(0);
        assert_eq!(result, PinResult::Unsupported);
    }
}
