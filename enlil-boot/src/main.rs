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
    use enlil_boot::handoff::BootHandoff;
    use uefi::boot::MemoryType;
    use uefi::mem::memory_map::MemoryMap;

    // Bring up the uefi-rs helpers (global allocator, logger routed to the
    // active console/serial, and panic handler).
    uefi::helpers::init().unwrap();

    // First live-boot signal, emitted while boot services are still up.
    log::info!("{}", enlil_boot::BOOT_BANNER);

    // Collect the platform resources the kernel needs (Phase 6.1) while boot
    // services are live, binding them for the post-exit handoff block.
    let acpi_rsdp = uefi_boot::find_acpi_rsdp();
    match acpi_rsdp {
        0 => log::warn!("no ACPI RSDP in the UEFI configuration table"),
        addr => log::info!("ACPI RSDP at {addr:#x}"),
    }

    let framebuffer = uefi_boot::collect_framebuffer();
    match &framebuffer {
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

    // PCI root bridges from the firmware's PCI Root Bridge I/O protocol
    // handles (one per PCI segment) — the inventory the boot device tree
    // (roadmap 6.3) consumes. Must be collected before ExitBootServices:
    // after exit the protocols are gone.
    let (pci_root_bridge_count, pci_root_bridges) = uefi_boot::collect_pci_root_bridges();
    if pci_root_bridge_count == 0 {
        log::warn!("pci: no root bridge inventory collected");
    }

    // Take full control of the machine. After this returns, the firmware boot
    // services — including the uefi-rs logger and allocator — are gone, so all
    // further output must go straight to hardware. The returned map is the
    // final UEFI memory map, which the kernel re-parses via `enlil-platform`'s
    // `MemoryMap::from_uefi`.
    let memory_map = unsafe { uefi::boot::exit_boot_services(MemoryType::LOADER_DATA) };

    let meta = memory_map.meta();
    let handoff = BootHandoff {
        memory_map_base: memory_map.buffer().as_ptr() as u64,
        memory_map_len: meta.map_size,
        memory_descriptor_size: meta.desc_size,
        acpi_rsdp,
        framebuffer,
        pci_root_bridges,
        pci_root_bridge_count,
    };
    // Keep the memory-map buffer alive for the kernel: dropping the owned map
    // would call the now-defunct boot-services `FreePool`. Its base and extent
    // are already recorded in `handoff`.
    core::mem::forget(memory_map);

    // Prove we are running under our own control past ExitBootServices: emit
    // the liveness banner straight to COM1. This is the serial line the
    // QEMU+OVMF boot harness asserts.
    let serial = enlil_boot::serial::SerialPort::com1();
    serial.write_str(enlil_boot::BOOT_BANNER);
    serial.write_byte(b'\n');
    if handoff.is_valid() {
        serial.write_str("handoff: UEFI memory map + ACPI RSDP captured\n");
    } else {
        serial.write_str("handoff: INCOMPLETE (missing memory map or RSDP)\n");
    }
    // The PCI inventory collected pre-exit — the durable record the kernel's
    // PCI bring-up reports below (and the qemu-boot-test.sh "pci" proof).
    if handoff.has_pci_root_bridges() {
        let mut digits = [0u8; 20];
        serial.write_str("handoff: PCI root bridge handles captured: ");
        serial.write_str(enlil_boot::kernel::format_u64(
            u64::from(handoff.pci_root_bridge_count),
            &mut digits,
        ));
        serial.write_str("\n");
    } else {
        serial.write_str("handoff: no PCI root bridge handles captured\n");
    }

    // Transition control into the enlil kernel. Never returns: boot services
    // are gone, so control cannot go back to the firmware.
    enlil_boot::kernel::kernel_entry(&handoff)
}

/// Firmware-protocol readers, isolated so the pure handoff logic they feed
/// stays host-testable in `enlil_boot::handoff`.
#[cfg(target_os = "uefi")]
mod uefi_boot {
    use enlil_boot::handoff::{Framebuffer, MAX_PCI_ROOT_BRIDGES, PciRootBridge, PixelFormat};

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

    /// Raw `EFI_PCI_ROOT_BRIDGE_IO_PROTOCOL` as implemented by EDK2
    /// (MdePkg/Include/Protocol/PciRootBridgeIo.h).
    ///
    /// Declared by hand because uefi-rs 0.33 does not bind this protocol.
    /// The layout follows EDK2 exactly: note the `PollMem` member right
    /// after `ParentHandle`, the `Mem`/`Io`/`Pci` access-struct order, and
    /// EDK2's trailing `SegmentNumber` — all of which differ from the older
    /// UEFI 2.0 summary of the protocol, so verify against the header, not
    /// memory. Only `GetAttributes` and `Configuration` are ever called;
    /// every other member is an opaque pointer slot whose only job is to
    /// keep those two at their real offsets. The two called members are
    /// `Option<fn>` so a firmware that installs a short/odd table fails
    /// closed instead of jumping through a garbage pointer.
    ///
    /// EDK2's trailing `SegmentNumber` (UINT32) is deliberately not
    /// declared: the segment is read from the handle's device path instead,
    /// so this binding also works on firmware without the extension.
    #[uefi::proto::unsafe_protocol("2f707ebb-4a1a-11d4-9a38-0090273fc14d")]
    #[repr(C)]
    struct RawPciRootBridgeIo {
        parent_handle: uefi::Handle,
        poll_mem: *const core::ffi::c_void,
        poll_io: *const core::ffi::c_void,
        mem_read: *const core::ffi::c_void,
        mem_write: *const core::ffi::c_void,
        io_read: *const core::ffi::c_void,
        io_write: *const core::ffi::c_void,
        pci_read: *const core::ffi::c_void,
        pci_write: *const core::ffi::c_void,
        copy_mem: *const core::ffi::c_void,
        map: *const core::ffi::c_void,
        unmap: *const core::ffi::c_void,
        allocate_buffer: *const core::ffi::c_void,
        free_buffer: *const core::ffi::c_void,
        flush: *const core::ffi::c_void,
        get_attributes: Option<
            unsafe extern "efiapi" fn(
                this: *const RawPciRootBridgeIo,
                supports: *mut u64,
                attributes: *mut u64,
            ) -> uefi::Status,
        >,
        set_attributes: *const core::ffi::c_void,
        configuration: Option<
            unsafe extern "efiapi" fn(
                this: *const RawPciRootBridgeIo,
                resources: *mut *const core::ffi::c_void,
            ) -> uefi::Status,
        >,
    }

    /// Bus range (`AddrRangeMin`..=`AddrRangeMax`) of the bus-type address
    /// space descriptor in a `Configuration()` resource buffer, or `None`.
    ///
    /// The buffer is a packed run of `EFI_ACPI_ADDRESS_SPACE_DESCRIPTOR`s
    /// (QWORD address-space descriptors, 46 bytes each: `Desc=0x8A`,
    /// `Len=43`) terminated by an end tag (`Desc=0x79`). Parsing stops at
    /// the first malformed entry and is iteration-capped, so a corrupt
    /// firmware buffer cannot spin the boot payload.
    fn parse_bus_range(resources: *const core::ffi::c_void) -> Option<(u8, u8)> {
        const DESC_ADDRESS_SPACE: u8 = 0x8A;
        const DESC_END_TAG: u8 = 0x79;
        const DESCRIPTOR_LEN: usize = 46;
        const DESCRIPTOR_LEN_FIELD: u16 = 43; // bytes after Desc+Len
        // ACPI address-space resource types: 0x00 memory, 0x01 I/O,
        // 0x02 bus-number range.
        const RES_TYPE_BUS: u8 = 0x02;
        const MAX_DESCRIPTORS: usize = 32;

        if resources.is_null() {
            return None;
        }
        let mut cursor = resources.cast::<u8>();
        for _ in 0..MAX_DESCRIPTORS {
            // SAFETY: `resources` is the buffer `Configuration()` just
            // returned; each read is validated (Desc/Len) before advancing,
            // and the loop is capped.
            let desc = unsafe { cursor.read() };
            if desc == DESC_END_TAG {
                break;
            }
            if desc != DESC_ADDRESS_SPACE {
                break;
            }
            let len = unsafe { cursor.add(1).cast::<u16>().read_unaligned() };
            if len != DESCRIPTOR_LEN_FIELD {
                break;
            }
            let res_type = unsafe { cursor.add(3).read() };
            if res_type == RES_TYPE_BUS {
                let min = unsafe { cursor.add(14).cast::<u64>().read_unaligned() };
                let max = unsafe { cursor.add(22).cast::<u64>().read_unaligned() };
                return if min <= max && max <= u64::from(u8::MAX) {
                    // Guarded by the check above, so the truncation is exact.
                    Some((min as u8, max as u8))
                } else {
                    None
                };
            }
            // Advance past this descriptor; usize arithmetic so overflow is
            // checked instead of wrapping the pointer.
            cursor = match (cursor as usize).checked_add(DESCRIPTOR_LEN) {
                Some(next) => next as *const u8,
                None => break,
            };
        }
        None
    }

    /// PCI segment group of a root-bridge handle, from the UID of its ACPI
    /// device-path node (`_SEG`); 0 when the path is missing or malformed.
    ///
    /// The HID distinguishes PCI (`PNP0A03`) from PCIe (`PNP0A08`) root
    /// bridges; both carry the segment in UID.
    fn root_bridge_segment(handle: uefi::Handle) -> u16 {
        use uefi::boot::{OpenProtocolAttributes, OpenProtocolParams};
        use uefi::proto::device_path::{DevicePath, DevicePathNodeEnum};

        // Compressed-EISA HIDs: (product << 16) | manufacturer("PNP"=0x41D0).
        const PNP0A03_PCI_ROOT_BRIDGE: u32 = 0x0A03_41D0;
        const PNP0A08_PCIE_ROOT_BRIDGE: u32 = 0x0A08_41D0;

        let params = OpenProtocolParams {
            handle,
            agent: uefi::boot::image_handle(),
            controller: None,
        };
        // SAFETY: GetProtocol only reads the interface pointer; the
        // ScopedProtocol is dropped before ExitBootServices.
        let path = match unsafe {
            uefi::boot::open_protocol::<DevicePath>(params, OpenProtocolAttributes::GetProtocol)
        } {
            Ok(path) => path,
            Err(_) => return 0,
        };
        for node in path.node_iter() {
            if let Ok(DevicePathNodeEnum::AcpiAcpi(acpi)) = node.as_enum() {
                let hid = acpi.hid();
                if hid == PNP0A03_PCI_ROOT_BRIDGE || hid == PNP0A08_PCIE_ROOT_BRIDGE {
                    return acpi.uid() as u16;
                }
            }
        }
        0
    }

    /// PCI root bridges inventoried from the firmware, as `(count, table)`.
    ///
    /// Enumerates every handle carrying the PCI Root Bridge I/O Protocol
    /// (one per firmware-enumerated PCI segment), reads each bridge's
    /// attributes and bus range, and takes the segment from the bridge's
    /// device path. The table is bounded by
    /// [`MAX_PCI_ROOT_BRIDGES`](enlil_boot::handoff::MAX_PCI_ROOT_BRIDGES);
    /// bridges past the bound are still logged, just not recorded.
    ///
    /// Protocols are opened with `GetProtocol` rather than exclusively: the
    /// firmware's PCI bus driver already holds them `BY_DRIVER`, so an
    /// exclusive open would be denied.
    pub fn collect_pci_root_bridges() -> (u8, [PciRootBridge; MAX_PCI_ROOT_BRIDGES]) {
        use uefi::boot::{OpenProtocolAttributes, OpenProtocolParams};

        let mut table = [PciRootBridge::default(); MAX_PCI_ROOT_BRIDGES];
        let mut count: u8 = 0;

        let handles = match uefi::boot::find_handles::<RawPciRootBridgeIo>() {
            Ok(handles) => handles,
            Err(err) => {
                log::warn!("pci: no PCI root bridge handles ({err:?})");
                return (0, table);
            }
        };

        for handle in handles {
            let params = OpenProtocolParams {
                handle,
                agent: uefi::boot::image_handle(),
                controller: None,
            };
            // SAFETY: GetProtocol only reads the interface pointer; the
            // ScopedProtocol is dropped before ExitBootServices, and only
            // the two read-only members declared above are ever called.
            let proto = match unsafe {
                uefi::boot::open_protocol::<RawPciRootBridgeIo>(
                    params,
                    OpenProtocolAttributes::GetProtocol,
                )
            } {
                Ok(proto) => proto,
                Err(err) => {
                    log::warn!("pci: cannot open root-bridge protocol ({err:?})");
                    continue;
                }
            };

            let this = core::ptr::from_ref(&*proto);
            let mut supports = 0u64;
            let mut attributes = 0u64;
            let attrs_ok = match proto.get_attributes {
                Some(get_attributes) => {
                    // SAFETY: `this` points at the live protocol instance the
                    // ScopedProtocol holds; the out-pointers are valid u64s.
                    let status = unsafe { get_attributes(this, &mut supports, &mut attributes) };
                    status == uefi::Status::SUCCESS
                }
                None => false,
            };
            if !attrs_ok {
                log::warn!("pci: root-bridge GetAttributes failed; skipping handle");
                continue;
            }

            let (bus_start, bus_end) = match proto.configuration {
                Some(configuration) => {
                    let mut resources: *const core::ffi::c_void = core::ptr::null();
                    // SAFETY: as above; `resources` is written by the call.
                    let status = unsafe { configuration(this, &mut resources) };
                    if status != uefi::Status::SUCCESS {
                        log::warn!("pci: root-bridge Configuration failed ({status:?})");
                        (0, 0)
                    } else {
                        let range = parse_bus_range(resources).unwrap_or((0, 0));
                        // NOTE: deliberately no FreePool here. EDK2's
                        // Configuration() returns its driver-owned
                        // ConfigBuffer, not a per-call allocation — freeing
                        // it would corrupt the firmware heap. If some other
                        // firmware does allocate per call, the few leaked
                        // bytes are boot-services memory that
                        // ExitBootServices reclaims anyway.
                        range
                    }
                }
                None => (0, 0),
            };

            let segment = root_bridge_segment(handle);
            log::info!(
                "pci: root bridge seg={segment} buses {bus_start:#04x}-{bus_end:#04x} attrs={attributes:#x}"
            );
            let bridge = PciRootBridge {
                segment,
                bus_start,
                bus_end,
                // Every EFI_PCI_ATTRIBUTE_* the spec defines fits in 32 bits.
                attributes: attributes as u32,
            };
            if (count as usize) < MAX_PCI_ROOT_BRIDGES {
                table[count as usize] = bridge;
                count += 1;
            } else {
                log::warn!(
                    "pci: more than {MAX_PCI_ROOT_BRIDGES} root bridges; extra not recorded"
                );
            }
        }

        if count == 0 {
            log::warn!("pci: no PCI root bridge handles collected");
        }
        (count, table)
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
