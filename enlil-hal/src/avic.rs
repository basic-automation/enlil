//! AMD Advanced Virtual Interrupt Controller (AVIC).
//!
//! AVIC virtualizes the guest's local APIC in hardware, removing the VM
//! exits that software LAPIC emulation needs for TPR/EOI/ICR accesses and
//! for interrupt delivery:
//!
//! ```text
//!  Per-vCPU AVIC backing page (4 KiB)
//!      Holds the guest's virtual APIC state (TPR, EOI, IRR/ISR/TMR, IER, …)
//!      at the same offsets as the xAPIC MMIO page. The processor delivers
//!      interrupts straight from this page while the vCPU runs.
//!  AVIC logical APIC ID table (per VM)
//!      Maps guest *logical* APIC IDs to guest *physical* APIC IDs.
//!  AVIC physical APIC ID table (per VM)
//!      Maps guest physical APIC IDs to host physical APIC IDs, with an
//!      IsRunning bit per entry so the doorbell knows whom to signal.
//!  AVIC doorbell (MSR C001_011Bh)
//!      To post an interrupt the hypervisor sets the IRR bit in the target's
//!      backing page; if that vCPU's table entry says IsRunning, it writes
//!      the guest physical APIC ID to the doorbell MSR and the processor
//!      re-evaluates the backing page. A vCPU that is not running simply
//!      finds the IRR bit pending at its next VMRUN — no doorbell needed.
//! ```
//!
//! Enablement is one VMCB bit ([`INT_CTL_AVIC_ENABLE`](super::svm::control::INT_CTL_AVIC_ENABLE)
//! in [`INT_CONTROL`](super::svm::control::INT_CONTROL)) plus the three
//! table/page pointers programmed by [`arm_avic`]. As with the rest of the
//! HAL, the privileged operations (`CPUID`, `WRMSR` to the doorbell, `VMRUN`)
//! are the bare-metal backend's job; everything here is host-testable. See
//! `docs/src/apic-acceleration.md` for the hardware-gated end-to-end plan.
//!
//! References: AMD APM Vol. 2 §15.29 (AVIC), Appendix B Table B-1 (VMCB).

use alloc::boxed::Box;

use super::svm::{VMCB_SIZE, control};

// ---------------------------------------------------------------------------
// Doorbell MSR
// ---------------------------------------------------------------------------

/// The `AVIC` doorbell MSR.
///
/// The backend writes the target's guest physical APIC ID here to signal a
/// running vCPU that its backing page has pending interrupts. Advertised by
/// `CPUID Fn8000_000A EDX` bit 13 ([`AVIC`](super::svm::feature::AVIC)).
pub const MSR_AMD64_AVIC_DOORBELL: u32 = 0xC001_011B;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why `AVIC` arming failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvicError {
    /// The VMCB region was smaller than [`VMCB_SIZE`].
    VmcbTooSmall,
    /// A page/table address was not 4 KiB aligned (VM entry requires bits
    /// 11:0 clear).
    UnalignedAddress {
        /// Which address (for diagnostics).
        what: &'static str,
    },
}

impl core::fmt::Display for AvicError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::VmcbTooSmall => write!(f, "VMCB region smaller than {VMCB_SIZE} bytes"),
            Self::UnalignedAddress { what } => write!(f, "{what} is not 4 KiB aligned"),
        }
    }
}

impl core::error::Error for AvicError {}

// ---------------------------------------------------------------------------
// Backing page
// ---------------------------------------------------------------------------

/// Size of an `AVIC` backing page in bytes.
pub const AVIC_BACKING_PAGE_SIZE: usize = 4096;

/// Register offsets inside the `AVIC` backing page — the same offsets as the
/// xAPIC MMIO page (AMD APM Vol. 2 §15.29).
pub mod backing {
    /// APIC ID register.
    pub const APIC_ID: usize = 0x020;
    /// Task-priority register.
    pub const TPR: usize = 0x080;
    /// Processor-priority register.
    pub const PPR: usize = 0x0A0;
    /// End-of-interrupt register.
    pub const EOI: usize = 0x0B0;
    /// In-service registers (8 × u32).
    pub const ISR_BASE: usize = 0x100;
    /// Trigger-mode registers (8 × u32).
    pub const TMR_BASE: usize = 0x180;
    /// Interrupt-request registers (8 × u32).
    pub const IRR_BASE: usize = 0x200;
    /// Interrupt-command register, low dword.
    pub const ICR_LO: usize = 0x300;
    /// Interrupt-command register, high dword.
    pub const ICR_HI: usize = 0x310;
    /// Interrupt-enable registers (8 × u32).
    pub const IER_BASE: usize = 0x480;
}

/// Number of APIC ID table entries (8-bit APIC IDs).
pub const AVIC_TABLE_ENTRIES: usize = 256;

fn load_u32(page: &[u8], offset: usize) -> u32 {
    let bytes: [u8; 4] = page
        .get(offset..offset + 4)
        .and_then(|s| s.try_into().ok())
        .unwrap_or([0; 4]);
    u32::from_le_bytes(bytes)
}

fn store_u32(page: &mut [u8], offset: usize, value: u32) {
    if let Some(slot) = page.get_mut(offset..offset + 4) {
        slot.copy_from_slice(&value.to_le_bytes());
    }
}

/// A vCPU's `AVIC` backing page: the guest's virtual APIC state.
///
/// The processor reads/writes this page directly while the vCPU runs; the
/// hypervisor posts interrupts by setting IRR bits and reads TPR/EOI for
/// diagnostics. The backend maps the page and programs its physical address
/// into the VMCB via [`arm_avic`].
#[derive(Debug, Clone)]
pub struct AvicBackingPage(Box<[u8; AVIC_BACKING_PAGE_SIZE]>);

impl AvicBackingPage {
    /// A zeroed backing page.
    #[must_use]
    pub fn new() -> Self {
        Self(Box::new([0; AVIC_BACKING_PAGE_SIZE]))
    }

    /// Raw page bytes (for the backend's page-table mapping).
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; AVIC_BACKING_PAGE_SIZE] {
        &self.0
    }

    /// Mutable raw page bytes.
    pub fn as_bytes_mut(&mut self) -> &mut [u8; AVIC_BACKING_PAGE_SIZE] {
        &mut self.0
    }

    /// Set the interrupt-request bit for `vector` (the post primitive).
    pub fn irr_set(&mut self, vector: u8) {
        let (word, bit) = vector_word_bit(vector, backing::IRR_BASE);
        let cur = load_u32(&self.0[..], word);
        store_u32(&mut self.0[..], word, cur | (1 << bit));
    }

    /// Clear the interrupt-request bit for `vector`.
    pub fn irr_clear(&mut self, vector: u8) {
        let (word, bit) = vector_word_bit(vector, backing::IRR_BASE);
        let cur = load_u32(&self.0[..], word);
        store_u32(&mut self.0[..], word, cur & !(1 << bit));
    }

    /// Whether the interrupt-request bit for `vector` is set.
    #[must_use]
    pub fn irr_test(&self, vector: u8) -> bool {
        let (word, bit) = vector_word_bit(vector, backing::IRR_BASE);
        load_u32(&self.0[..], word) & (1 << bit) != 0
    }

    /// Highest pending IRR vector, if any.
    #[must_use]
    pub fn irr_highest_pending(&self) -> Option<u8> {
        for i in (0..8).rev() {
            let word = load_u32(&self.0[..], backing::IRR_BASE + i * 4);
            if word != 0 {
                let base = u8::try_from(i).ok()?.checked_mul(32)?;
                let bit = u8::try_from(word.ilog2()).ok()?;
                return base.checked_add(bit);
            }
        }
        None
    }

    /// Read the task-priority register (bits 7:0).
    #[must_use]
    pub fn tpr(&self) -> u8 {
        (load_u32(&self.0[..], backing::TPR) & 0xFF) as u8
    }

    /// Write the task-priority register (bits 7:0).
    pub fn set_tpr(&mut self, tpr: u8) {
        let cur = load_u32(&self.0[..], backing::TPR);
        store_u32(
            &mut self.0[..],
            backing::TPR,
            (cur & !0xFF) | u32::from(tpr),
        );
    }

    /// Read the APIC ID (bits 31:24).
    #[must_use]
    pub fn apic_id(&self) -> u8 {
        ((load_u32(&self.0[..], backing::APIC_ID) >> 24) & 0xFF) as u8
    }

    /// Write the APIC ID (bits 31:24).
    pub fn set_apic_id(&mut self, apic_id: u8) {
        let cur = load_u32(&self.0[..], backing::APIC_ID);
        store_u32(
            &mut self.0[..],
            backing::APIC_ID,
            (cur & 0x00FF_FFFF) | (u32::from(apic_id) << 24),
        );
    }
}

impl Default for AvicBackingPage {
    fn default() -> Self {
        Self::new()
    }
}

/// `(register offset, bit)` of `vector` in an 8-word bitmap at `base`.
const fn vector_word_bit(vector: u8, base: usize) -> (usize, u32) {
    let word = base + ((vector as usize) >> 5) * 4;
    let bit = (vector & 31) as u32;
    (word, bit)
}

// ---------------------------------------------------------------------------
// APIC ID tables
// ---------------------------------------------------------------------------

/// A logical APIC ID table entry: bits 7:0 hold the guest *physical* APIC ID
/// the logical ID maps to; bit 31 marks the entry valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvicLogicalIdEntry(u32);

impl AvicLogicalIdEntry {
    /// Valid bit (bit 31).
    pub const VALID: u32 = 1 << 31;

    /// Build a valid entry mapping to `guest_physical_id`.
    #[must_use]
    pub const fn new(guest_physical_id: u8) -> Self {
        Self(Self::VALID | (guest_physical_id as u32))
    }

    /// An invalid (unmapped) entry.
    #[must_use]
    pub const fn invalid() -> Self {
        Self(0)
    }

    /// Raw entry value for the table page.
    #[must_use]
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Wrap a raw table value.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Whether the entry is valid.
    #[must_use]
    pub const fn valid(self) -> bool {
        self.0 & Self::VALID != 0
    }

    /// The guest physical APIC ID this logical ID maps to.
    #[must_use]
    pub const fn guest_physical_id(self) -> u8 {
        (self.0 & 0xFF) as u8
    }
}

/// A physical APIC ID table entry: bits 7:0 hold the *host* physical APIC ID
/// of the CPU running the vCPU, bit 62 is `IsRunning`, bit 63 is valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvicPhysicalIdEntry(u64);

impl AvicPhysicalIdEntry {
    /// Valid bit (bit 63).
    pub const VALID: u64 = 1 << 63;
    /// `IsRunning` bit (bit 62): the vCPU is currently executing on the host
    /// CPU in bits 7:0.
    pub const IS_RUNNING: u64 = 1 << 62;

    /// Build a valid entry for a vCPU on `host_apic_id`, not yet running.
    #[must_use]
    pub const fn new(host_apic_id: u8) -> Self {
        Self(Self::VALID | (host_apic_id as u64))
    }

    /// An invalid entry (no vCPU with this guest physical APIC ID).
    #[must_use]
    pub const fn invalid() -> Self {
        Self(0)
    }

    /// Raw entry value for the table page.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Wrap a raw table value.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Whether the entry is valid.
    #[must_use]
    pub const fn valid(self) -> bool {
        self.0 & Self::VALID != 0
    }

    /// Whether the vCPU is currently running on the host CPU.
    #[must_use]
    pub const fn is_running(self) -> bool {
        self.0 & Self::IS_RUNNING != 0
    }

    /// Set or clear `IsRunning`, preserving the other fields.
    #[must_use]
    pub const fn with_running(self, running: bool) -> Self {
        if running {
            Self(self.0 | Self::IS_RUNNING)
        } else {
            Self(self.0 & !Self::IS_RUNNING)
        }
    }

    /// The host physical APIC ID of the CPU running (or last running) the vCPU.
    #[must_use]
    pub const fn host_physical_id(self) -> u8 {
        (self.0 & 0xFF) as u8
    }
}

/// The per-VM `AVIC` tables: the logical ID table (guest logical → guest
/// physical APIC ID) and the physical ID table (guest physical → host
/// physical APIC ID + `IsRunning`).
///
/// The backend maps both tables and programs their physical addresses into
/// every vCPU's VMCB via [`arm_avic`].
#[derive(Debug, Clone)]
pub struct AvicTables {
    logical: [AvicLogicalIdEntry; AVIC_TABLE_ENTRIES],
    physical: [AvicPhysicalIdEntry; AVIC_TABLE_ENTRIES],
}

impl AvicTables {
    /// Empty tables: every entry invalid.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            logical: [AvicLogicalIdEntry::invalid(); AVIC_TABLE_ENTRIES],
            physical: [AvicPhysicalIdEntry::invalid(); AVIC_TABLE_ENTRIES],
        }
    }

    /// Bind `guest_apic_id` to the host CPU `host_apic_id`.
    ///
    /// Marks the entry valid but not running; the backend calls
    /// [`set_running`](Self::set_running) around `VMRUN`.
    pub fn assign_vcpu(&mut self, guest_apic_id: u8, host_apic_id: u8) {
        self.physical[usize::from(guest_apic_id)] = AvicPhysicalIdEntry::new(host_apic_id);
    }

    /// Update the `IsRunning` bit for `guest_apic_id` (no-op on invalid entries).
    pub fn set_running(&mut self, guest_apic_id: u8, running: bool) {
        let slot = &mut self.physical[usize::from(guest_apic_id)];
        if slot.valid() {
            *slot = slot.with_running(running);
        }
    }

    /// Move a vCPU to another host CPU, preserving validity/running state.
    pub fn migrate_vcpu(&mut self, guest_apic_id: u8, host_apic_id: u8) {
        let slot = &mut self.physical[usize::from(guest_apic_id)];
        if slot.valid() {
            let running = slot.is_running();
            *slot = AvicPhysicalIdEntry::new(host_apic_id).with_running(running);
        }
    }

    /// The physical ID entry for `guest_apic_id`.
    #[must_use]
    pub fn physical_entry(&self, guest_apic_id: u8) -> AvicPhysicalIdEntry {
        self.physical[usize::from(guest_apic_id)]
    }

    /// Map a guest *logical* APIC ID to a guest *physical* APIC ID.
    pub fn map_logical(&mut self, logical_id: u8, guest_physical_id: u8) {
        self.logical[usize::from(logical_id)] = AvicLogicalIdEntry::new(guest_physical_id);
    }

    /// The logical ID entry for `logical_id`.
    #[must_use]
    pub fn logical_entry(&self, logical_id: u8) -> AvicLogicalIdEntry {
        self.logical[usize::from(logical_id)]
    }

    /// Raw logical table words (for the backend's table page).
    #[must_use]
    pub const fn logical_raw(&self) -> &[AvicLogicalIdEntry; AVIC_TABLE_ENTRIES] {
        &self.logical
    }

    /// Raw physical table words (for the backend's table page).
    #[must_use]
    pub const fn physical_raw(&self) -> &[AvicPhysicalIdEntry; AVIC_TABLE_ENTRIES] {
        &self.physical
    }
}

impl Default for AvicTables {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// VMCB arming
// ---------------------------------------------------------------------------

fn load_u64(region: &[u8], offset: usize) -> u64 {
    let bytes: [u8; 8] = region
        .get(offset..offset + 8)
        .and_then(|s| s.try_into().ok())
        .unwrap_or([0; 8]);
    u64::from_le_bytes(bytes)
}

fn store_u64(region: &mut [u8], offset: usize, value: u64) {
    if let Some(slot) = region.get_mut(offset..offset + 8) {
        slot.copy_from_slice(&value.to_le_bytes());
    }
}

/// Arm `AVIC` on a programmed VMCB region.
///
/// Writes the backing-page, logical-ID-table, and physical-ID-table physical
/// addresses to [`AVIC_BACKING_PAGE_PTR`](control::AVIC_BACKING_PAGE_PTR),
/// [`AVIC_LOGICAL_ID_TABLE_PTR`](control::AVIC_LOGICAL_ID_TABLE_PTR), and
/// [`AVIC_PHYSICAL_ID_TABLE_PTR`](control::AVIC_PHYSICAL_ID_TABLE_PTR), and
/// sets the `AVIC`-enable bit in
/// [`INT_CONTROL`](control::INT_CONTROL) (preserving the other bits).
/// Mirrors the [`arm_port_intercepts`](super::svm::arm_port_intercepts) style:
/// the caller owns the region bytes; only the byte layout is handled here.
///
/// # Errors
///
/// Returns [`AvicError::VmcbTooSmall`] when `vmcb` is shorter than
/// [`VMCB_SIZE`], or [`AvicError::UnalignedAddress`] when an address is not
/// 4 KiB aligned.
pub fn arm_avic(
    vmcb: &mut [u8],
    backing_page_pa: u64,
    logical_id_table_pa: u64,
    physical_id_table_pa: u64,
) -> Result<(), AvicError> {
    if vmcb.len() < VMCB_SIZE {
        return Err(AvicError::VmcbTooSmall);
    }
    for (what, pa) in [
        ("AVIC backing page", backing_page_pa),
        ("AVIC logical ID table", logical_id_table_pa),
        ("AVIC physical ID table", physical_id_table_pa),
    ] {
        if pa & 0xFFF != 0 {
            return Err(AvicError::UnalignedAddress { what });
        }
    }
    store_u64(vmcb, control::AVIC_BACKING_PAGE_PTR, backing_page_pa);
    store_u64(
        vmcb,
        control::AVIC_LOGICAL_ID_TABLE_PTR,
        logical_id_table_pa,
    );
    store_u64(
        vmcb,
        control::AVIC_PHYSICAL_ID_TABLE_PTR,
        physical_id_table_pa,
    );
    let int_ctl = load_u64(vmcb, control::INT_CONTROL) | control::INT_CTL_AVIC_ENABLE;
    store_u64(vmcb, control::INT_CONTROL, int_ctl);
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-vCPU state
// ---------------------------------------------------------------------------

/// What posting an interrupt decided about the doorbell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvicPostAction {
    /// The target vCPU is running: the IRR bit is set; the backend must ring
    /// the doorbell by writing the guest APIC ID to
    /// [`MSR_AMD64_AVIC_DOORBELL`].
    Doorbell {
        /// Guest physical APIC ID to write to the doorbell MSR.
        guest_apic_id: u8,
    },
    /// The target vCPU is not running (or has no table entry): the IRR bit
    /// is set and will be delivered at the next `VMRUN`; no doorbell.
    Pending,
}

/// Owned per-vCPU `AVIC` state: the backing page plus the guest physical APIC
/// ID that indexes the VM's physical ID table.
#[derive(Debug, Clone)]
pub struct AvicVcpu {
    guest_apic_id: u8,
    backing: AvicBackingPage,
}

impl AvicVcpu {
    /// Allocate zeroed per-vCPU state for `guest_apic_id`.
    #[must_use]
    pub fn new(guest_apic_id: u8) -> Self {
        let mut backing = AvicBackingPage::new();
        backing.set_apic_id(guest_apic_id);
        Self {
            guest_apic_id,
            backing,
        }
    }

    /// The guest physical APIC ID of this vCPU.
    #[must_use]
    pub const fn guest_apic_id(&self) -> u8 {
        self.guest_apic_id
    }

    /// Rebind this vCPU to another guest physical APIC ID (e.g. on topology
    /// change or re-placement). Updates the backing page's APIC ID register
    /// to match so the guest still sees its own ID.
    pub fn set_guest_apic_id(&mut self, guest_apic_id: u8) {
        self.guest_apic_id = guest_apic_id;
        self.backing.set_apic_id(guest_apic_id);
    }

    /// The backing page.
    #[must_use]
    pub const fn backing(&self) -> &AvicBackingPage {
        &self.backing
    }

    /// Mutable access to the backing page.
    pub const fn backing_mut(&mut self) -> &mut AvicBackingPage {
        &mut self.backing
    }

    /// Post `vector` to this vCPU: set the IRR bit in the backing page, then
    /// consult the physical ID table — ring the doorbell only if the vCPU is
    /// currently running ([`AvicPostAction`]).
    pub fn post(&mut self, tables: &AvicTables, vector: u8) -> AvicPostAction {
        self.backing.irr_set(vector);
        let entry = tables.physical_entry(self.guest_apic_id);
        if entry.valid() && entry.is_running() {
            AvicPostAction::Doorbell {
                guest_apic_id: self.guest_apic_id,
            }
        } else {
            AvicPostAction::Pending
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::svm::feature;
    use super::*;
    use alloc::string::ToString;

    // -- Table entries -----------------------------------------------------

    #[test]
    fn logical_id_entry_round_trip() {
        let e = AvicLogicalIdEntry::new(0x2A);
        assert!(e.valid());
        assert_eq!(e.guest_physical_id(), 0x2A);
        assert_eq!(AvicLogicalIdEntry::from_raw(e.raw()), e);

        let inv = AvicLogicalIdEntry::invalid();
        assert!(!inv.valid());

        // Bit layout: valid = bit 31, id = bits 7:0.
        assert_eq!(AvicLogicalIdEntry::new(0x01).raw(), (1 << 31) | 0x01);
    }

    #[test]
    fn physical_id_entry_round_trip() {
        let e = AvicPhysicalIdEntry::new(0x07);
        assert!(e.valid());
        assert!(!e.is_running());
        assert_eq!(e.host_physical_id(), 0x07);

        let running = e.with_running(true);
        assert!(running.is_running());
        assert!(running.valid());
        assert_eq!(running.host_physical_id(), 0x07);

        let stopped = running.with_running(false);
        assert!(!stopped.is_running());
        assert_eq!(stopped, e);

        assert_eq!(AvicPhysicalIdEntry::from_raw(running.raw()), running);
        assert!(!AvicPhysicalIdEntry::invalid().valid());

        // Bit layout: valid = bit 63, IsRunning = bit 62, id = bits 7:0.
        assert_eq!(AvicPhysicalIdEntry::new(0x03).raw(), (1 << 63) | 0x03);
        assert_eq!(
            AvicPhysicalIdEntry::new(0x03).with_running(true).raw(),
            (1 << 63) | (1 << 62) | 0x03
        );
    }

    // -- Tables ------------------------------------------------------------

    #[test]
    fn tables_assign_run_migrate() {
        let mut t = AvicTables::new();
        assert!(!t.physical_entry(3).valid());

        t.assign_vcpu(3, 11);
        let e = t.physical_entry(3);
        assert!(e.valid());
        assert!(!e.is_running());
        assert_eq!(e.host_physical_id(), 11);

        t.set_running(3, true);
        assert!(t.physical_entry(3).is_running());
        t.set_running(3, false);
        assert!(!t.physical_entry(3).is_running());

        // set_running on an invalid entry is a no-op, not a phantom entry.
        t.set_running(9, true);
        assert!(!t.physical_entry(9).valid());

        t.set_running(3, true);
        t.migrate_vcpu(3, 14);
        let m = t.physical_entry(3);
        assert_eq!(m.host_physical_id(), 14);
        assert!(m.is_running()); // running state preserved across migration

        t.map_logical(0xF0, 3);
        let l = t.logical_entry(0xF0);
        assert!(l.valid());
        assert_eq!(l.guest_physical_id(), 3);
        assert!(!t.logical_entry(0x0F).valid());
    }

    #[test]
    fn tables_raw_views_cover_all_entries() {
        let mut t = AvicTables::new();
        t.assign_vcpu(0, 1);
        t.assign_vcpu(255, 2);
        assert_eq!(t.physical_raw().len(), AVIC_TABLE_ENTRIES);
        assert_eq!(t.logical_raw().len(), AVIC_TABLE_ENTRIES);
        assert!(t.physical_raw()[0].valid());
        assert!(t.physical_raw()[255].valid());
        assert!(!t.physical_raw()[128].valid());
    }

    // -- Backing page ------------------------------------------------------

    #[test]
    fn backing_page_irr_ops() {
        let mut p = AvicBackingPage::new();
        assert_eq!(p.irr_highest_pending(), None);

        p.irr_set(0);
        p.irr_set(31);
        p.irr_set(32);
        p.irr_set(255);
        assert!(p.irr_test(0));
        assert!(p.irr_test(255));
        assert!(!p.irr_test(1));
        assert_eq!(p.irr_highest_pending(), Some(255));

        p.irr_clear(255);
        p.irr_clear(32);
        assert_eq!(p.irr_highest_pending(), Some(31));
        assert!(!p.irr_test(32));

        // IRR lives at 0x200..0x270; neighboring registers are untouched.
        assert_eq!(p.tpr(), 0);
    }

    #[test]
    fn backing_page_tpr_and_apic_id() {
        let mut p = AvicBackingPage::new();
        p.set_tpr(0x50);
        p.set_apic_id(0x0B);
        assert_eq!(p.tpr(), 0x50);
        assert_eq!(p.apic_id(), 0x0B);
        // APIC ID occupies bits 31:24 only.
        let raw = load_u32(p.as_bytes(), backing::APIC_ID);
        assert_eq!(raw, 0x0B00_0000);
    }

    #[test]
    fn backing_page_is_4k() {
        assert_eq!(
            AvicBackingPage::new().as_bytes().len(),
            AVIC_BACKING_PAGE_SIZE
        );
    }

    // -- VMCB arming -------------------------------------------------------

    #[test]
    fn arm_avic_programs_vmcb() {
        let mut vmcb = [0u8; VMCB_SIZE];
        // Pre-seed INT_CONTROL with unrelated bits; arming must preserve them.
        store_u64(&mut vmcb, control::INT_CONTROL, 0x0000_00FF_0000_0000);
        arm_avic(&mut vmcb, 0x1_0000, 0x2_0000, 0x3_0000).unwrap();

        assert_eq!(load_u64(&vmcb, control::AVIC_BACKING_PAGE_PTR), 0x1_0000);
        assert_eq!(
            load_u64(&vmcb, control::AVIC_LOGICAL_ID_TABLE_PTR),
            0x2_0000
        );
        assert_eq!(
            load_u64(&vmcb, control::AVIC_PHYSICAL_ID_TABLE_PTR),
            0x3_0000
        );
        let int_ctl = load_u64(&vmcb, control::INT_CONTROL);
        assert_eq!(
            int_ctl & control::INT_CTL_AVIC_ENABLE,
            control::INT_CTL_AVIC_ENABLE
        );
        assert_eq!(int_ctl & 0x0000_00FF_0000_0000, 0x0000_00FF_0000_0000);

        // Offsets match AMD APM Appendix B Table B-1.
        assert_eq!(control::AVIC_BACKING_PAGE_PTR, 0xF0);
        assert_eq!(control::AVIC_LOGICAL_ID_TABLE_PTR, 0xF8);
        assert_eq!(control::AVIC_PHYSICAL_ID_TABLE_PTR, 0x100);
    }

    #[test]
    fn arm_avic_rejects_bad_input() {
        let mut short = [0u8; 128];
        assert_eq!(
            arm_avic(&mut short, 0x1000, 0x2000, 0x3000),
            Err(AvicError::VmcbTooSmall)
        );
        let mut vmcb = [0u8; VMCB_SIZE];
        assert!(matches!(
            arm_avic(&mut vmcb, 0x1001, 0x2000, 0x3000),
            Err(AvicError::UnalignedAddress { .. })
        ));
        assert!(matches!(
            arm_avic(&mut vmcb, 0x1000, 0x2000, 0x3001),
            Err(AvicError::UnalignedAddress { .. })
        ));
    }

    // -- Post / doorbell protocol -------------------------------------------

    #[test]
    fn post_doorbells_running_vcpu_only() {
        let mut tables = AvicTables::new();
        tables.assign_vcpu(2, 5);
        let mut vcpu = AvicVcpu::new(2);
        assert_eq!(vcpu.guest_apic_id(), 2);

        // Not running: IRR recorded, no doorbell.
        assert_eq!(vcpu.post(&tables, 0x40), AvicPostAction::Pending);
        assert!(vcpu.backing().irr_test(0x40));

        // Running: IRR recorded + doorbell for this guest APIC ID.
        tables.set_running(2, true);
        assert_eq!(
            vcpu.post(&tables, 0x41),
            AvicPostAction::Doorbell { guest_apic_id: 2 }
        );
        assert!(vcpu.backing().irr_test(0x41));

        // Unknown APIC ID (no table entry): pending, never a doorbell.
        let mut orphan = AvicVcpu::new(9);
        assert_eq!(orphan.post(&tables, 0x42), AvicPostAction::Pending);
    }

    #[test]
    fn vcpu_rebind_updates_backing_page_id() {
        let mut vcpu = AvicVcpu::new(2);
        assert_eq!(vcpu.backing().apic_id(), 2);
        vcpu.set_guest_apic_id(9);
        assert_eq!(vcpu.guest_apic_id(), 9);
        assert_eq!(vcpu.backing().apic_id(), 9);
    }

    // -- Capability wiring ---------------------------------------------------

    #[test]
    fn avic_feature_bit_is_wired() {
        // Guards the AVIC enablement path against feature-bit drift.
        assert_eq!(feature::AVIC, 1 << 13);
        assert_eq!(MSR_AMD64_AVIC_DOORBELL, 0xC001_011B);
        assert_eq!(control::INT_CTL_AVIC_ENABLE, 1 << 13);
    }

    // -- Error display -------------------------------------------------------

    #[test]
    fn avic_error_display() {
        assert!(AvicError::VmcbTooSmall.to_string().contains("VMCB"));
        assert!(
            AvicError::UnalignedAddress {
                what: "AVIC backing page"
            }
            .to_string()
            .contains("4 KiB aligned")
        );
    }
}
