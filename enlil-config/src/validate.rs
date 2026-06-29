use crate::EnlilConfig;
use std::collections::HashSet;

/// Validate an Enlil configuration. Returns a list of errors (empty = valid).
#[must_use]
pub fn validate_config(config: &EnlilConfig) -> Vec<String> {
    let mut errors = Vec::new();

    // Check for CPU overlap between guests
    let mut global_cpus: HashSet<u32> = HashSet::new();
    for (id, guest) in &config.guest {
        if guest.cpus.is_empty() {
            errors.push(format!("Guest '{id}': no CPUs assigned"));
        }

        // Check for duplicates within this guest
        let mut local = HashSet::new();
        for &cpu in &guest.cpus {
            if !local.insert(cpu) {
                errors.push(format!("Guest '{id}': duplicate CPU {cpu}"));
            }
        }

        // Check for overlap with other guests (only for dedicated scheduling)
        if guest.scheduling == crate::SchedulingMode::Dedicated
            || guest.scheduling == crate::SchedulingMode::Auto
        {
            for &cpu in &guest.cpus {
                if !global_cpus.insert(cpu) {
                    errors.push(format!(
                        "Guest '{id}': CPU {cpu} already assigned to another guest"
                    ));
                }
            }
        }
    }

    // Check memory
    let total_guest_memory: u64 = config.guest.values().map(|g| g.memory_mb).sum();
    let total_with_hypervisor = total_guest_memory + config.hypervisor.reserved_memory_mb;
    // Warn if total exceeds 256GB (sanity check)
    if total_with_hypervisor > 256 * 1024 {
        errors.push(format!(
            "Total memory {total_with_hypervisor}MB exceeds 256GB sanity limit"
        ));
    }
    // When the host total is pinned (non-zero; 0 means auto-detect), the guests
    // plus the hypervisor reservation must actually fit in it — otherwise the
    // configuration overcommits RAM the host does not have and a guest is starved
    // or fails to map its memory at start.
    let host_total = config.hypervisor.total_memory_mb;
    if host_total > 0 && total_with_hypervisor > host_total {
        errors.push(format!(
            "Total memory {total_with_hypervisor}MB (guests {total_guest_memory}MB + \
             hypervisor reserve {}MB) exceeds host total_memory_mb={host_total}MB",
            config.hypervisor.reserved_memory_mb
        ));
    }

    // Check each guest has at least some memory
    for (id, guest) in &config.guest {
        if guest.memory_mb == 0 {
            errors.push(format!("Guest '{id}': memory_mb cannot be 0"));
        }
        if guest.memory_mb < 64 {
            errors.push(format!(
                "Guest '{}': memory_mb={} is below 64MB minimum",
                id, guest.memory_mb
            ));
        }
    }

    // Check guest names are non-empty
    for (id, guest) in &config.guest {
        if guest.name.trim().is_empty() {
            errors.push(format!("Guest '{id}': name cannot be empty"));
        }
    }

    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use std::collections::HashMap;

    fn minimal_config() -> EnlilConfig {
        let mut guests = HashMap::new();
        guests.insert(
            "vm1".into(),
            GuestConfig {
                name: "Test VM 1".into(),
                cpus: vec![0, 1],
                memory_mb: 2048,
                kernel: None,
                initrd: None,
                cmdline: "console=ttyS0".into(),
                scheduling: SchedulingMode::Dedicated,
                disks: vec![],
                serial: SerialPortConfig::default(),
            },
        );
        EnlilConfig {
            hypervisor: HypervisorConfig::default(),
            guest: guests,
        }
    }

    #[test]
    fn valid_config_passes() {
        let config = minimal_config();
        let errors = validate_config(&config);
        assert!(errors.is_empty(), "Expected no errors, got: {errors:?}");
    }

    #[test]
    fn detects_cpu_overlap() {
        let mut config = minimal_config();
        config.guest.insert(
            "vm2".into(),
            GuestConfig {
                name: "Test VM 2".into(),
                cpus: vec![1, 2], // CPU 1 overlaps with vm1
                memory_mb: 2048,
                kernel: None,
                initrd: None,
                cmdline: "console=ttyS0".into(),
                scheduling: SchedulingMode::Dedicated,
                disks: vec![],
                serial: SerialPortConfig::default(),
            },
        );
        let errors = validate_config(&config);
        assert!(errors.iter().any(|e| e.contains("CPU 1 already assigned")));
    }

    #[test]
    fn detects_zero_memory() {
        let mut config = minimal_config();
        config.guest.get_mut("vm1").unwrap().memory_mb = 0;
        let errors = validate_config(&config);
        assert!(errors.iter().any(|e| e.contains("cannot be 0")));
    }

    #[test]
    fn detects_memory_overcommit_against_pinned_host_total() {
        let mut config = minimal_config();
        // vm1 needs 2048MB; with the 512MB default reserve that is 2560MB. Pin the
        // host total below that so the guests no longer fit.
        config.hypervisor.total_memory_mb = 2048;
        let errors = validate_config(&config);
        assert!(
            errors.iter().any(|e| e.contains("exceeds host total_memory_mb")),
            "expected an overcommit error, got: {errors:?}"
        );
    }

    #[test]
    fn host_total_zero_means_auto_detect_and_does_not_overcommit() {
        let mut config = minimal_config();
        // 0 is the auto-detect sentinel: no overcommit check should fire.
        config.hypervisor.total_memory_mb = 0;
        let errors = validate_config(&config);
        assert!(
            !errors.iter().any(|e| e.contains("exceeds host total_memory_mb")),
            "auto-detect host total must not trigger an overcommit error: {errors:?}"
        );
    }

    #[test]
    fn guests_that_fit_the_pinned_host_total_pass() {
        let mut config = minimal_config();
        // 2048 (guest) + 512 (reserve) = 2560; a 4096MB host has room.
        config.hypervisor.total_memory_mb = 4096;
        let errors = validate_config(&config);
        assert!(errors.is_empty(), "expected no errors, got: {errors:?}");
    }
}
