//! Intel VT-d DMA-remapping bring-up (Phase 6.4, T-6.16).
//!
//! The DMAR told us *where* each remapping unit's registers live (see
//! [`crate::acpi::RemappingUnit`]); this module *programs* them. For one unit
//! it builds the root table, a context table, and two DMA domains — the host
//! domain every bus-0 device starts in, and a guest domain that receives the
//! first assignable bus-0 device — then enables translation, so device
//! passthrough is confined by the IOMMU instead of running unconfined
//! (SECURITY.md's known gap, ROADMAP 6.4).
//!
//! The register/entry encodings live in [`enlil_hal::vtd`] (the ISA seam,
//! LOCKED PRINCIPLE 2); the volatile MMIO, allocation, and enable sequence
//! live here, gated to the firmware target like the rest of the bring-up.
//! Interrupt remapping (ROADMAP 6.4's remainder) is deliberately not touched.

/// DMA domain ID for the boot kernel's own devices.
///
/// Never 0: under Caching Mode (`CAP.CM`) domain ID 0 is architecturally
/// reserved, and several IOMMUs (including QEMU's) report it.
pub const HOST_DOMAIN_ID: u16 = 1;

/// DMA domain ID for the first per-guest domain — T-6.16's demonstrated
/// device assignment.
pub const GUEST_DOMAIN_ID: u16 = 2;

/// Low-memory span always identity-mapped for DMA: the sub-4 GiB MMIO window
/// (ECAM, LAPIC, the IOMMU registers themselves) plus low RAM.
const IDENTITY_MAP_MIN_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Bytes of second-stage identity map to build.
///
/// The firmware-reported RAM end (rounded up to 2 MiB) or 4 GiB, whichever is
/// larger — so the host domain covers every RAM page plus the MMIO window
/// below 4 GiB.
#[must_use]
pub fn identity_map_bytes(highest_usable_end: u64) -> u64 {
    const HUGE_2MIB: u64 = 2 * 1024 * 1024;
    let aligned = highest_usable_end
        .div_ceil(HUGE_2MIB)
        .saturating_mul(HUGE_2MIB);
    aligned.max(IDENTITY_MAP_MIN_BYTES)
}

/// Why programming a remapping unit failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramError {
    /// The unit reports no usable AGAW in `CAP.SAGAW`.
    UnsupportedAddressWidth,
    /// A table allocation failed (kernel heap exhausted).
    AllocFailed,
    /// Building a domain's second-stage tables failed.
    TableBuildFailed,
    /// `GCMD.SRTP` never completed.
    SrtpTimeout,
    /// Translation enable (`GCMD.TE`) never completed.
    TranslationEnableTimeout,
    /// A context-cache invalidation never completed.
    ContextInvalidateTimeout,
    /// An IOTLB invalidation never completed.
    IotlbInvalidateTimeout,
    /// The bus's root entry was not present — nothing to assign through.
    NoContextTable,
}

impl ProgramError {
    /// A short human name for the serial report.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::UnsupportedAddressWidth => "no usable AGAW in CAP.SAGAW",
            Self::AllocFailed => "table allocation failed",
            Self::TableBuildFailed => "second-stage table build failed",
            Self::SrtpTimeout => "GCMD.SRTP never completed",
            Self::TranslationEnableTimeout => "GCMD.TE never completed",
            Self::ContextInvalidateTimeout => "context-cache invalidate never completed",
            Self::IotlbInvalidateTimeout => "IOTLB invalidate never completed",
            Self::NoContextTable => "no context table for the bus",
        }
    }
}

#[cfg(target_os = "uefi")]
pub use hw::{ProgramReport, assign_device_to_domain, program_unit};

#[cfg(target_os = "uefi")]
mod hw {
    use super::{GUEST_DOMAIN_ID, HOST_DOMAIN_ID, ProgramError};
    use crate::acpi::RemappingUnit;
    use crate::pci::{
        PCI_CONFIG_ADDRESS, PCI_CONFIG_DATA, config_address, vendor_of, vendor_present,
    };
    use enlil_hal::vtd::{
        AddressWidth, GCMD_SRTP, GCMD_TE, REG_CAP, REG_CCMD, REG_ECAP, REG_GCMD, REG_GSTS,
        REG_RTADDR, TranslationType, build_second_stage_identity, cap_coherent, cap_sagaw,
        cap_super_page_2mib, ccmd_device_invalidate, ccmd_global_invalidate, ccmd_pending,
        choose_address_width, context_entry, context_index, ecap_iotlb_reg_offset,
        iotlb_domain_invalidate, iotlb_global_invalidate, iotlb_pending, root_entry,
        root_entry_ctp, root_entry_present, root_index, rtaddr_value, second_stage_table_count,
    };

    /// Volatile access to one remapping unit's register block.
    struct Regs {
        base: u64,
    }

    impl Regs {
        /// # Safety
        ///
        /// `self.base + off` must be a live MMIO register.
        unsafe fn read32(&self, off: u64) -> u32 {
            unsafe { core::ptr::read_volatile((self.base + off) as *const u32) }
        }

        /// # Safety
        ///
        /// `self.base + off` must be a live MMIO register.
        unsafe fn read64(&self, off: u64) -> u64 {
            unsafe { core::ptr::read_volatile((self.base + off) as *const u64) }
        }

        /// # Safety
        ///
        /// `self.base + off` must be a live MMIO register.
        unsafe fn write32(&self, off: u64, value: u32) {
            unsafe { core::ptr::write_volatile((self.base + off) as *mut u32, value) }
        }

        /// # Safety
        ///
        /// `self.base + off` must be a live MMIO register.
        unsafe fn write64(&self, off: u64, value: u64) {
            unsafe { core::ptr::write_volatile((self.base + off) as *mut u64, value) }
        }
    }

    /// Allocate `pages` zeroed 4 KiB pages from the kernel heap and return the
    /// physical address — which, on the firmware identity map this runs under,
    /// is the allocation's address itself.
    fn alloc_table_pages(pages: usize) -> Option<u64> {
        use alloc::alloc::{Layout, alloc_zeroed};
        let bytes = pages.checked_mul(4096)?;
        let layout = Layout::from_size_align(bytes, 4096).ok()?;
        // SAFETY: `bytes` is nonzero — every caller passes at least one page —
        // and the kernel heap is installed by now.
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            return None;
        }
        Some(ptr as u64)
    }

    /// Spin until `GSTS` shows `bit`, giving up after a bounded spin so a dead
    /// unit cannot hang the boot.
    fn poll_gsts(regs: &Regs, bit: u32) -> bool {
        for _ in 0..1_000_000 {
            // SAFETY: GSTS is a live MMIO status register.
            if unsafe { regs.read32(REG_GSTS) } & bit != 0 {
                return true;
            }
            core::hint::spin_loop();
        }
        false
    }

    /// Spin until the context-cache invalidation completes, giving up after a
    /// bounded spin so a dead unit cannot hang the boot.
    fn poll_ccmd_done(regs: &Regs) -> bool {
        for _ in 0..1_000_000 {
            // SAFETY: CCMD is a live MMIO register.
            if !ccmd_pending(unsafe { regs.read64(REG_CCMD) }) {
                return true;
            }
            core::hint::spin_loop();
        }
        false
    }

    /// Spin until the IOTLB invalidation completes, giving up after a bounded
    /// spin so a dead unit cannot hang the boot.
    fn poll_iotlb_done(regs: &Regs, iotlb_invalidate: u64) -> bool {
        for _ in 0..1_000_000 {
            // SAFETY: the IOTLB invalidate register is live MMIO.
            if !iotlb_pending(unsafe { regs.read64(iotlb_invalidate) }) {
                return true;
            }
            core::hint::spin_loop();
        }
        false
    }

    /// Make finished table writes visible to the IOMMU's page-table walker.
    ///
    /// # Safety
    ///
    /// The caller must have finished writing every table.
    unsafe fn flush_tables_for_iommu(coherent: bool) {
        if coherent {
            return;
        }
        // SAFETY: ring 0; writes back and invalidates all caches so a
        // non-coherent IOMMU reads fresh tables from DRAM.
        unsafe {
            core::arch::asm!("wbinvd", options(nostack, preserves_flags));
        }
    }

    /// Write one 16-byte root/context entry.
    ///
    /// # Safety
    ///
    /// `table_phys` must be an exclusively-owned table's live physical base.
    unsafe fn write_entry(table_phys: u64, index: usize, entry: [u64; 2]) {
        let off = table_phys + (index as u64) * 16;
        // SAFETY: the caller owns the table; entries are 16-byte aligned.
        unsafe {
            core::ptr::write_volatile(off as *mut u64, entry[0]);
            core::ptr::write_volatile((off + 8) as *mut u64, entry[1]);
        }
    }

    /// Read one 16-byte root/context entry.
    ///
    /// # Safety
    ///
    /// `table_phys` must be a live table's physical base.
    unsafe fn read_entry(table_phys: u64, index: usize) -> [u64; 2] {
        let off = table_phys + (index as u64) * 16;
        // SAFETY: the caller guarantees a live table.
        unsafe {
            [
                core::ptr::read_volatile(off as *const u64),
                core::ptr::read_volatile((off + 8) as *const u64),
            ]
        }
    }

    /// Build one domain's second-stage identity map into `pages` 4 KiB pages
    /// at `base_phys`, returning the root's physical address.
    fn build_domain(
        base_phys: u64,
        pages: usize,
        map_bytes: u64,
        levels: u8,
        leaf_2mib: bool,
    ) -> Result<u64, ProgramError> {
        // SAFETY: `base_phys` is `pages` exclusively-owned zeroed pages from
        // `alloc_table_pages`.
        let buf = unsafe { core::slice::from_raw_parts_mut(base_phys as *mut u8, pages * 4096) };
        let layout = build_second_stage_identity(buf, base_phys, map_bytes, levels, leaf_2mib)
            .map_err(|_| ProgramError::TableBuildFailed)?;
        Ok(layout.root_phys)
    }

    /// What programming one remapping unit established.
    #[derive(Debug, Clone, Copy)]
    pub struct ProgramReport {
        /// Raw capability register (serial report).
        pub cap: u64,
        /// Raw extended-capability register (serial report).
        pub ecap: u64,
        /// Address width the second-stage tables use, in bits.
        pub address_width_bits: u32,
        /// Physical address of the root table.
        pub root_phys: u64,
        /// Physical address of the host domain's second-stage root.
        pub host_slpt_phys: u64,
        /// Physical address of the guest domain's second-stage root.
        pub guest_slpt_phys: u64,
        /// Bus-0 `(device, function)` moved into the guest domain, if any.
        pub guest_device: Option<(u8, u8)>,
        /// Context entries programmed on bus 0.
        pub context_entries: usize,
    }

    /// Program one VT-d remapping unit's DMA-remapping tables and enable
    /// translation.
    ///
    /// Builds the root table, the bus-0 context table (every function starts
    /// in the host domain), and two DMA domains with their own second-stage
    /// identity maps; latches the root table (`SRTP`), invalidates the
    /// context cache and IOTLB globally, enables translation (`TE`), then
    /// moves the first assignable bus-0 device into the guest domain through
    /// [`assign_device_to_domain`] — the per-guest assignment path, exercised
    /// live with invalidation. From here on, bus-0 device DMA goes through
    /// the IOMMU; devices on other buses have no root entry, so their DMA
    /// faults rather than running unconfined.
    ///
    /// # Safety
    ///
    /// `unit.register_base` must be the live, identity-mapped MMIO block of a
    /// VT-d remapping unit; the kernel heap must be installed (table
    /// allocation). Call once per unit — reprogramming a live unit is not
    /// supported.
    ///
    /// # Errors
    ///
    /// [`ProgramError`] when the unit reports no usable address width, a table
    /// allocation fails, or a command/invalidation times out — in which case
    /// translation is left however far the sequence got, and the caller must
    /// report it rather than assume confinement.
    pub unsafe fn program_unit(
        unit: &RemappingUnit,
        map_bytes: u64,
    ) -> Result<ProgramReport, ProgramError> {
        // SAFETY: the caller guarantees the register block is live MMIO.
        let regs = Regs {
            base: unit.register_base,
        };
        let cap = unsafe { regs.read64(REG_CAP) };
        let ecap = unsafe { regs.read64(REG_ECAP) };
        let aw =
            choose_address_width(cap_sagaw(cap)).ok_or(ProgramError::UnsupportedAddressWidth)?;
        let levels = aw.levels();
        let leaf_2mib = cap_super_page_2mib(cap);
        let coherent = cap_coherent(cap);

        let root_phys = alloc_table_pages(1).ok_or(ProgramError::AllocFailed)?;
        let ctx_phys = alloc_table_pages(1).ok_or(ProgramError::AllocFailed)?;
        let domain_pages = second_stage_table_count(map_bytes, levels, leaf_2mib)
            .map_err(|_| ProgramError::TableBuildFailed)?;
        let host_pages = alloc_table_pages(domain_pages).ok_or(ProgramError::AllocFailed)?;
        let guest_pages = alloc_table_pages(domain_pages).ok_or(ProgramError::AllocFailed)?;
        let host_slpt = build_domain(host_pages, domain_pages, map_bytes, levels, leaf_2mib)?;
        let guest_slpt = build_domain(guest_pages, domain_pages, map_bytes, levels, leaf_2mib)?;

        // Every bus-0 function starts in the host domain; the guest-domain
        // assignment below moves one device out.
        for devfn in 0..256usize {
            debug_assert_eq!(ctx_phys & 0xFFF, 0);
            debug_assert_eq!(host_slpt & 0xFFF, 0);
            // SAFETY: `ctx_phys` is our exclusively-owned context table.
            unsafe {
                write_entry(
                    ctx_phys,
                    devfn,
                    context_entry(HOST_DOMAIN_ID, host_slpt, aw, TranslationType::SecondStage),
                );
            }
        }
        debug_assert_eq!(root_phys & 0xFFF, 0);
        // SAFETY: `root_phys` is our exclusively-owned root table.
        unsafe {
            write_entry(root_phys, root_index(0), root_entry(ctx_phys));
        }

        // The IOMMU must see the finished tables before SRTP latches them.
        unsafe {
            flush_tables_for_iommu(coherent);
        }

        // Latch the root table, then enable translation (VT-d spec §10.4.3).
        // SAFETY: the register block is live MMIO.
        unsafe {
            regs.write64(REG_RTADDR, rtaddr_value(root_phys));
            regs.write32(REG_GCMD, GCMD_SRTP);
        }
        if !poll_gsts(&regs, GCMD_SRTP) {
            return Err(ProgramError::SrtpTimeout);
        }

        // Fresh tables, empty caches — still invalidate globally per the
        // spec's enable sequence before turning translation on.
        // SAFETY: the register block is live MMIO.
        unsafe {
            regs.write64(REG_CCMD, ccmd_global_invalidate());
        }
        if !poll_ccmd_done(&regs) {
            return Err(ProgramError::ContextInvalidateTimeout);
        }
        let iotlb_off = ecap_iotlb_reg_offset(ecap);
        // SAFETY: the register block is live MMIO.
        unsafe {
            regs.write64(iotlb_off + 8, iotlb_global_invalidate());
        }
        if !poll_iotlb_done(&regs, iotlb_off + 8) {
            return Err(ProgramError::IotlbInvalidateTimeout);
        }

        // SAFETY: the register block is live MMIO.
        unsafe {
            regs.write32(REG_GCMD, GCMD_TE);
        }
        if !poll_gsts(&regs, GCMD_TE) {
            return Err(ProgramError::TranslationEnableTimeout);
        }

        // Per-guest domain, demonstrated live: move the first assignable bus-0
        // device (past the host bridge) into the guest domain through the
        // post-enable assignment path, with invalidation.
        let guest_device = first_assignable_function();
        if let Some((device, function)) = guest_device {
            // SAFETY: the unit is programmed and translation is enabled.
            unsafe {
                assign_device_to_domain(
                    unit.register_base,
                    root_phys,
                    0,
                    device,
                    function,
                    GUEST_DOMAIN_ID,
                    guest_slpt,
                    aw,
                    iotlb_off,
                    coherent,
                )?;
            }
        }

        Ok(ProgramReport {
            cap,
            ecap,
            address_width_bits: aw.bits(),
            root_phys,
            host_slpt_phys: host_slpt,
            guest_slpt_phys: guest_slpt,
            guest_device,
            context_entries: 256,
        })
    }

    /// Move one device into a DMA domain on a live (translation-enabled) unit.
    ///
    /// Rewrites the device's context entry, flushes it for a non-coherent
    /// IOMMU, then invalidates the device's context-cache entry and the
    /// domain's IOTLB entries — the sequence §11.2.2 requires after changing
    /// an entry the hardware may have cached. This is the primitive guest
    /// bring-up will call when a passed-through device is handed to a guest.
    ///
    /// # Safety
    ///
    /// `reg_base`/`root_phys` must belong to a programmed, translation-enabled
    /// unit whose tables this code owns; `iotlb_off` must be the unit's IOTLB
    /// register-block offset ([`ecap_iotlb_reg_offset`]).
    ///
    /// # Errors
    ///
    /// [`ProgramError::NoContextTable`] when the bus has no programmed root
    /// entry; timeout variants when an invalidation never completes.
    pub unsafe fn assign_device_to_domain(
        reg_base: u64,
        root_phys: u64,
        bus: u8,
        device: u8,
        function: u8,
        did: u16,
        slpt_phys: u64,
        aw: AddressWidth,
        iotlb_off: u64,
        coherent: bool,
    ) -> Result<(), ProgramError> {
        // SAFETY: the caller guarantees a live, programmed root table.
        let root = unsafe { read_entry(root_phys, root_index(bus)) };
        if !root_entry_present(&root) {
            return Err(ProgramError::NoContextTable);
        }
        let ctx_phys = root_entry_ctp(&root);
        debug_assert_eq!(slpt_phys & 0xFFF, 0);
        // SAFETY: the context table came from our own programmed root entry.
        unsafe {
            write_entry(
                ctx_phys,
                context_index(device, function),
                context_entry(did, slpt_phys, aw, TranslationType::SecondStage),
            );
            flush_tables_for_iommu(coherent);
        }
        let devfn = (u16::from(device) << 3) | u16::from(function);
        let sid = (u16::from(bus) << 8) | devfn;
        let regs = Regs { base: reg_base };
        // SAFETY: the register block is live MMIO.
        unsafe {
            regs.write64(REG_CCMD, ccmd_device_invalidate(did, sid));
        }
        if !poll_ccmd_done(&regs) {
            return Err(ProgramError::ContextInvalidateTimeout);
        }
        // SAFETY: the register block is live MMIO.
        unsafe {
            regs.write64(iotlb_off + 8, iotlb_domain_invalidate(did));
        }
        if !poll_iotlb_done(&regs, iotlb_off + 8) {
            return Err(ProgramError::IotlbInvalidateTimeout);
        }
        Ok(())
    }

    /// Read a PCI config dword via mechanism #1.
    fn config_read(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
        // SAFETY: 0xCF8/0xCFC are the architectural PCI config ports; a
        // select then a data read is the mechanism-#1 protocol, at ring 0.
        unsafe {
            outl(
                PCI_CONFIG_ADDRESS,
                config_address(bus, device, function, offset),
            );
            inl(PCI_CONFIG_DATA)
        }
    }

    /// Write a 32-bit `value` to `port`.
    ///
    /// # Safety
    ///
    /// `port` must be a valid I/O port for a dword write at ring 0.
    unsafe fn outl(port: u16, value: u32) {
        unsafe {
            core::arch::asm!(
                "out dx, eax",
                in("dx") port,
                in("eax") value,
                options(nomem, nostack, preserves_flags)
            );
        }
    }

    /// Read a 32-bit value from `port`.
    ///
    /// # Safety
    ///
    /// `port` must be a valid I/O port for a dword read at ring 0.
    unsafe fn inl(port: u16) -> u32 {
        let value: u32;
        unsafe {
            core::arch::asm!(
                "in eax, dx",
                out("eax") value,
                in("dx") port,
                options(nomem, nostack, preserves_flags)
            );
        }
        value
    }

    /// The first present bus-0 function past the host bridge — the device the
    /// bring-up moves into the guest domain to demonstrate per-guest
    /// assignment. Probes config dword 0; an all-ones vendor means absent
    /// (the same probe as [`crate::pci`]'s bus-0 scan).
    fn first_assignable_function() -> Option<(u8, u8)> {
        for device in 1u8..32 {
            for function in 0u8..8 {
                if vendor_present(vendor_of(config_read(0, device, function, 0))) {
                    return Some((device, function));
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_map_covers_ram_and_the_mmio_window() {
        const GIB: u64 = 1024 * 1024 * 1024;
        const MIB: u64 = 1024 * 1024;
        // No usable RAM reported — still map the 4 GiB MMIO window.
        assert_eq!(identity_map_bytes(0), 4 * GIB);
        // 2 GiB of RAM — the window dominates.
        assert_eq!(identity_map_bytes(2 * GIB), 4 * GIB);
        // RAM past 4 GiB extends the map, 2 MiB-aligned up.
        assert_eq!(identity_map_bytes(5 * GIB), 5 * GIB);
        assert_eq!(identity_map_bytes(5 * GIB + 1), 5 * GIB + 2 * MIB);
    }

    #[test]
    fn program_error_names_are_stable() {
        for err in [
            ProgramError::UnsupportedAddressWidth,
            ProgramError::AllocFailed,
            ProgramError::TableBuildFailed,
            ProgramError::SrtpTimeout,
            ProgramError::TranslationEnableTimeout,
            ProgramError::ContextInvalidateTimeout,
            ProgramError::IotlbInvalidateTimeout,
            ProgramError::NoContextTable,
        ] {
            assert!(!err.name().is_empty(), "{err:?}");
        }
        assert_eq!(
            ProgramError::UnsupportedAddressWidth.name(),
            "no usable AGAW in CAP.SAGAW"
        );
    }

    #[test]
    fn domain_ids_avoid_the_reserved_zero() {
        // Under Caching Mode domain ID 0 is architecturally reserved.
        assert_ne!(HOST_DOMAIN_ID, 0);
        assert_ne!(GUEST_DOMAIN_ID, 0);
        assert_ne!(HOST_DOMAIN_ID, GUEST_DOMAIN_ID);
    }
}
