//! `enlil-run` — the top-level guest-boot orchestrator binary (item 3.12).
//!
//! Loads a hypervisor config and a Linux `bzImage`, validates the config, and
//! boots the first configured guest via
//! [`enlil_core::orchestrator::run_first_guest`] — the thin CLI over the tested
//! orchestration library. `target_os = "linux"`-only (it drives KVM).
//!
//! Usage: `enlil-run <config.toml> <bzImage> [cmdline]`

use std::process::ExitCode;

#[cfg(target_os = "linux")]
use std::path::Path;

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: {} <config.toml> <bzImage> [cmdline]", args[0]);
        return ExitCode::FAILURE;
    }
    let config_path = &args[1];
    let kernel_path = &args[2];
    let cmdline = args.get(3).map_or("console=ttyS0", String::as_str);

    match run(config_path, kernel_path, cmdline) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("enlil-run: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// On a target with no host VMM backend (only `target_os = "linux"` ships the
/// KVM backend today), the orchestrator cannot boot a guest. The binary still
/// builds — so the workspace stays compilable on the Windows dev toolchain —
/// but it exits with a clear message rather than pretending to run.
#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!(
        "enlil-run requires the Linux/KVM host backend; this target has no host VMM backend yet."
    );
    ExitCode::FAILURE
}

#[cfg(target_os = "linux")]
fn run(config_path: &str, kernel_path: &str, cmdline: &str) -> anyhow::Result<()> {
    let config = enlil_config::load_config(Path::new(config_path))?;

    let errors = enlil_config::validate_config(&config);
    if !errors.is_empty() {
        for err in &errors {
            eprintln!("config error: {err}");
        }
        anyhow::bail!("{} configuration error(s)", errors.len());
    }

    let kernel = std::fs::read(kernel_path)
        .map_err(|e| anyhow::anyhow!("reading kernel {kernel_path}: {e}"))?;

    // Load the first guest's initrd from its config path, if it names one.
    let initrd = config
        .guest
        .values()
        .next()
        .and_then(|g| g.initrd.as_ref())
        .map(|path| {
            std::fs::read(path)
                .map_err(|e| anyhow::anyhow!("reading initrd {}: {e}", path.display()))
        })
        .transpose()?;

    println!(
        "Booting first guest from {config_path} with kernel {kernel_path} ({} bytes){}, \
         cmdline: {cmdline:?}",
        kernel.len(),
        initrd
            .as_ref()
            .map_or(String::new(), |i| format!(" + initrd ({} bytes)", i.len())),
    );
    // A generous per-boot entry bound; a real guest runs until it halts/resets.
    let outcome = enlil_core::orchestrator::run_first_guest(
        &config,
        &kernel,
        initrd.as_deref(),
        cmdline,
        10_000_000,
    )?;
    println!("Guest exited: {outcome:?}");
    Ok(())
}
