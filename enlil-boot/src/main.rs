// enlil-boot binary: the UEFI firmware entry point.
//
// Built for `x86_64-unknown-uefi` this is a `no_std`/`no_main` UEFI
// application whose `efi_main` runs as the firmware boot payload. Built for
// the dev host it degrades to an ordinary `main` that just documents how to
// produce the firmware image, so `cargo build/test/clippy --workspace` stays
// green on Linux/Windows without a UEFI toolchain.
#![cfg_attr(target_os = "uefi", no_std)]
#![cfg_attr(target_os = "uefi", no_main)]

#[cfg(target_os = "uefi")]
#[uefi::entry]
fn efi_main() -> uefi::Status {
    // Bring up the uefi-rs helpers (global allocator, logger routed to the
    // active console/serial, and panic handler).
    uefi::helpers::init().unwrap();

    // First live-boot signal: prove the enlil payload is running under
    // firmware control. The QEMU+OVMF harness asserts this on the serial line.
    log::info!("{}", enlil_boot::BOOT_BANNER);

    // Phase 6.1 next slices collect the UEFI memory map + ACPI RSDP + GOP
    // framebuffer into an `enlil_boot::handoff::BootHandoff`, call
    // `ExitBootServices()`, and jump into the kernel. Until then, return to
    // firmware cleanly.
    uefi::Status::SUCCESS
}

#[cfg(not(target_os = "uefi"))]
fn main() {
    println!("{}", enlil_boot::BOOT_BANNER);
    println!(
        "enlil-boot is the UEFI firmware payload; build it with \
         `cargo build -p enlil-boot --target x86_64-unknown-uefi`."
    );
}
