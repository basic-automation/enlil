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
                    "  Guest '{}': {} — {} vCPUs, {}MB RAM",
                    id,
                    guest.name,
                    guest.cpus.len(),
                    guest.memory_mb
                );
            }
        }
        Commands::Start => {
            println!("Starting guests...");
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
                };
                let vm = enlil_core::vm::Vm::new(vm_config)?;
                println!(
                    "    Created VM '{}' with {} vCPUs [{}]",
                    vm.name(),
                    vm.vcpu_count(),
                    vm.state()
                );
            }
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
