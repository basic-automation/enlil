use crate::EnlilConfig;
use std::collections::HashSet;

/// Validate an Enlil configuration. Returns a list of errors (empty = valid).
pub fn validate_config(config: &EnlilConfig) -> Vec<String> {
    let mut errors = Vec::new();

    // Check for CPU overlap between guests
    let mut global_cpus: HashSet<u32> = HashSet::new();
    for (id, guest) in &config.guest {
        if guest.cpus.is_empty() {
            errors.push(format!("Guest '{}': no CPUs assigned", id));
        }

        // Check for duplicates within this guest
        let mut local = HashSet::new();
        for &cpu in &guest.cpus {
            if !local.insert(cpu) {
                errors.push(format!("Guest '{}': duplicate CPU {}", id, cpu));
            }
        }

        // Check for overlap with other guests (only for dedicated scheduling)
        if guest.scheduling == crate::SchedulingMode::Dedicated
            || guest.scheduling == crate::SchedulingMode::Auto
        {
            for &cpu in &guest.cpus {
                if !global_cpus.insert(cpu) {
                    errors.push(format!(
                        "Guest '{}': CPU {} already assigned to another guest",
                        id, cpu
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
            "Total memory {}MB exceeds 256GB sanity limit",
            total_with_hypervisor
        ));
    }

    // Check each guest has at least some memory
    for (id, guest) in &config.guest {
        if guest.memory_mb == 0 {
            errors.push(format!("Guest '{}': memory_mb cannot be 0", id));
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
            errors.push(format!("Guest '{}': name cannot be empty", id));
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
        guests.insert("vm1".into(), GuestConfig {
            name: "Test VM 1".into(),
            cpus: vec![0, 1],
            memory_mb: 2048,
            kernel: None,
            initrd: None,
            cmdline: "console=ttyS0".into(),
            scheduling: SchedulingMode::Dedicated,
            disks: vec![],
        });
        EnlilConfig {
            hypervisor: HypervisorConfig::default(),
            guest: guests,
        }
    }

    #[test]
    fn valid_config_passes() {
        let config = minimal_config();
        let errors = validate_config(&config);
        assert!(errors.is_empty(), "Expected no errors, got: {:?}", errors);
    }

    #[test]
    fn detects_cpu_overlap() {
        let mut config = minimal_config();
        config.guest.insert("vm2".into(), GuestConfig {
            name: "Test VM 2".into(),
            cpus: vec![1, 2], // CPU 1 overlaps with vm1
            memory_mb: 2048,
            kernel: None,
            initrd: None,
            cmdline: "console=ttyS0".into(),
            scheduling: SchedulingMode::Dedicated,
            disks: vec![],
        });
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
}
