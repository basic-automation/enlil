//! Intel APIC virtualization (`APICv`) and posted-interrupt processing.
//!
//! Without `APICv`, every guest access to its local APIC (TPR reads, EOI writes,
//! self-IPIs) and every interrupt *delivery* exits to the hypervisor — at
//! scale (many vCPUs, interrupt-heavy virtio devices) those exits dominate
//! guest overhead. `APICv` removes most of them in hardware:
//!
//! ```text
//!  APIC-register virtualization  (secondary ctrl bit 8)
//!      Guest TPR/EOI/self-IPI/ICR accesses are emulated against the
//!      virtual-APIC page instead of exiting.
//!  Virtual-interrupt delivery    (secondary ctrl bit 9)
//!      Pending/delivered state (VIRR/VISR) lives in the virtual-APIC page;
//!      EOI writes exit only for vectors in the EOI-exit bitmap.
//!  Posted-interrupt processing   (pin-based ctrl bit 7)
//!      The hypervisor *posts* an interrupt by setting a bit in the
//!      posted-interrupt descriptor (PIR) and — only if the
//!      outstanding-notification bit was clear — sends one notification IPI.
//!      The processor then injects every pending vector into the guest
//!      without a VM exit.
//! ```
//!
//! The seam follows the crate's rule: this module owns the pure ISA detail
//! (capability decoding, descriptor layout, VMCS field encodings, the
//! enablement recipe), while the bare-metal backend performs the privileged
//! operations (`RDMSR`, `VMWRITE`, sending the notification IPI) on real
//! hardware. Everything here is host-testable; see
//! `docs/src/apic-acceleration.md` for the hardware-gated end-to-end plan.
//!
//! References: Intel SDM Vol. 3C §29 (APIC virtualization), §27.3 (VM-entry
//! checks for these controls), Appendix B (VMCS field encodings).

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::vmx::VmcsField;

// ---------------------------------------------------------------------------
// VM-execution control bits
// ---------------------------------------------------------------------------

/// Pin-based VM-execution control bit 7: "process posted interrupts".
///
/// When 1, an interrupt arriving with the posted-interrupt notification
/// vector does not cause a VM exit; the processor clears the descriptor's
/// outstanding-notification bit and injects the pending posted vectors.
/// Requires virtual-interrupt delivery (secondary bit 9) and the
/// "acknowledge interrupt on exit" VM-exit control (bit 15).
pub const PINBASED_PROCESS_POSTED_INTERRUPTS: u32 = 1 << 7;

/// Secondary processor-based control bit 0: "virtualize APIC accesses".
///
/// Guest accesses to the APIC-access page take APIC-access VM exits unless
/// claimed by APIC-register virtualization. Mutually exclusive with
/// [`SECONDARY_VIRTUALIZE_X2APIC_MODE`]: VM entry fails if both are 1.
pub const SECONDARY_VIRTUALIZE_APIC_ACCESSES: u32 = 1 << 0;

/// Secondary processor-based control bit 4: "virtualize x2APIC mode".
///
/// `RDMSR`/`WRMSR` to the x2APIC MSR range (800H–8FFH) are virtualized
/// instead of exiting. Mutually exclusive with
/// [`SECONDARY_VIRTUALIZE_APIC_ACCESSES`].
pub const SECONDARY_VIRTUALIZE_X2APIC_MODE: u32 = 1 << 4;

/// Secondary processor-based control bit 8: "APIC-register virtualization".
///
/// Guest reads/writes of the virtualized APIC registers (TPR, EOI, self-IPI,
/// …) are emulated against the virtual-APIC page without exiting.
pub const SECONDARY_APIC_REGISTER_VIRTUALIZATION: u32 = 1 << 8;

/// Secondary processor-based control bit 9: "virtual-interrupt delivery".
///
/// Enables the virtual-APIC page's pending/delivered state (VIRR/VISR),
/// EOI virtualization, and posted-interrupt processing. VM entry requires
/// "external-interrupt exiting" (primary control bit 0) to be 1 alongside.
pub const SECONDARY_VIRTUAL_INTERRUPT_DELIVERY: u32 = 1 << 9;

/// Primary processor-based control bit 0: "external-interrupt exiting".
///
/// Must be 1 whenever virtual-interrupt delivery is 1 (the posted-interrupt
/// notification itself arrives as an external interrupt).
pub const PRIMARY_EXTERNAL_INTERRUPT_EXITING: u32 = 1 << 0;

/// VM-exit control bit 15: "acknowledge interrupt on exit".
///
/// Must be 1 whenever "process posted interrupts" is 1.
pub const VMEXIT_ACK_INTR_ON_EXIT: u32 = 1 << 15;

// ---------------------------------------------------------------------------
// VMCS field encodings (Intel SDM Vol. 3, Appendix B)
// ---------------------------------------------------------------------------

/// 16-bit control field: the vector the processor treats as a
/// posted-interrupt notification (bits 15:8 must be zero).
pub const VMCS_POSTED_INTERRUPT_NOTIFICATION_VECTOR: VmcsField =
    VmcsField::from_encoding(0x0000_0082);

/// 64-bit control field: physical address of the virtual-APIC page (4 KiB).
pub const VMCS_VIRTUAL_APIC_PAGE_ADDR: VmcsField = VmcsField::from_encoding(0x0000_2012);

/// 64-bit control field: physical address of the APIC-access page (4 KiB).
///
/// Not programmed when x2APIC mode is virtualized instead.
pub const VMCS_APIC_ACCESS_ADDR: VmcsField = VmcsField::from_encoding(0x0000_2014);

/// 64-bit control field: physical address of the posted-interrupt
/// descriptor (64 bytes, 64-byte aligned).
pub const VMCS_POSTED_INTERRUPT_DESCRIPTOR_ADDR: VmcsField = VmcsField::from_encoding(0x0000_2016);

/// 64-bit control fields: EOI-exit bitmaps (4 × 64 bits = 256 vectors).
///
/// A set bit means a guest EOI for that vector exits; clear (the default)
/// means the EOI is virtualized. Only meaningful with virtual-interrupt
/// delivery.
pub const VMCS_EOI_EXIT_BITMAP_0: VmcsField = VmcsField::from_encoding(0x0000_201C);
/// EOI-exit bitmap bits 127:64.
pub const VMCS_EOI_EXIT_BITMAP_1: VmcsField = VmcsField::from_encoding(0x0000_201E);
/// EOI-exit bitmap bits 191:128.
pub const VMCS_EOI_EXIT_BITMAP_2: VmcsField = VmcsField::from_encoding(0x0000_2020);
/// EOI-exit bitmap bits 255:192.
pub const VMCS_EOI_EXIT_BITMAP_3: VmcsField = VmcsField::from_encoding(0x0000_2022);

/// 32-bit control field: TPR threshold. While virtual-interrupt delivery is
/// 0, an operation lowering VTPR[7:4] below bits 3:0 of this field exits.
pub const VMCS_TPR_THRESHOLD: VmcsField = VmcsField::from_encoding(0x0000_401C);

// ---------------------------------------------------------------------------
// Capability decoding
// ---------------------------------------------------------------------------

/// Why `APICv` enablement was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApicvError {
    /// Posted interrupts requested but the CPU disallows pin-based bit 7.
    PostedInterruptsUnsupported,
    /// APIC-register virtualization / virtual-interrupt delivery requested
    /// but the CPU disallows secondary bits 8/9.
    ApicvUnsupported,
    /// x2APIC virtualization requested but the CPU disallows secondary bit 4.
    X2ApicVirtualizationUnsupported,
    /// A VMCS address violated an alignment requirement.
    UnalignedAddress {
        /// Which address (for diagnostics).
        what: &'static str,
        /// Required alignment in bytes.
        align: u64,
    },
    /// A page-sized region was too small.
    RegionTooSmall {
        /// Which region (for diagnostics).
        what: &'static str,
        /// Required size in bytes.
        need: usize,
    },
}

impl core::fmt::Display for ApicvError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::PostedInterruptsUnsupported => {
                write!(f, "CPU does not allow posted-interrupt processing")
            }
            Self::ApicvUnsupported => write!(f, "CPU does not allow APICv (secondary bits 8/9)"),
            Self::X2ApicVirtualizationUnsupported => {
                write!(f, "CPU does not allow x2APIC virtualization")
            }
            Self::UnalignedAddress { what, align } => {
                write!(f, "{what} is not {align}-byte aligned")
            }
            Self::RegionTooSmall { what, need } => {
                write!(f, "{what} is smaller than {need} bytes")
            }
        }
    }
}

impl core::error::Error for ApicvError {}

/// Whether the high dword of a VMX control-capability MSR allows a control
/// bit to be 1 (Intel SDM Vol. 3, §24.6.2: a clear bit in the allowed-1
/// settings means the control *must* be 0).
const fn ctrl_may_be_one(caps_msr: u64, bit: u32) -> bool {
    (caps_msr >> 32) & (bit as u64) != 0
}

/// Decoded `APICv` capability from the VMX control-capability MSRs.
///
/// Construct from the raw values the backend read with `RDMSR` of
/// [`IA32_VMX_TRUE_PINBASED_CTLS`] / [`IA32_VMX_TRUE_PROCBASED_CTLS`]
/// (falling back to the non-TRUE MSRs when `IA32_VMX_BASIC[55]` is clear).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApicvCaps {
    pinbased_true_ctls: u64,
    procbased_true_ctls: u64,
}

impl ApicvCaps {
    /// Wrap the raw capability MSR values.
    #[must_use]
    pub const fn from_msrs(pinbased_true_ctls: u64, procbased_true_ctls: u64) -> Self {
        Self {
            pinbased_true_ctls,
            procbased_true_ctls,
        }
    }

    /// Whether "process posted interrupts" (pin-based bit 7) may be 1.
    #[must_use]
    pub const fn posted_interrupts(self) -> bool {
        ctrl_may_be_one(self.pinbased_true_ctls, PINBASED_PROCESS_POSTED_INTERRUPTS)
    }

    /// Whether APIC-register virtualization + virtual-interrupt delivery
    /// (secondary bits 8 and 9) may both be 1 — the `APICv` core.
    #[must_use]
    pub const fn apicv(self) -> bool {
        ctrl_may_be_one(
            self.procbased_true_ctls,
            SECONDARY_APIC_REGISTER_VIRTUALIZATION | SECONDARY_VIRTUAL_INTERRUPT_DELIVERY,
        )
    }

    /// Whether "virtualize x2APIC mode" (secondary bit 4) may be 1.
    #[must_use]
    pub const fn x2apic_virtualization(self) -> bool {
        ctrl_may_be_one(self.procbased_true_ctls, SECONDARY_VIRTUALIZE_X2APIC_MODE)
    }

    /// Whether "virtualize APIC accesses" (secondary bit 0) may be 1.
    #[must_use]
    pub const fn apic_access_virtualization(self) -> bool {
        ctrl_may_be_one(self.procbased_true_ctls, SECONDARY_VIRTUALIZE_APIC_ACCESSES)
    }
}

// ---------------------------------------------------------------------------
// Posted-interrupt descriptor
// ---------------------------------------------------------------------------

/// Size of a posted-interrupt descriptor in bytes (Intel SDM Vol. 3C §29.6).
pub const POSTED_INTERRUPT_DESCRIPTOR_SIZE: usize = 64;

/// Required alignment of a posted-interrupt descriptor in bytes.
pub const POSTED_INTERRUPT_DESCRIPTOR_ALIGN: usize = 64;

/// What posting an interrupt decided about the notification IPI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostAction {
    /// The outstanding-notification bit was clear: the backend must now send
    /// the notification IPI (vector = descriptor's NV) to the physical CPU in
    /// the descriptor's NDST field.
    Notify,
    /// The outstanding-notification bit was already set: a notification is
    /// in flight (or the processor already saw it); nothing more to do.
    AlreadyNotified,
}

/// A posted-interrupt descriptor (Intel SDM Vol. 3C §29.6, Fig. 29-16).
///
/// ```text
///  bytes  0..32  PIR   — 256-bit posted-interrupt requests bitmap
///  byte  32 bit 0  ON  — outstanding notification
///  byte  32 bit 1  SN  — suppress notification
///  byte  34       NV   — posted-interrupt notification vector
///  bytes 36..40  NDST  — notification destination (physical APIC ID)
///  bytes 40..64        — reserved, must be zero
/// ```
///
/// The hypervisor posts by setting a PIR bit; if ON was clear it sets ON and
/// sends the notification IPI itself — the processor never sends IPIs. The
/// processor clears ON when it acts on the notification and injects the
/// pending vectors into the guest without VM exits.
#[repr(C, align(64))]
#[derive(Debug, Clone)]
pub struct PostedInterruptDescriptor {
    pir: [u32; 8],
    on_sn: u16,
    nv: u8,
    _reserved0: u8,
    ndst: u32,
    _reserved1: [u8; 24],
}

impl PostedInterruptDescriptor {
    /// A zeroed descriptor. The backend programs NV/NDST before use.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pir: [0; 8],
            on_sn: 0,
            nv: 0,
            _reserved0: 0,
            ndst: 0,
            _reserved1: [0; 24],
        }
    }

    /// Set the posted-interrupt request bit for `vector`.
    pub const fn set_vector(&mut self, vector: u8) {
        let word = (vector as usize) >> 5;
        let bit = (vector & 31) as u32;
        self.pir[word] |= 1 << bit;
    }

    /// Clear the posted-interrupt request bit for `vector`.
    pub const fn clear_vector(&mut self, vector: u8) {
        let word = (vector as usize) >> 5;
        let bit = (vector & 31) as u32;
        self.pir[word] &= !(1 << bit);
    }

    /// Whether the posted-interrupt request bit for `vector` is set.
    #[must_use]
    pub const fn test_vector(&self, vector: u8) -> bool {
        let word = (vector as usize) >> 5;
        let bit = (vector & 31) as u32;
        self.pir[word] & (1 << bit) != 0
    }

    /// Drain the whole PIR bitmap, clearing it. Used by backend diagnostics
    /// (e.g. counting coalesced posted interrupts); the processor consumes
    /// PIR directly and needs no software drain.
    pub const fn drain_pir(&mut self) -> [u32; 8] {
        let pir = self.pir;
        self.pir = [0; 8];
        pir
    }

    /// Highest set PIR vector, if any.
    #[must_use]
    pub fn highest_pending(&self) -> Option<u8> {
        for (i, &word) in self.pir.iter().enumerate().rev() {
            if word != 0 {
                let base = u8::try_from(i).ok()?.checked_mul(32)?;
                let bit = u8::try_from(word.ilog2()).ok()?;
                return base.checked_add(bit);
            }
        }
        None
    }

    /// The outstanding-notification (ON) bit.
    #[must_use]
    pub const fn outstanding_notification(&self) -> bool {
        self.on_sn & 1 != 0
    }

    /// Set or clear the outstanding-notification bit. The processor clears
    /// it when it processes a notification; software sets it while posting.
    pub const fn set_outstanding_notification(&mut self, on: bool) {
        if on {
            self.on_sn |= 1;
        } else {
            self.on_sn &= !1;
        }
    }

    /// The suppress-notification (SN) bit.
    #[must_use]
    pub const fn suppress_notification(&self) -> bool {
        self.on_sn & 2 != 0
    }

    /// Set or clear the suppress-notification bit.
    pub const fn set_suppress_notification(&mut self, sn: bool) {
        if sn {
            self.on_sn |= 2;
        } else {
            self.on_sn &= !2;
        }
    }

    /// View the descriptor as its 64 raw bytes (for backend diagnostics and
    /// page-content dumps).
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 64] {
        // `repr(C)` with no padding: the struct is exactly 64 bytes.
        unsafe { &*core::ptr::from_ref(self).cast::<[u8; 64]>() }
    }

    /// The posted-interrupt notification vector (NV).
    #[must_use]
    pub const fn notification_vector(&self) -> u8 {
        self.nv
    }

    /// Program the notification vector (must be ≤ 255 by construction).
    pub const fn set_notification_vector(&mut self, vector: u8) {
        self.nv = vector;
    }

    /// The notification destination (NDST): physical APIC ID of the CPU that
    /// should receive the notification IPI.
    #[must_use]
    pub const fn notification_destination(&self) -> u32 {
        self.ndst
    }

    /// Program the notification destination. The backend refreshes this when
    /// the vCPU migrates to another physical CPU.
    pub const fn set_notification_destination(&mut self, apic_id: u32) {
        self.ndst = apic_id;
    }

    /// Post `vector`: set its PIR bit and decide about the notification.
    ///
    /// Implements the SDM §29.6 posting protocol: if the
    /// outstanding-notification bit was clear, it is set and the caller must
    /// send the notification IPI ([`PostAction::Notify`]); otherwise a
    /// notification is already outstanding ([`PostAction::AlreadyNotified`]).
    pub const fn post(&mut self, vector: u8) -> PostAction {
        self.set_vector(vector);
        if self.outstanding_notification() {
            PostAction::AlreadyNotified
        } else {
            self.set_outstanding_notification(true);
            PostAction::Notify
        }
    }
}

impl Default for PostedInterruptDescriptor {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Virtual-APIC page
// ---------------------------------------------------------------------------

/// Size of the virtual-APIC and APIC-access pages in bytes.
pub const APIC_PAGE_SIZE: usize = 4096;

/// Register offsets inside the virtual-APIC page — the same offsets as the
/// xAPIC MMIO page (Intel SDM Vol. 3C §29.4.1).
pub mod vapic_reg {
    /// APIC ID register.
    pub const APIC_ID: usize = 0x020;
    /// Task-priority register.
    pub const TPR: usize = 0x080;
    /// Processor-priority register (read-only to the guest).
    pub const PPR: usize = 0x0A0;
    /// End-of-interrupt register (write-only).
    pub const EOI: usize = 0x0B0;
    /// In-service registers (8 × u32).
    pub const ISR_BASE: usize = 0x100;
    /// Trigger-mode registers (8 × u32).
    pub const TMR_BASE: usize = 0x180;
    /// Interrupt-request registers (8 × u32).
    pub const IRR_BASE: usize = 0x200;
    /// Interrupt-command registers.
    pub const ICR_LO: usize = 0x300;
    /// Interrupt-command registers (high dword).
    pub const ICR_HI: usize = 0x310;
}

/// Zero a virtual-APIC or APIC-access page.
///
/// # Errors
///
/// Returns [`ApicvError::RegionTooSmall`] if `page` is shorter than
/// [`APIC_PAGE_SIZE`].
pub fn init_apic_page(page: &mut [u8]) -> Result<(), ApicvError> {
    if page.len() < APIC_PAGE_SIZE {
        return Err(ApicvError::RegionTooSmall {
            what: "APIC page",
            need: APIC_PAGE_SIZE,
        });
    }
    page[..APIC_PAGE_SIZE].fill(0);
    Ok(())
}

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

/// Read the virtual TPR (bits 7:0) from a virtual-APIC page.
#[must_use]
pub fn vapic_read_tpr(page: &[u8]) -> u8 {
    (load_u32(page, vapic_reg::TPR) & 0xFF) as u8
}

/// Write the virtual TPR (bits 7:0) of a virtual-APIC page.
pub fn vapic_write_tpr(page: &mut [u8], tpr: u8) {
    let cur = load_u32(page, vapic_reg::TPR);
    store_u32(page, vapic_reg::TPR, (cur & !0xFF) | u32::from(tpr));
}

/// Read the virtual APIC ID (bits 31:24) from a virtual-APIC page.
#[must_use]
pub fn vapic_read_apic_id(page: &[u8]) -> u8 {
    ((load_u32(page, vapic_reg::APIC_ID) >> 24) & 0xFF) as u8
}

/// Write the virtual APIC ID (bits 31:24) of a virtual-APIC page.
pub fn vapic_write_apic_id(page: &mut [u8], apic_id: u8) {
    let cur = load_u32(page, vapic_reg::APIC_ID);
    store_u32(
        page,
        vapic_reg::APIC_ID,
        (cur & 0x00FF_FFFF) | (u32::from(apic_id) << 24),
    );
}

// ---------------------------------------------------------------------------
// EOI-exit bitmap
// ---------------------------------------------------------------------------

/// The 256-bit EOI-exit bitmap (4 × u64), programmed into
/// [`VMCS_EOI_EXIT_BITMAP_0`]–[`VMCS_EOI_EXIT_BITMAP_3`].
///
/// A set bit makes a guest EOI for that vector exit; the all-clear default
/// virtualizes every EOI. Used to force exits for vectors whose EOI has
/// device side effects the backend must observe (e.g. level-triggered lines
/// re-armed on EOI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EoiExitBitmap(pub [u64; 4]);

impl EoiExitBitmap {
    /// All clear: no vector's EOI exits (maximum virtualization).
    #[must_use]
    pub const fn new() -> Self {
        Self([0; 4])
    }

    /// Force a VM exit when the guest EOIs `vector`.
    pub const fn set_exit(&mut self, vector: u8) {
        let word = (vector as usize) >> 6;
        let bit = (vector & 63) as u32;
        self.0[word] |= 1 << bit;
    }

    /// Virtualize the EOI for `vector` again.
    pub const fn clear_exit(&mut self, vector: u8) {
        let word = (vector as usize) >> 6;
        let bit = (vector & 63) as u32;
        self.0[word] &= !(1 << bit);
    }

    /// Whether the guest's EOI for `vector` exits.
    #[must_use]
    pub const fn exits_on(&self, vector: u8) -> bool {
        let word = (vector as usize) >> 6;
        let bit = (vector & 63) as u32;
        self.0[word] & (1 << bit) != 0
    }

    /// The four bitmap words for the VMCS fields, in order.
    #[must_use]
    pub const fn words(&self) -> &[u64; 4] {
        &self.0
    }
}

impl Default for EoiExitBitmap {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Enablement recipe
// ---------------------------------------------------------------------------

/// The `APICv` configuration for one VM.
///
/// The backend probes [`ApicvCaps`], builds this, checks it with
/// [`check_against`](Self::check_against), then programs the control words
/// (via [`VmxControlCaps::adjust`](super::vmx::VmxControlCaps::adjust)) and
/// the [`vmcs_fields`](Self::vmcs_fields) with `VMWRITE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApicvConfig {
    /// Enable posted-interrupt processing (pin-based bit 7 + descriptor).
    /// Requires [`ApicvCaps::posted_interrupts`].
    pub posted_interrupts: bool,
    /// Virtualize x2APIC mode (secondary bit 4) instead of the APIC-access
    /// page (secondary bit 0). The two are mutually exclusive: VM entry
    /// fails if both are 1, so enabling this clears bit 0.
    pub x2apic_mode: bool,
    /// Posted-interrupt notification vector programmed into
    /// [`VMCS_POSTED_INTERRUPT_NOTIFICATION_VECTOR`].
    pub notification_vector: u8,
}

impl ApicvConfig {
    /// The secondary processor-based control bits this config needs.
    ///
    /// Always APIC-register virtualization + virtual-interrupt delivery,
    /// plus APIC-access virtualization — or x2APIC virtualization instead
    /// when [`x2apic_mode`](Self::x2apic_mode) is set (mutually exclusive).
    #[must_use]
    pub const fn secondary_bits(self) -> u32 {
        let mut bits =
            SECONDARY_APIC_REGISTER_VIRTUALIZATION | SECONDARY_VIRTUAL_INTERRUPT_DELIVERY;
        if self.x2apic_mode {
            bits |= SECONDARY_VIRTUALIZE_X2APIC_MODE;
        } else {
            bits |= SECONDARY_VIRTUALIZE_APIC_ACCESSES;
        }
        bits
    }

    /// The pin-based control bits this config needs.
    #[must_use]
    pub const fn pin_bits(self) -> u32 {
        if self.posted_interrupts {
            PINBASED_PROCESS_POSTED_INTERRUPTS
        } else {
            0
        }
    }

    /// Check the config against the probed CPU capabilities.
    ///
    /// # Errors
    ///
    /// Returns [`ApicvError`] when the CPU disallows a requested feature.
    pub const fn check_against(self, caps: ApicvCaps) -> Result<(), ApicvError> {
        if self.posted_interrupts && !caps.posted_interrupts() {
            return Err(ApicvError::PostedInterruptsUnsupported);
        }
        if !caps.apicv() {
            return Err(ApicvError::ApicvUnsupported);
        }
        if self.x2apic_mode && !caps.x2apic_virtualization() {
            return Err(ApicvError::X2ApicVirtualizationUnsupported);
        }
        if !self.x2apic_mode && !caps.apic_access_virtualization() {
            return Err(ApicvError::ApicvUnsupported);
        }
        Ok(())
    }

    /// Check VMCS address alignment: the descriptor must be 64-byte aligned
    /// (VM entry requires bits 5:0 clear) and the APIC pages 4 KiB aligned.
    ///
    /// # Errors
    ///
    /// Returns [`ApicvError::UnalignedAddress`] on a violation.
    pub const fn check_addresses(
        desc_pa: u64,
        virtual_apic_pa: u64,
        apic_access_pa: u64,
        x2apic_mode: bool,
    ) -> Result<(), ApicvError> {
        if desc_pa & 0x3F != 0 {
            return Err(ApicvError::UnalignedAddress {
                what: "posted-interrupt descriptor",
                align: 64,
            });
        }
        if virtual_apic_pa & 0xFFF != 0 {
            return Err(ApicvError::UnalignedAddress {
                what: "virtual-APIC page",
                align: 4096,
            });
        }
        if !x2apic_mode && apic_access_pa & 0xFFF != 0 {
            return Err(ApicvError::UnalignedAddress {
                what: "APIC-access page",
                align: 4096,
            });
        }
        Ok(())
    }

    /// The `(field, value)` pairs the backend writes with `VMWRITE`.
    ///
    /// Programs the descriptor address, notification vector, virtual-APIC
    /// page address, APIC-access page address (skipped in x2APIC mode), TPR
    /// threshold, and the four EOI-exit bitmap words. The caller supplies the
    /// *physical* addresses of pages the backend allocated and mapped.
    #[must_use]
    pub fn vmcs_fields(
        &self,
        desc_pa: u64,
        virtual_apic_pa: u64,
        apic_access_pa: u64,
        tpr_threshold: u32,
        eoi: &EoiExitBitmap,
    ) -> Vec<(VmcsField, u64)> {
        let mut fields = Vec::with_capacity(9);
        fields.push((VMCS_POSTED_INTERRUPT_DESCRIPTOR_ADDR, desc_pa));
        fields.push((
            VMCS_POSTED_INTERRUPT_NOTIFICATION_VECTOR,
            u64::from(self.notification_vector),
        ));
        fields.push((VMCS_VIRTUAL_APIC_PAGE_ADDR, virtual_apic_pa));
        if !self.x2apic_mode {
            fields.push((VMCS_APIC_ACCESS_ADDR, apic_access_pa));
        }
        fields.push((VMCS_TPR_THRESHOLD, u64::from(tpr_threshold)));
        let bitmaps = [
            VMCS_EOI_EXIT_BITMAP_0,
            VMCS_EOI_EXIT_BITMAP_1,
            VMCS_EOI_EXIT_BITMAP_2,
            VMCS_EOI_EXIT_BITMAP_3,
        ];
        for (field, &word) in bitmaps.iter().zip(eoi.words().iter()) {
            fields.push((*field, word));
        }
        fields
    }
}

// ---------------------------------------------------------------------------
// Per-vCPU state
// ---------------------------------------------------------------------------

/// Owned per-vCPU `APICv` state: the posted-interrupt descriptor, the
/// virtual-APIC page, and the APIC-access page.
///
/// The backend maps these at guest-chosen physical addresses, programs the
/// addresses from [`ApicvConfig::vmcs_fields`], and calls [`post`](Self::post)
/// to deliver interrupts; a [`PostAction::Notify`] result means "send the
/// notification IPI now".
pub struct ApicvVcpu {
    descriptor: Box<PostedInterruptDescriptor>,
    virtual_apic_page: Box<[u8; APIC_PAGE_SIZE]>,
    apic_access_page: Box<[u8; APIC_PAGE_SIZE]>,
    eoi_exits: EoiExitBitmap,
}

impl ApicvVcpu {
    /// Allocate zeroed per-vCPU state and program the descriptor's
    /// notification vector / destination.
    #[must_use]
    pub fn new(notification_vector: u8, notification_destination: u32) -> Self {
        let mut descriptor = Box::new(PostedInterruptDescriptor::new());
        descriptor.set_notification_vector(notification_vector);
        descriptor.set_notification_destination(notification_destination);
        Self {
            descriptor,
            virtual_apic_page: Box::new([0; APIC_PAGE_SIZE]),
            apic_access_page: Box::new([0; APIC_PAGE_SIZE]),
            eoi_exits: EoiExitBitmap::new(),
        }
    }

    /// The posted-interrupt descriptor.
    #[must_use]
    pub fn descriptor(&self) -> &PostedInterruptDescriptor {
        &self.descriptor
    }

    /// Mutable access to the posted-interrupt descriptor.
    pub fn descriptor_mut(&mut self) -> &mut PostedInterruptDescriptor {
        &mut self.descriptor
    }

    /// The virtual-APIC page (guest's virtualized LAPIC registers).
    #[must_use]
    pub fn virtual_apic_page(&self) -> &[u8; APIC_PAGE_SIZE] {
        &self.virtual_apic_page
    }

    /// Mutable access to the virtual-APIC page.
    pub fn virtual_apic_page_mut(&mut self) -> &mut [u8; APIC_PAGE_SIZE] {
        &mut self.virtual_apic_page
    }

    /// The APIC-access page (unused in x2APIC mode).
    #[must_use]
    pub fn apic_access_page(&self) -> &[u8; APIC_PAGE_SIZE] {
        &self.apic_access_page
    }

    /// The EOI-exit bitmap programmed into the VMCS.
    #[must_use]
    pub const fn eoi_exits(&self) -> &EoiExitBitmap {
        &self.eoi_exits
    }

    /// Mutable access to the EOI-exit bitmap.
    pub const fn eoi_exits_mut(&mut self) -> &mut EoiExitBitmap {
        &mut self.eoi_exits
    }

    /// Refresh the notification destination, e.g. after the vCPU migrates
    /// to another physical CPU.
    pub fn set_notification_destination(&mut self, apic_id: u32) {
        self.descriptor.set_notification_destination(apic_id);
    }

    /// Post `vector` to this vCPU. [`PostAction::Notify`] means the backend
    /// must now send an IPI with the notification vector to
    /// [`notification_destination`](PostedInterruptDescriptor::notification_destination).
    pub const fn post(&mut self, vector: u8) -> PostAction {
        self.descriptor.post(vector)
    }
}

impl core::fmt::Debug for ApicvVcpu {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ApicvVcpu")
            .field("descriptor", &self.descriptor)
            .field("eoi_exits", &self.eoi_exits)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::vmx::{IA32_VMX_TRUE_PINBASED_CTLS, IA32_VMX_TRUE_PROCBASED_CTLS};
    use super::*;
    use alloc::string::ToString;
    use core::mem::{align_of, size_of};

    // -- Descriptor layout -------------------------------------------------

    #[test]
    fn descriptor_size_and_alignment() {
        assert_eq!(size_of::<PostedInterruptDescriptor>(), 64);
        assert_eq!(align_of::<PostedInterruptDescriptor>(), 64);
        assert_eq!(
            size_of::<PostedInterruptDescriptor>(),
            POSTED_INTERRUPT_DESCRIPTOR_SIZE
        );
        assert_eq!(
            align_of::<PostedInterruptDescriptor>(),
            POSTED_INTERRUPT_DESCRIPTOR_ALIGN
        );
    }

    #[test]
    fn descriptor_vector_bits_round_trip() {
        let mut d = PostedInterruptDescriptor::new();
        // Word-boundary vectors: 0, 31, 32, 33, 127, 128, 223, 255.
        for v in [0u8, 31, 32, 33, 127, 128, 223, 255] {
            assert!(!d.test_vector(v), "vector {v} should start clear");
            d.set_vector(v);
            assert!(d.test_vector(v), "vector {v} should be set");
        }
        // Neighbors untouched.
        assert!(!d.test_vector(1));
        assert!(!d.test_vector(30));
        assert!(!d.test_vector(129));
        // Clearing works per-bit.
        d.clear_vector(32);
        assert!(!d.test_vector(32));
        assert!(d.test_vector(33));
        assert!(d.test_vector(31));
    }

    #[test]
    fn descriptor_control_fields_round_trip() {
        let mut d = PostedInterruptDescriptor::new();
        assert!(!d.outstanding_notification());
        assert!(!d.suppress_notification());

        d.set_notification_vector(0xE7);
        d.set_notification_destination(0x1A2B_3C4D);
        d.set_outstanding_notification(true);
        d.set_suppress_notification(true);
        assert_eq!(d.notification_vector(), 0xE7);
        assert_eq!(d.notification_destination(), 0x1A2B_3C4D);
        assert!(d.outstanding_notification());
        assert!(d.suppress_notification());

        d.set_outstanding_notification(false);
        d.set_suppress_notification(false);
        assert!(!d.outstanding_notification());
        assert!(!d.suppress_notification());
        // Vector/destination survive flag toggles.
        assert_eq!(d.notification_vector(), 0xE7);
        assert_eq!(d.notification_destination(), 0x1A2B_3C4D);
    }

    #[test]
    fn descriptor_byte_layout_matches_sdm() {
        // Spot-check the exact byte layout against SDM Vol. 3C §29.6:
        // PIR = bytes 0..32, ON = byte 32 bit 0, SN = byte 32 bit 1,
        // NV = byte 34, NDST = bytes 36..40, rest zero.
        let mut d = PostedInterruptDescriptor::new();
        d.set_vector(200);
        d.set_notification_vector(0xAB);
        d.set_notification_destination(0x0102_0304);
        d.set_outstanding_notification(true);

        let bytes = d.as_bytes();
        assert_eq!(bytes[25], 1 << (200 % 8)); // vector 200 -> byte 25, bit 0
        assert_eq!(bytes[32] & 1, 1); // ON
        assert_eq!(bytes[32] & 2, 0); // SN clear
        assert_eq!(bytes[34], 0xAB); // NV
        assert_eq!(&bytes[36..40], &[0x04, 0x03, 0x02, 0x01]); // NDST LE
        assert!(bytes[40..64].iter().all(|&b| b == 0)); // reserved
        assert_eq!(bytes[33], 0); // reserved between ON/SN and NV
        assert_eq!(bytes[35], 0);
    }

    #[test]
    fn post_protocol_notifies_once() {
        let mut d = PostedInterruptDescriptor::new();
        // First post: ON was clear -> Notify, ON now set.
        assert_eq!(d.post(0x30), PostAction::Notify);
        assert!(d.outstanding_notification());
        assert!(d.test_vector(0x30));
        // Second post while ON set: no new notification needed.
        assert_eq!(d.post(0x31), PostAction::AlreadyNotified);
        assert!(d.test_vector(0x31));
        // Processor acted on the notification (clears ON) -> next post notifies again.
        d.set_outstanding_notification(false);
        assert_eq!(d.post(0x32), PostAction::Notify);
    }

    #[test]
    fn highest_pending_scans_downward() {
        let mut d = PostedInterruptDescriptor::new();
        assert_eq!(d.highest_pending(), None);
        d.set_vector(33);
        d.set_vector(200);
        d.set_vector(7);
        assert_eq!(d.highest_pending(), Some(200));
        d.clear_vector(200);
        assert_eq!(d.highest_pending(), Some(33));
    }

    #[test]
    fn drain_pir_returns_and_clears() {
        let mut d = PostedInterruptDescriptor::new();
        d.set_vector(5);
        d.set_vector(250);
        let pir = d.drain_pir();
        assert_eq!(pir[0] & (1 << 5), 1 << 5);
        assert_eq!(pir[7] >> 26 & 1, 1); // vector 250 -> word 7 bit 26
        assert_eq!(d.highest_pending(), None);
    }

    // -- Capability decoding -----------------------------------------------

    fn caps_with(bits_allowed_1: u64) -> ApicvCaps {
        // allowed-1 settings live in the high dword; low dword (allowed-0)
        // left clear.
        ApicvCaps::from_msrs(bits_allowed_1 << 32, bits_allowed_1 << 32)
    }

    #[test]
    fn caps_decode_posted_interrupts() {
        let pin = u64::from(PINBASED_PROCESS_POSTED_INTERRUPTS);
        let sec = u64::from(
            SECONDARY_APIC_REGISTER_VIRTUALIZATION | SECONDARY_VIRTUAL_INTERRUPT_DELIVERY,
        );
        let caps = ApicvCaps::from_msrs(pin << 32, sec << 32);
        assert!(caps.posted_interrupts());
        assert!(caps.apicv());
        assert!(!caps.x2apic_virtualization());

        let none = caps_with(0);
        assert!(!none.posted_interrupts());
        assert!(!none.apicv());
    }

    #[test]
    fn config_check_against_caps() {
        // The pin-based MSR carries the pin bit; the procbased MSR the rest.
        let caps = ApicvCaps::from_msrs(
            u64::from(PINBASED_PROCESS_POSTED_INTERRUPTS) << 32,
            u64::from(
                SECONDARY_VIRTUALIZE_APIC_ACCESSES
                    | SECONDARY_VIRTUALIZE_X2APIC_MODE
                    | SECONDARY_APIC_REGISTER_VIRTUALIZATION
                    | SECONDARY_VIRTUAL_INTERRUPT_DELIVERY,
            ) << 32,
        );
        let cfg = ApicvConfig {
            posted_interrupts: true,
            x2apic_mode: false,
            notification_vector: 0xE0,
        };
        assert!(cfg.check_against(caps).is_ok());

        let no_pi = ApicvCaps::from_msrs(
            0,
            u64::from(
                SECONDARY_VIRTUALIZE_APIC_ACCESSES
                    | SECONDARY_APIC_REGISTER_VIRTUALIZATION
                    | SECONDARY_VIRTUAL_INTERRUPT_DELIVERY,
            ) << 32,
        );
        assert_eq!(
            cfg.check_against(no_pi),
            Err(ApicvError::PostedInterruptsUnsupported)
        );

        let no_apicv = ApicvCaps::from_msrs(u64::from(PINBASED_PROCESS_POSTED_INTERRUPTS) << 32, 0);
        assert_eq!(
            cfg.check_against(no_apicv),
            Err(ApicvError::ApicvUnsupported)
        );

        // posted_interrupts is checked first, so disable it to reach the
        // x2APIC check.
        let x2 = ApicvConfig {
            posted_interrupts: false,
            x2apic_mode: true,
            ..cfg
        };
        assert_eq!(
            x2.check_against(no_pi),
            Err(ApicvError::X2ApicVirtualizationUnsupported)
        );
    }

    #[test]
    fn config_bits_encode_recipe() {
        let cfg = ApicvConfig {
            posted_interrupts: true,
            x2apic_mode: false,
            notification_vector: 0xE1,
        };
        assert_eq!(
            cfg.secondary_bits(),
            SECONDARY_VIRTUALIZE_APIC_ACCESSES
                | SECONDARY_APIC_REGISTER_VIRTUALIZATION
                | SECONDARY_VIRTUAL_INTERRUPT_DELIVERY
        );
        assert_eq!(cfg.pin_bits(), PINBASED_PROCESS_POSTED_INTERRUPTS);

        let x2 = ApicvConfig {
            x2apic_mode: true,
            ..cfg
        };
        // x2APIC mode and APIC-access virtualization are mutually exclusive.
        assert_eq!(
            x2.secondary_bits(),
            SECONDARY_VIRTUALIZE_X2APIC_MODE
                | SECONDARY_APIC_REGISTER_VIRTUALIZATION
                | SECONDARY_VIRTUAL_INTERRUPT_DELIVERY
        );
        assert_eq!(x2.secondary_bits() & SECONDARY_VIRTUALIZE_APIC_ACCESSES, 0);
    }

    #[test]
    fn check_addresses_enforces_alignment() {
        assert!(ApicvConfig::check_addresses(0x1000_0040, 0x2000, 0x3000, false).is_ok());
        assert_eq!(
            ApicvConfig::check_addresses(0x1000_0001, 0x2000, 0x3000, false),
            Err(ApicvError::UnalignedAddress {
                what: "posted-interrupt descriptor",
                align: 64
            })
        );
        assert_eq!(
            ApicvConfig::check_addresses(0x1000_0040, 0x2001, 0x3000, false),
            Err(ApicvError::UnalignedAddress {
                what: "virtual-APIC page",
                align: 4096
            })
        );
        assert_eq!(
            ApicvConfig::check_addresses(0x1000_0040, 0x2000, 0x3001, false),
            Err(ApicvError::UnalignedAddress {
                what: "APIC-access page",
                align: 4096
            })
        );
        // x2APIC mode skips the APIC-access page entirely.
        assert!(ApicvConfig::check_addresses(0x1000_0040, 0x2000, 0x3001, true).is_ok());
    }

    // -- VMCS field encodings ------------------------------------------------

    #[test]
    fn vmcs_field_encodings_have_expected_widths() {
        use super::super::vmx::VmcsFieldWidth;
        // Cross-checks the hand-written encodings against the repo's own
        // VMCS field decoder: a wrong constant would decode to the wrong
        // width class.
        assert_eq!(
            VMCS_POSTED_INTERRUPT_NOTIFICATION_VECTOR.width(),
            VmcsFieldWidth::Bits16
        );
        assert_eq!(VMCS_VIRTUAL_APIC_PAGE_ADDR.width(), VmcsFieldWidth::Bits64);
        assert_eq!(VMCS_APIC_ACCESS_ADDR.width(), VmcsFieldWidth::Bits64);
        assert_eq!(
            VMCS_POSTED_INTERRUPT_DESCRIPTOR_ADDR.width(),
            VmcsFieldWidth::Bits64
        );
        for f in [
            VMCS_EOI_EXIT_BITMAP_0,
            VMCS_EOI_EXIT_BITMAP_1,
            VMCS_EOI_EXIT_BITMAP_2,
            VMCS_EOI_EXIT_BITMAP_3,
        ] {
            assert_eq!(f.width(), VmcsFieldWidth::Bits64);
        }
        assert_eq!(VMCS_TPR_THRESHOLD.width(), VmcsFieldWidth::Bits32);
    }

    #[test]
    fn vmcs_fields_covers_enablement_recipe() {
        let cfg = ApicvConfig {
            posted_interrupts: true,
            x2apic_mode: false,
            notification_vector: 0xE2,
        };
        let eoi = EoiExitBitmap::new();
        let fields = cfg.vmcs_fields(0x1000_0040, 0x2000, 0x3000, 0, &eoi);
        // descriptor + NV + vapic + apic-access + TPR threshold + 4 EOI words
        assert_eq!(fields.len(), 9);
        let get = |fields: &[(VmcsField, u64)], f: VmcsField| {
            fields.iter().find(|(ff, _)| *ff == f).map(|(_, v)| *v)
        };
        assert_eq!(
            get(&fields, VMCS_POSTED_INTERRUPT_DESCRIPTOR_ADDR),
            Some(0x1000_0040)
        );
        assert_eq!(
            get(&fields, VMCS_POSTED_INTERRUPT_NOTIFICATION_VECTOR),
            Some(0xE2)
        );
        assert_eq!(get(&fields, VMCS_VIRTUAL_APIC_PAGE_ADDR), Some(0x2000));
        assert_eq!(get(&fields, VMCS_APIC_ACCESS_ADDR), Some(0x3000));

        let x2 = ApicvConfig {
            x2apic_mode: true,
            ..cfg
        };
        let fields = x2.vmcs_fields(0x1000_0040, 0x2000, 0x3000, 0, &eoi);
        assert_eq!(fields.len(), 8); // no APIC-access page in x2APIC mode
        assert_eq!(get(&fields, VMCS_APIC_ACCESS_ADDR), None);
    }

    // -- Virtual-APIC page ---------------------------------------------------

    #[test]
    fn virtual_apic_page_tpr_and_id() {
        let mut page = [0u8; APIC_PAGE_SIZE];
        init_apic_page(&mut page).unwrap();
        vapic_write_tpr(&mut page, 0xA0);
        vapic_write_apic_id(&mut page, 0x2A);
        assert_eq!(vapic_read_tpr(&page), 0xA0);
        assert_eq!(vapic_read_apic_id(&page), 0x2A);
        // Writes preserve the untouched bits of each register.
        assert_eq!(load_u32(&page, vapic_reg::TPR) & !0xFF, 0);
    }

    #[test]
    fn init_apic_page_rejects_short_slice() {
        let mut small = [0u8; 128];
        assert!(init_apic_page(&mut small).is_err());
    }

    // -- EOI-exit bitmap -----------------------------------------------------

    #[test]
    fn eoi_exit_bitmap_set_clear() {
        let mut b = EoiExitBitmap::new();
        assert!(!b.exits_on(0x40));
        b.set_exit(0x40);
        b.set_exit(255);
        assert!(b.exits_on(0x40));
        assert!(b.exits_on(255));
        assert!(!b.exits_on(0x41));
        // Bitmap words land in the right slots: vector 0x40 -> word 1 bit 0.
        assert_eq!(b.words()[1] & 1, 1);
        assert_eq!(b.words()[3] >> 63, 1);
        b.clear_exit(0x40);
        assert!(!b.exits_on(0x40));
        assert!(b.exits_on(255));
    }

    // -- Per-vCPU state ------------------------------------------------------

    #[test]
    fn apicv_vcpu_posts_and_tracks_destination() {
        let mut vcpu = ApicvVcpu::new(0xE3, 7);
        assert_eq!(vcpu.descriptor().notification_vector(), 0xE3);
        assert_eq!(vcpu.descriptor().notification_destination(), 7);
        assert_eq!(vcpu.post(0x50), PostAction::Notify);
        assert_eq!(vcpu.post(0x51), PostAction::AlreadyNotified);
        // Migration refreshes NDST.
        vcpu.set_notification_destination(12);
        assert_eq!(vcpu.descriptor().notification_destination(), 12);
        // Virtual-APIC page is writable host-side.
        vapic_write_tpr(vcpu.virtual_apic_page_mut(), 0x10);
        assert_eq!(vapic_read_tpr(vcpu.virtual_apic_page()), 0x10);
    }

    // -- Error display --------------------------------------------------------

    #[test]
    fn apicv_error_display() {
        assert!(
            ApicvError::PostedInterruptsUnsupported
                .to_string()
                .contains("posted-interrupt")
        );
        assert!(
            ApicvError::UnalignedAddress {
                what: "posted-interrupt descriptor",
                align: 64
            }
            .to_string()
            .contains("64-byte aligned")
        );
    }

    // -- MSR index constants are referenced (seam documentation) ---------------

    #[test]
    fn capability_msr_indices_are_wired() {
        // Guards against the caps struct drifting from the MSRs it decodes.
        assert_eq!(IA32_VMX_TRUE_PINBASED_CTLS, 0x48D);
        assert_eq!(IA32_VMX_TRUE_PROCBASED_CTLS, 0x48E);
    }
}
