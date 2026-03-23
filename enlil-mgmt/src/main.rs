//! Enlil Management Console
//!
//! CLI tool for managing the Enlil hypervisor — loading configs,
//! starting/stopping guests, and monitoring status.

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "enlil", about = "Enlil hypervisor management console")]
struct Cli {
    /// Path to the configuration file
    #[arg(short, long, default_value = "enlil.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::Subcommand, Debug)]
enum Commands {
    /// Validate configuration file
    Validate,
    /// Start all configured guests
    Start,
    /// Show status of running guests
    Status,
    /// Stop all guests
    Stop,
}

fn parse_serial_output(output: &str) -> enlil_core::serial::SerialOutputMode {
    match output {
        "null" => enlil_core::serial::SerialOutputMode::Null,
        "buffer" => enlil_core::serial::SerialOutputMode::Buffer,
        s if s.starts_with("file:") => {
            enlil_core::serial::SerialOutputMode::File(s[5..].to_string())
        }
        _ => enlil_core::serial::SerialOutputMode::Stdout,
    }
}

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    let config = enlil_config::load_config(&cli.config)?;
    log::info!("Loaded config with {} guest(s)", config.guest.len());

    match cli.command {
        Commands::Validate => {
            println!("Configuration is valid.");
            for (id, guest) in &config.guest {
                println!(
                    "  Guest '{}': {} — {} vCPUs, {}MB RAM, scheduling={}",
                    id,
                    guest.name,
                    guest.cpus.len(),
                    guest.memory_mb,
                    match guest.scheduling {
                        enlil_config::SchedulingMode::Dedicated => "dedicated",
                        enlil_config::SchedulingMode::Timeslice => "timeslice",
                        enlil_config::SchedulingMode::Auto => "auto",
                    }
                );
            }
        }
        Commands::Start => {
            println!("Starting guests...");

            // Determine total memory (use configured or default 16GB)
            let total_mem = if config.hypervisor.total_memory_mb > 0 {
                config.hypervisor.total_memory_mb * 1024 * 1024
            } else {
                16 * 1024 * 1024 * 1024 // 16 GB default
            };
            let reserved = config.hypervisor.reserved_memory_mb * 1024 * 1024;

            let mut hypervisor = enlil_core::vm::Hypervisor::new(total_mem, reserved);

            for (id, guest) in &config.guest {
                println!("  Initializing '{}' ({})...", id, guest.name);
                let vm_config = enlil_core::vm::VmConfig {
                    name: guest.name.clone(),
                    cpus: guest.cpus.clone(),
                    memory: enlil_core::memory::GuestMemoryConfig {
                        size_mb: guest.memory_mb,
                    },
                    kernel: guest.kernel.clone(),
                    initrd: guest.initrd.clone(),
                    cmdline: guest.cmdline.clone(),
                    scheduling: match guest.scheduling {
                        enlil_config::SchedulingMode::Dedicated => {
                            enlil_core::vcpu::SchedulingPolicy::Dedicated
                        }
                        enlil_config::SchedulingMode::Timeslice => {
                            enlil_core::vcpu::SchedulingPolicy::TimeSlice { quantum_ms: 10 }
                        }
                        enlil_config::SchedulingMode::Auto => {
                            enlil_core::vcpu::SchedulingPolicy::Auto
                        }
                    },
                    serial_output: parse_serial_output(&guest.serial.output),
                };

                let idx = hypervisor.add_vm(vm_config)?;
                let vm = hypervisor.vm(idx).unwrap();
                println!(
                    "    Created VM '{}' with {} vCPUs [{}]",
                    vm.name(),
                    vm.vcpu_count(),
                    vm.state()
                );
            }

            println!(
                "\nHypervisor ready: {} VMs, {:.0} MB allocated, {:.0} MB available",
                hypervisor.vm_count(),
                hypervisor.memory_manager().allocated_bytes() as f64 / (1024.0 * 1024.0),
                hypervisor.memory_manager().available_bytes() as f64 / (1024.0 * 1024.0),
            );
        }
        Commands::Status => {
            println!("Status: not yet implemented (requires runtime state)");
        }
        Commands::Stop => {
            println!("Stop: not yet implemented (requires runtime state)");
        }
    }

    Ok(())
}
