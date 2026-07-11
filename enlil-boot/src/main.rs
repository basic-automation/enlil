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

    // Collect the platform resources the kernel needs (Phase 6.1). We read
    // them here, while boot services are live, so the next slice can pack them
    // into a `BootHandoff` and pass it across `ExitBootServices()`.
    match uefi_boot::find_acpi_rsdp() {
        0 => log::warn!("no ACPI RSDP in the UEFI configuration table"),
        addr => log::info!("ACPI RSDP at {addr:#x}"),
    }

    match uefi_boot::collect_framebuffer() {
        Some(fb) => log::info!(
            "GOP framebuffer: {}x{} stride={} bpp={} base={:#x} ({} bytes)",
            fb.width,
            fb.height,
            fb.stride,
            fb.bytes_per_pixel,
            fb.base,
            fb.size_bytes(),
        ),
        None => log::warn!("no addressable GOP framebuffer available"),
    }

    // Phase 6.1 next slice: also snapshot the UEFI memory map, call
    // `ExitBootServices()`, and jump into the kernel with the assembled
    // `BootHandoff`. Until then, return to firmware cleanly.
    uefi::Status::SUCCESS
}

/// Firmware-protocol readers, isolated so the pure handoff logic they feed
/// stays host-testable in `enlil_boot::handoff`.
#[cfg(target_os = "uefi")]
mod uefi_boot {
    use enlil_boot::handoff::{Framebuffer, PixelFormat};

    /// Physical address of the ACPI RSDP from the UEFI configuration table,
    /// preferring the ACPI 2.0+ (XSDT) entry over the legacy 1.0 one; `0` if
    /// the firmware exposed neither.
    pub fn find_acpi_rsdp() -> u64 {
        use uefi::table::cfg::{ACPI_GUID, ACPI2_GUID};
        uefi::system::with_config_table(|entries| {
            let mut legacy = 0u64;
            for entry in entries {
                if entry.guid == ACPI2_GUID {
                    return entry.address as u64;
                }
                if entry.guid == ACPI_GUID {
                    legacy = entry.address as u64;
                }
            }
            legacy
        })
    }

    /// The current GOP framebuffer, or `None` if there is no graphics output
    /// or the mode is blt-only (no linear framebuffer).
    pub fn collect_framebuffer() -> Option<Framebuffer> {
        use uefi::proto::console::gop::{GraphicsOutput, PixelFormat as GopFmt};

        let handle = uefi::boot::get_handle_for_protocol::<GraphicsOutput>().ok()?;
        let mut gop = uefi::boot::open_protocol_exclusive::<GraphicsOutput>(handle).ok()?;

        let info = gop.current_mode_info();
        let format = match info.pixel_format() {
            GopFmt::Rgb => PixelFormat::Rgb,
            GopFmt::Bgr => PixelFormat::Bgr,
            GopFmt::Bitmask => PixelFormat::Bitmask,
            GopFmt::BltOnly => PixelFormat::BltOnly,
        };
        // Bail before touching the framebuffer in a blt-only mode (uefi-rs
        // panics on `frame_buffer()` there).
        format.bytes_per_pixel()?;

        let (width, height) = info.resolution();
        let stride = info.stride();
        let base = gop.frame_buffer().as_mut_ptr() as u64;

        Framebuffer::from_gop(
            base,
            u32::try_from(width).ok()?,
            u32::try_from(height).ok()?,
            u32::try_from(stride).ok()?,
            format,
        )
    }
}

#[cfg(not(target_os = "uefi"))]
fn main() {
    println!("{}", enlil_boot::BOOT_BANNER);
    println!(
        "enlil-boot is the UEFI firmware payload; build it with \
         `cargo build -p enlil-boot --target x86_64-unknown-uefi`."
    );
}
