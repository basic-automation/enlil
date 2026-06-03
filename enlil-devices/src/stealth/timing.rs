//! Timing stealth — TSC offsetting, APERF/MPERF shadow counters
//!
//! Critical for defeating IET (Instruction Execution Time) divergence tests.
//! Anti-cheat IET tests compare CPUID execution time against a slow reference
//! instruction using IA32_APERF instead of TSC.

/// Per-vCPU timing stealth state
#[derive(Debug, Clone)]
pub struct TimingStealth {
    /// TSC offset applied in VMCS/VMCB to hide VMEXIT overhead
    tsc_offset: i64,
    /// Cumulative TSC cycles spent in VMEXITs (host-side)
    cumulative_exit_tsc: u64,
    /// Shadow APERF counter (actual perf cycles, excluding VMEXIT time)
    shadow_aperf: u64,
    /// Shadow MPERF counter (max perf cycles, excluding VMEXIT time)
    shadow_mperf: u64,
    /// TSC at last VMENTRY (for measuring exit duration)
    last_entry_tsc: u64,
    /// TSC at last VMEXIT
    last_exit_tsc: u64,
    /// Average CPUID exit cost in TSC cycles (calibrated)
    avg_cpuid_exit_cost: u64,
    /// Running count of VMEXIT events (for calibration)
    exit_count: u64,
    /// Running sum of exit costs (for computing average)
    exit_cost_sum: u64,
}

impl TimingStealth {
    /// Create a new timing stealth state
    #[must_use]
    pub fn new() -> Self {
        Self {
            tsc_offset: 0,
            cumulative_exit_tsc: 0,
            shadow_aperf: 0,
            shadow_mperf: 0,
            last_entry_tsc: 0,
            last_exit_tsc: 0,
            avg_cpuid_exit_cost: 1000, // Initial estimate
            exit_count: 0,
            exit_cost_sum: 0,
        }
    }

    /// Called at VMEXIT: record the TSC and update shadow counters.
    /// `host_tsc` is the TSC read immediately after VMEXIT.
    /// `host_aperf` and `host_mperf` are the physical counter values.
    pub fn on_vmexit(&mut self, host_tsc: u64) {
        self.last_exit_tsc = host_tsc;

        // Calculate guest execution time since last entry
        if self.last_entry_tsc > 0 {
            let guest_cycles = host_tsc.saturating_sub(self.last_entry_tsc);
            // Shadow counters advance only during guest execution
            self.shadow_aperf = self.shadow_aperf.wrapping_add(guest_cycles);
            self.shadow_mperf = self.shadow_mperf.wrapping_add(guest_cycles);
        }
    }

    /// Called at VMENTRY: record the TSC and update the offset.
    /// `host_tsc` is the TSC read just before VMENTRY/VMRESUME.
    pub fn on_vmentry(&mut self, host_tsc: u64) {
        if self.last_exit_tsc > 0 {
            let exit_duration = host_tsc.saturating_sub(self.last_exit_tsc);
            self.cumulative_exit_tsc += exit_duration;

            // Update calibration
            self.exit_count += 1;
            self.exit_cost_sum += exit_duration;
            if self.exit_count > 0 {
                self.avg_cpuid_exit_cost = self.exit_cost_sum / self.exit_count;
            }
        }

        self.last_entry_tsc = host_tsc;

        // Update TSC offset to hide cumulative VMEXIT time
        // Guest TSC = Host TSC + offset
        // We want: Guest TSC ≈ Host TSC - cumulative_exit_time
        #[allow(clippy::cast_possible_wrap)]
        {
            self.tsc_offset = -(self.cumulative_exit_tsc as i64);
        }
    }

    /// Get the TSC offset to write into VMCS/VMCB
    #[must_use]
    pub const fn tsc_offset(&self) -> i64 {
        self.tsc_offset
    }

    /// Handle RDMSR for IA32_APERF (0xE8) — return shadow value
    #[must_use]
    pub const fn read_aperf(&self) -> u64 {
        self.shadow_aperf
    }

    /// Handle RDMSR for IA32_MPERF (0xE7) — return shadow value
    #[must_use]
    pub const fn read_mperf(&self) -> u64 {
        self.shadow_mperf
    }

    /// Handle WRMSR for IA32_APERF — update shadow
    pub fn write_aperf(&mut self, value: u64) {
        self.shadow_aperf = value;
    }

    /// Handle WRMSR for IA32_MPERF — update shadow
    pub fn write_mperf(&mut self, value: u64) {
        self.shadow_mperf = value;
    }

    /// Get the average VMEXIT cost in TSC cycles
    #[must_use]
    pub const fn avg_exit_cost(&self) -> u64 {
        self.avg_cpuid_exit_cost
    }

    /// Get total number of VMEXITs tracked
    #[must_use]
    pub const fn exit_count(&self) -> u64 {
        self.exit_count
    }

    /// Get cumulative TSC spent in VMEXITs
    #[must_use]
    pub const fn cumulative_exit_tsc(&self) -> u64 {
        self.cumulative_exit_tsc
    }
}

impl Default for TimingStealth {
    fn default() -> Self {
        Self::new()
    }
}

/// MSR addresses for timing-related MSRs
pub mod msr {
    pub const IA32_MPERF: u32 = 0xE7;
    pub const IA32_APERF: u32 = 0xE8;
    pub const IA32_TSC: u32 = 0x10;
    pub const IA32_TSC_ADJUST: u32 = 0x3B;
    pub const IA32_PERF_STATUS: u32 = 0x198;
    pub const IA32_PERF_CTL: u32 = 0x199;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state() {
        let ts = TimingStealth::new();
        assert_eq!(ts.tsc_offset(), 0);
        assert_eq!(ts.read_aperf(), 0);
        assert_eq!(ts.read_mperf(), 0);
    }

    #[test]
    fn tsc_offset_hides_exit_time() {
        let mut ts = TimingStealth::new();

        // Simulate: guest runs from TSC 1000 to 2000 (1000 cycles)
        ts.on_vmentry(1000);
        ts.on_vmexit(2000);

        // Host spends 500 cycles handling the exit
        ts.on_vmentry(2500);

        // TSC offset should be -500 (hiding the 500 cycle exit)
        assert_eq!(ts.tsc_offset(), -500);
    }

    #[test]
    fn shadow_aperf_tracks_guest_time() {
        let mut ts = TimingStealth::new();

        ts.on_vmentry(1000);
        ts.on_vmexit(2000); // 1000 guest cycles

        assert_eq!(ts.read_aperf(), 1000);
        assert_eq!(ts.read_mperf(), 1000);

        // Second round
        ts.on_vmentry(2500); // 500 cycles in host
        ts.on_vmexit(4000); // 1500 more guest cycles

        assert_eq!(ts.read_aperf(), 2500); // 1000 + 1500
    }

    #[test]
    fn aperf_mperf_ratio_consistent() {
        let mut ts = TimingStealth::new();

        // Simulate several entries/exits
        for i in 0..100u64 {
            let base = i * 2000;
            ts.on_vmentry(base);
            ts.on_vmexit(base + 1000);
        }

        // APERF and MPERF should be equal (no frequency scaling simulated)
        assert_eq!(ts.read_aperf(), ts.read_mperf());
    }

    #[test]
    fn exit_calibration() {
        let mut ts = TimingStealth::new();

        ts.on_vmentry(0);
        ts.on_vmexit(1000);
        ts.on_vmentry(1200); // 200 cycle exit

        ts.on_vmexit(2200);
        ts.on_vmentry(2600); // 400 cycle exit

        assert_eq!(ts.exit_count(), 2);
        assert_eq!(ts.avg_exit_cost(), 300); // (200 + 400) / 2
    }

    #[test]
    fn cumulative_exit_tracking() {
        let mut ts = TimingStealth::new();

        ts.on_vmentry(0);
        ts.on_vmexit(1000);
        ts.on_vmentry(1500); // 500 in exit

        ts.on_vmexit(2500);
        ts.on_vmentry(3000); // 500 in exit

        assert_eq!(ts.cumulative_exit_tsc(), 1000); // 500 + 500
        assert_eq!(ts.tsc_offset(), -1000);
    }
}
