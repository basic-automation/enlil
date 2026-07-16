//! AMD SVM decode layer (AMD64 APM Vol. 2, chapters 15 and Appendix B).
//!
//! The AMD half of the Phase 6.2 bare-metal backend, mirroring [`vmx`](crate::vmx)
//! for Intel: everything here is pure decode/layout logic — CPUID/MSR
//! capability decoding, the VMCB layout, #VMEXIT code decoding, and the
//! routing of simple exits onto the arch-neutral [`VmExit`](crate::VmExit) —
//! host-tested against the APM, with no privileged instruction anywhere.
//! The `VMRUN`/`VMSAVE`/`VMLOAD` assembly ops build on top of these types.
//!
//! This host's nightly QEMU harness exposes nested **SVM** (`-cpu host,+svm`
//! on an AMD workstation), so this path — not VMX — is the one the second
//! live-boot sub-milestone (bare-metal enlil running a trivial guest to
//! `hlt`) exercises at night.

// ---------------------------------------------------------------------------
// Capability discovery
// ---------------------------------------------------------------------------

/// CPUID leaf reporting SVM capabilities (`CPUID Fn8000_000A`).
pub const CPUID_SVM_FEATURES_LEAF: u32 = 0x8000_000A;

/// CPUID `Fn8000_0001` ECX bit advertising SVM support at all.
pub const CPUID_FN8000_0001_ECX_SVM: u32 = 1 << 2;

/// The `VM_CR` MSR controlling global SVM enablement (APM §15.30.1).
pub const MSR_VM_CR: u32 = 0xC001_0114;

/// `VM_CR.SVMDIS` (bit 4): SVM disabled — `EFER.SVME` writes #GP when set.
pub const VM_CR_SVMDIS: u64 = 1 << 4;

/// `VM_CR.LOCK` (bit 3): the SVMDIS setting is locked until reset.
pub const VM_CR_LOCK: u64 = 1 << 3;

/// The extended-feature-enable register MSR.
pub const MSR_EFER: u32 = 0xC000_0080;

/// `EFER.SVME` (bit 12): the SVM-enable bit `VMRUN` requires.
pub const EFER_SVME: u64 = 1 << 12;

/// The `VM_HSAVE_PA` MSR: physical address of the 4 KiB host state-save area
/// `VMRUN` uses (must be programmed before the first `VMRUN`).
pub const MSR_VM_HSAVE_PA: u32 = 0xC001_0117;

/// Whether the `VM_CR` value says SVM is disabled (and whether firmware
/// locked it that way).
#[must_use]
pub const fn vm_cr_svm_disabled(vm_cr: u64) -> bool {
    vm_cr & VM_CR_SVMDIS != 0
}

/// Whether `VM_CR.SVMDIS` is locked and cannot be cleared without a reset.
#[must_use]
pub const fn vm_cr_svm_locked(vm_cr: u64) -> bool {
    vm_cr & VM_CR_LOCK != 0
}

/// Decoded `CPUID Fn8000_000A` — the SVM feature set (APM §15.4, E.4.16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SvmFeatures {
    /// EAX: SVM revision (bits 7:0).
    pub eax: u32,
    /// EBX: number of address space identifiers (ASIDs).
    pub ebx: u32,
    /// EDX: SVM feature flags.
    pub edx: u32,
}

/// `Fn8000_000A EDX` feature bits (APM E.4.16).
pub mod feature {
    /// Nested paging.
    pub const NESTED_PAGING: u32 = 1 << 0;
    /// LBR virtualization.
    pub const LBR_VIRT: u32 = 1 << 1;
    /// SVM lock.
    pub const SVM_LOCK: u32 = 1 << 2;
    /// Next-RIP save on #VMEXIT.
    pub const NRIP_SAVE: u32 = 1 << 3;
    /// MSR-based TSC rate control.
    pub const TSC_RATE_MSR: u32 = 1 << 4;
    /// VMCB clean bits.
    pub const VMCB_CLEAN: u32 = 1 << 5;
    /// TLB flush selectable by ASID.
    pub const FLUSH_BY_ASID: u32 = 1 << 6;
    /// Decode assists.
    pub const DECODE_ASSISTS: u32 = 1 << 7;
    /// PAUSE intercept filter.
    pub const PAUSE_FILTER: u32 = 1 << 10;
    /// PAUSE filter threshold.
    pub const PAUSE_FILTER_THRESHOLD: u32 = 1 << 12;
    /// Advanced virtual interrupt controller.
    pub const AVIC: u32 = 1 << 13;
    /// Virtualized VMSAVE/VMLOAD.
    pub const V_VMSAVE_VMLOAD: u32 = 1 << 15;
    /// Virtualized GIF.
    pub const VGIF: u32 = 1 << 16;
}

impl SvmFeatures {
    /// Decode the three registers of `CPUID Fn8000_000A`.
    #[must_use]
    pub const fn from_cpuid(eax: u32, ebx: u32, edx: u32) -> Self {
        Self { eax, ebx, edx }
    }

    /// SVM revision number (EAX bits 7:0).
    #[must_use]
    pub const fn revision(self) -> u8 {
        (self.eax & 0xFF) as u8
    }

    /// Number of ASIDs the core supports. ASID 0 is the host; a guest needs
    /// a nonzero ASID, so fewer than 2 leaves no room for any guest.
    #[must_use]
    pub const fn nr_asids(self) -> u32 {
        self.ebx
    }

    /// Whether a feature bit from [`feature`] is present.
    #[must_use]
    pub const fn has(self, bit: u32) -> bool {
        self.edx & bit != 0
    }

    /// Nested paging (the SVM analogue of EPT) — required for enlil guests
    /// (LOCKED PRINCIPLE 5: hardware-enforced memory isolation).
    #[must_use]
    pub const fn has_nested_paging(self) -> bool {
        self.has(feature::NESTED_PAGING)
    }

    /// Next-RIP save: #VMEXIT reports the next instruction pointer, so the
    /// backend can skip emulated instructions without decoding lengths.
    #[must_use]
    pub const fn has_nrip_save(self) -> bool {
        self.has(feature::NRIP_SAVE)
    }
}

// ---------------------------------------------------------------------------
// VMCB layout
// ---------------------------------------------------------------------------

/// Size and required alignment of a VMCB (one 4 KiB page).
pub const VMCB_SIZE: usize = 4096;

/// Byte offsets inside the VMCB control area (APM Appendix B, Table B-1).
///
/// Curated to the fields the minimal-guest bring-up programs and reads; the
/// set grows as the backend does.
pub mod control {
    /// Intercept reads (bits 15:0) / writes (31:16) of CR0–CR15 (u32).
    pub const INTERCEPT_CR: usize = 0x000;
    /// Intercept reads/writes of DR0–DR15 (u32).
    pub const INTERCEPT_DR: usize = 0x004;
    /// Intercept exception vectors 0–31 (u32 bitmap).
    pub const INTERCEPT_EXCEPTIONS: usize = 0x008;
    /// Misc intercept vector 1 (u32) — see [`intercept1`](super::intercept1).
    pub const INTERCEPT_MISC1: usize = 0x00C;
    /// Misc intercept vector 2 (u32) — see [`intercept2`](super::intercept2).
    pub const INTERCEPT_MISC2: usize = 0x010;
    /// Physical base of the 12 KiB I/O permissions map (u64).
    pub const IOPM_BASE_PA: usize = 0x040;
    /// Physical base of the 8 KiB MSR permissions map (u64).
    pub const MSRPM_BASE_PA: usize = 0x048;
    /// Guest TSC offset added to RDTSC/RDTSCP (u64).
    pub const TSC_OFFSET: usize = 0x050;
    /// Guest ASID (u32).
    pub const GUEST_ASID: usize = 0x058;
    /// TLB control (u8): 0 = none, 1 = flush all, 3 = flush this ASID.
    pub const TLB_CONTROL: usize = 0x05C;
    /// Virtual interrupt control (u64): `V_TPR` / `V_IRQ` / `V_INTR` fields.
    pub const INT_CONTROL: usize = 0x060;
    /// Interrupt shadow / guest interruptibility state (u64).
    pub const INT_STATE: usize = 0x068;
    /// #VMEXIT code (u64) — decode with [`SvmExitCode`](super::SvmExitCode).
    pub const EXIT_CODE: usize = 0x070;
    /// First exit-information field (u64).
    pub const EXIT_INFO_1: usize = 0x078;
    /// Second exit-information field (u64).
    pub const EXIT_INFO_2: usize = 0x080;
    /// Pending-event capture at #VMEXIT (u64).
    pub const EXIT_INT_INFO: usize = 0x088;
    /// Nested-paging + SEV control (u64) — bit 0 enables nested paging.
    pub const NESTED_CTL: usize = 0x090;
    /// Event injection (u64).
    pub const EVENT_INJ: usize = 0x0A8;
    /// Nested page-table CR3 (u64).
    pub const NESTED_CR3: usize = 0x0B0;
    /// LBR virtualization enable (u64).
    pub const VIRT_EXT: usize = 0x0B8;
    /// VMCB clean bits (u32).
    pub const VMCB_CLEAN: usize = 0x0C0;
    /// Next sequential RIP after the intercepted instruction (u64), valid
    /// when [`SvmFeatures::has_nrip_save`].
    pub const NEXT_RIP: usize = 0x0C8;

    /// `NESTED_CTL` bit 0: enable nested paging for this guest.
    pub const NESTED_CTL_NP_ENABLE: u64 = 1 << 0;
}

/// Byte offsets inside the VMCB state-save area (APM Appendix B, Table B-2).
///
/// Segment entries are 16 bytes: selector (u16), attributes (u16), limit
/// (u32), base (u64).
pub mod save {
    /// ES segment.
    pub const ES: usize = 0x400;
    /// CS segment.
    pub const CS: usize = 0x410;
    /// SS segment.
    pub const SS: usize = 0x420;
    /// DS segment.
    pub const DS: usize = 0x430;
    /// FS segment.
    pub const FS: usize = 0x440;
    /// GS segment.
    pub const GS: usize = 0x450;
    /// GDTR (base/limit meaningful).
    pub const GDTR: usize = 0x460;
    /// LDTR.
    pub const LDTR: usize = 0x470;
    /// IDTR (base/limit meaningful).
    pub const IDTR: usize = 0x480;
    /// TR.
    pub const TR: usize = 0x490;
    /// Current privilege level (u8).
    pub const CPL: usize = 0x4CB;
    /// EFER (u64).
    pub const EFER: usize = 0x4D0;
    /// CR4 (u64).
    pub const CR4: usize = 0x548;
    /// CR3 (u64).
    pub const CR3: usize = 0x550;
    /// CR0 (u64).
    pub const CR0: usize = 0x558;
    /// DR7 (u64).
    pub const DR7: usize = 0x560;
    /// DR6 (u64).
    pub const DR6: usize = 0x568;
    /// RFLAGS (u64).
    pub const RFLAGS: usize = 0x570;
    /// RIP (u64).
    pub const RIP: usize = 0x578;
    /// RSP (u64).
    pub const RSP: usize = 0x5D8;
    /// RAX (u64) — the only GPR the VMCB carries; the rest are swapped by
    /// the backend around `VMRUN`.
    pub const RAX: usize = 0x5F8;
    /// CR2 (u64).
    pub const CR2: usize = 0x640;
    /// Guest PAT (u64), used when nested paging is enabled.
    pub const G_PAT: usize = 0x668;
}

/// Errors initializing a VMCB region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmcbRegionError {
    /// The supplied buffer is smaller than a VMCB.
    TooSmall {
        /// The buffer length that was provided.
        provided: usize,
        /// The minimum region size required ([`VMCB_SIZE`]).
        required: usize,
    },
}

/// Initialize a VMCB region in place, ready for field programming + `VMRUN`.
///
/// Unlike a VMXON/VMCS region there is no revision header: a fresh VMCB is
/// simply zeroed (APM §15.5 — reserved fields must be zero) and then the
/// backend programs intercepts, ASID, and guest state. The caller supplies
/// the region as a 4 KiB-aligned physical page; only the byte-level layout is
/// handled here so it stays host-testable.
///
/// # Errors
///
/// Returns [`VmcbRegionError::TooSmall`] if `region` is shorter than
/// [`VMCB_SIZE`].
pub fn init_vmcb_region(region: &mut [u8]) -> Result<(), VmcbRegionError> {
    if region.len() < VMCB_SIZE {
        return Err(VmcbRegionError::TooSmall {
            provided: region.len(),
            required: VMCB_SIZE,
        });
    }
    region[..VMCB_SIZE].fill(0);
    Ok(())
}

// ---------------------------------------------------------------------------
// Intercept vectors
// ---------------------------------------------------------------------------

/// Misc intercept vector 1 bits (VMCB offset [`control::INTERCEPT_MISC1`]).
pub mod intercept1 {
    /// Physical interrupt (INTR).
    pub const INTR: u32 = 1 << 0;
    /// Non-maskable interrupt.
    pub const NMI: u32 = 1 << 1;
    /// RDTSC instruction.
    pub const RDTSC: u32 = 1 << 14;
    /// CPUID instruction — always intercepted by enlil (LOCKED PRINCIPLE 1:
    /// the CPUID stealth table answers, never the host CPU directly).
    pub const CPUID: u32 = 1 << 18;
    /// INVD instruction.
    pub const INVD: u32 = 1 << 22;
    /// PAUSE instruction.
    pub const PAUSE: u32 = 1 << 23;
    /// HLT instruction — the minimal guest's exit vehicle.
    pub const HLT: u32 = 1 << 24;
    /// INVLPG instruction.
    pub const INVLPG: u32 = 1 << 25;
    /// I/O port access (subject to the IOPM).
    pub const IOIO_PROT: u32 = 1 << 27;
    /// MSR access (subject to the MSRPM).
    pub const MSR_PROT: u32 = 1 << 28;
    /// Shutdown (triple fault) — intercepted so it never resets the host.
    pub const SHUTDOWN: u32 = 1 << 31;
}

/// Misc intercept vector 2 bits (VMCB offset [`control::INTERCEPT_MISC2`]).
pub mod intercept2 {
    /// VMRUN instruction — must be intercepted for any guest (APM §15.9).
    pub const VMRUN: u32 = 1 << 0;
    /// VMMCALL instruction.
    pub const VMMCALL: u32 = 1 << 1;
    /// VMLOAD instruction.
    pub const VMLOAD: u32 = 1 << 2;
    /// VMSAVE instruction.
    pub const VMSAVE: u32 = 1 << 3;
    /// STGI instruction.
    pub const STGI: u32 = 1 << 4;
    /// CLGI instruction.
    pub const CLGI: u32 = 1 << 5;
    /// SKINIT instruction.
    pub const SKINIT: u32 = 1 << 6;
    /// RDTSCP instruction.
    pub const RDTSCP: u32 = 1 << 7;
    /// WBINVD instruction.
    pub const WBINVD: u32 = 1 << 9;
    /// MONITOR instruction.
    pub const MONITOR: u32 = 1 << 10;
    /// MWAIT instruction (unconditional).
    pub const MWAIT: u32 = 1 << 11;
    /// XSETBV instruction.
    pub const XSETBV: u32 = 1 << 13;
}

// ---------------------------------------------------------------------------
// #VMEXIT codes
// ---------------------------------------------------------------------------

/// Named #VMEXIT codes (APM Appendix C). Only the codes the backend routes
/// first are named; any other exit arrives as its raw number.
pub mod exit_code {
    /// Base of the CR0–CR15 read intercepts (`0x00 + n`).
    pub const READ_CR_BASE: u64 = 0x000;
    /// Base of the CR0–CR15 write intercepts (`0x10 + n`).
    pub const WRITE_CR_BASE: u64 = 0x010;
    /// Base of the exception intercepts (`0x40 + vector`).
    pub const EXCEPTION_BASE: u64 = 0x040;
    /// Physical interrupt.
    pub const INTR: u64 = 0x060;
    /// Non-maskable interrupt.
    pub const NMI: u64 = 0x061;
    /// CPUID instruction.
    pub const CPUID: u64 = 0x072;
    /// PAUSE instruction.
    pub const PAUSE: u64 = 0x077;
    /// HLT instruction.
    pub const HLT: u64 = 0x078;
    /// I/O port access.
    pub const IOIO: u64 = 0x07B;
    /// RDMSR/WRMSR access.
    pub const MSR: u64 = 0x07C;
    /// Shutdown (triple fault).
    pub const SHUTDOWN: u64 = 0x07F;
    /// VMRUN instruction (always intercepted).
    pub const VMRUN: u64 = 0x080;
    /// VMMCALL instruction.
    pub const VMMCALL: u64 = 0x081;
    /// Nested page fault.
    pub const NPF: u64 = 0x400;
    /// Invalid guest state — `VMRUN` failed consistency checks.
    pub const INVALID: u64 = u64::MAX;
}

/// A #VMEXIT code read from [`control::EXIT_CODE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SvmExitCode(u64);

impl SvmExitCode {
    /// Wrap the raw `EXITCODE` field.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw code.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Whether `VMRUN` itself failed (invalid guest state / VMCB).
    #[must_use]
    pub const fn is_invalid(self) -> bool {
        self.0 == exit_code::INVALID
    }

    /// The intercepted exception vector, for an exception intercept
    /// (`0x40..=0x5F`).
    #[must_use]
    pub const fn exception_vector(self) -> Option<u8> {
        if self.0 >= exit_code::EXCEPTION_BASE && self.0 < exit_code::EXCEPTION_BASE + 32 {
            Some(((self.0 - exit_code::EXCEPTION_BASE) & 0x1F) as u8)
        } else {
            None
        }
    }

    /// The control register index, for a CR-read intercept (`0x00..=0x0F`).
    #[must_use]
    pub const fn cr_read_index(self) -> Option<u8> {
        if self.0 < exit_code::WRITE_CR_BASE {
            Some((self.0 & 0xF) as u8)
        } else {
            None
        }
    }

    /// The control register index, for a CR-write intercept (`0x10..=0x1F`).
    #[must_use]
    pub const fn cr_write_index(self) -> Option<u8> {
        if self.0 >= exit_code::WRITE_CR_BASE && self.0 < exit_code::WRITE_CR_BASE + 16 {
            Some(((self.0 - exit_code::WRITE_CR_BASE) & 0xF) as u8)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Exit-information decoding
// ---------------------------------------------------------------------------

/// Decoded `EXITINFO1` for an [`IOIO`](exit_code::IOIO) exit (APM §15.10.2).
///
/// `EXITINFO2` carries the RIP of the instruction following the `IN`/`OUT`,
/// which the backend loads to skip it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoioExitInfo(u64);

impl IoioExitInfo {
    /// Wrap the raw `EXITINFO1` of an IOIO exit.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Whether the access is an `IN` (true) or `OUT` (false).
    #[must_use]
    pub const fn is_in(self) -> bool {
        self.0 & 1 != 0
    }

    /// Whether this is a string access (`INS`/`OUTS`).
    #[must_use]
    pub const fn is_string(self) -> bool {
        self.0 & (1 << 2) != 0
    }

    /// Whether the instruction has a REP prefix.
    #[must_use]
    pub const fn is_rep(self) -> bool {
        self.0 & (1 << 3) != 0
    }

    /// Access size in bytes (1, 2, or 4), from the SZ8/SZ16/SZ32 bits.
    #[must_use]
    pub const fn access_size(self) -> u8 {
        if self.0 & (1 << 4) != 0 {
            1
        } else if self.0 & (1 << 5) != 0 {
            2
        } else {
            4
        }
    }

    /// The I/O port number (bits 31:16).
    #[must_use]
    pub const fn port(self) -> u16 {
        ((self.0 >> 16) & 0xFFFF) as u16
    }
}

/// Map an [`IOIO`](exit_code::IOIO) exit onto the arch-neutral
/// [`VmExit`](crate::VmExit).
///
/// The guest `RAX` (from [`save::RAX`]) supplies the `OUT` data, masked to
/// the access size. String (`INS`/`OUTS`) I/O returns `None`: it moves data
/// through guest memory and the backend emulates it separately rather than
/// as a single port access.
#[must_use]
pub const fn ioio_to_vmexit(info: IoioExitInfo, rax: u32) -> Option<crate::VmExit> {
    if info.is_string() {
        return None;
    }
    let port = info.port();
    let size = info.access_size();
    if info.is_in() {
        Some(crate::VmExit::IoIn { port, size })
    } else {
        let mask = match size {
            1 => 0xFF,
            2 => 0xFFFF,
            _ => 0xFFFF_FFFF,
        };
        Some(crate::VmExit::IoOut {
            port,
            size,
            data: rax & mask,
        })
    }
}

// ---------------------------------------------------------------------------
// I/O permissions map (IOPM)
// ---------------------------------------------------------------------------

/// Size of the SVM I/O permissions map: 12 KiB (three 4 KiB pages), APM
/// §15.10.1.
///
/// The map is a bitmap of the 65536 ports (8 KiB) plus a guard tail so a
/// multi-byte access near port `0xFFFF` cannot read past the region. A set bit
/// means "intercept this port"; `IOPM_BASE_PA` in the VMCB points at it and the
/// `IOIO_PROT` intercept ([`intercept1::IOIO_PROT`]) arms the check.
pub const IOPM_SIZE: usize = 3 * 4096;

/// The `(byte index, bit mask)` of `port` in the IOPM bitmap — bit `port`, i.e.
/// byte `port / 8`, bit `port % 8` (APM §15.10.1).
#[must_use]
pub const fn iopm_bit(port: u16) -> (usize, u8) {
    let p = port as usize;
    (p / 8, 1u8 << (p % 8))
}

/// Set the intercept bit for `port` in the IOPM `map`, so a guest access to it
/// takes an `IOIO` #VMEXIT. No-op if `map` is too short to hold the bit.
pub fn set_iopm_intercept(map: &mut [u8], port: u16) {
    let (byte, mask) = iopm_bit(port);
    if let Some(cell) = map.get_mut(byte) {
        *cell |= mask;
    }
}

/// Whether `port` is currently intercepted in the IOPM `map` (bit set). A
/// too-short map reads as not-intercepted.
#[must_use]
pub fn iopm_intercepts(map: &[u8], port: u16) -> bool {
    let (byte, mask) = iopm_bit(port);
    map.get(byte).is_some_and(|&cell| cell & mask != 0)
}

/// Arm port-I/O interception on a programmed VMCB.
///
/// Sets the `IOIO_PROT` intercept bit (preserving the other misc-1 intercepts)
/// and points `IOPM_BASE_PA` at `iopm_base_pa` (the physical base of an
/// [`IOPM_SIZE`] permissions map). With this set, a guest port access whose
/// IOPM bit is set takes an [`IOIO`](exit_code::IOIO) #VMEXIT.
pub fn enable_io_intercept(region: &mut [u8], iopm_base_pa: u64) {
    let misc1 = get_u32(region, control::INTERCEPT_MISC1) | intercept1::IOIO_PROT;
    put_u32(region, control::INTERCEPT_MISC1, misc1);
    put_u64(region, control::IOPM_BASE_PA, iopm_base_pa);
}

// ---------------------------------------------------------------------------
// MSR permissions map (MSRPM)
// ---------------------------------------------------------------------------

/// Size of the SVM MSR permissions map: 8 KiB (two 4 KiB pages), APM §15.11.
///
/// The map holds two bits per MSR — read (even) then write (odd) — for three
/// MSR windows: `0x0000_0000..=0x0000_1FFF` at byte 0, `0xC000_0000..=0xC000_1FFF`
/// at byte 0x800, and `0xC001_0000..=0xC001_1FFF` at byte 0x1000. A set bit
/// intercepts that access; `MSRPM_BASE_PA` in the VMCB points at it and the
/// `MSR_PROT` intercept ([`intercept1::MSR_PROT`]) arms the check.
pub const MSRPM_SIZE: usize = 2 * 4096;

/// The bit index of `msr`'s **read** permission in the MSRPM (its write bit is
/// the next one), or `None` if `msr` is outside the three mapped windows.
#[must_use]
pub const fn msrpm_read_bit(msr: u32) -> Option<usize> {
    let (region_byte, offset) = if msr <= 0x0000_1FFF {
        (0usize, msr)
    } else if msr >= 0xC000_0000 && msr <= 0xC000_1FFF {
        (0x800, msr - 0xC000_0000)
    } else if msr >= 0xC001_0000 && msr <= 0xC001_1FFF {
        (0x1000, msr - 0xC001_0000)
    } else {
        return None;
    };
    Some(region_byte * 8 + (offset as usize) * 2)
}

/// Set the read and/or write intercept bits for `msr` in the MSRPM `map`.
///
/// A set bit makes a guest `RDMSR`/`WRMSR` of `msr` take an
/// [`MSR`](exit_code::MSR) #VMEXIT. A no-op for an MSR outside the mapped
/// windows or a map too short to hold the bit.
pub fn set_msr_intercept(map: &mut [u8], msr: u32, read: bool, write: bool) {
    let Some(bit) = msrpm_read_bit(msr) else {
        return;
    };
    let byte = bit / 8;
    if let Some(cell) = map.get_mut(byte) {
        if read {
            *cell |= 1 << (bit % 8);
        }
        if write {
            *cell |= 1 << ((bit + 1) % 8);
        }
    }
}

/// Whether `msr`'s read access is currently intercepted in the MSRPM `map`.
#[must_use]
pub fn msr_read_intercepted(map: &[u8], msr: u32) -> bool {
    msrpm_read_bit(msr).is_some_and(|bit| {
        map.get(bit / 8)
            .is_some_and(|&cell| cell & (1 << (bit % 8)) != 0)
    })
}

/// Arm MSR interception on a programmed VMCB.
///
/// Sets the `MSR_PROT` intercept bit (preserving the other misc-1 intercepts)
/// and points `MSRPM_BASE_PA` at `msrpm_base_pa` (the physical base of an
/// [`MSRPM_SIZE`] permissions map). With this set, a guest `RDMSR`/`WRMSR` whose
/// MSRPM bit is set takes an [`MSR`](exit_code::MSR) #VMEXIT.
pub fn enable_msr_intercept(region: &mut [u8], msrpm_base_pa: u64) {
    let misc1 = get_u32(region, control::INTERCEPT_MISC1) | intercept1::MSR_PROT;
    put_u32(region, control::INTERCEPT_MISC1, misc1);
    put_u64(region, control::MSRPM_BASE_PA, msrpm_base_pa);
}

/// Map a #VMEXIT code that needs no further VMCB reads onto the arch-neutral
/// [`VmExit`](crate::VmExit).
///
/// Returns `None` for codes that require extra decoding the caller performs
/// with `EXITINFO1/2` and guest state (IOIO — see [`ioio_to_vmexit`]; nested
/// page faults; CPUID / MSR access), or that are simply not routed yet. An
/// [`INVALID`](exit_code::INVALID) code is never a normal guest exit; the
/// caller must abort.
#[must_use]
pub const fn simple_svm_exit_to_vmexit(code: SvmExitCode) -> Option<crate::VmExit> {
    if code.is_invalid() {
        return None;
    }
    match code.raw() {
        exit_code::HLT => Some(crate::VmExit::Hlt),
        exit_code::SHUTDOWN => Some(crate::VmExit::Shutdown),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// #VMEXIT dispatch-loop control
// ---------------------------------------------------------------------------

/// Length of the `CPUID` instruction in bytes (opcode `0F A2`).
///
/// The amount to advance guest `RIP` past an intercepted `CPUID` when the CPU
/// does not save `NEXT_RIP` (NRIP-save absent — see [`resume_rip_after`]).
pub const CPUID_INSN_LEN: u64 = 2;

/// Resolve the guest `RIP` to resume at after emulating-and-skipping a
/// fixed-length intercepted instruction.
///
/// The hardware-saved [`next_rip`] is authoritative when present (NRIP-save,
/// [`SvmFeatures::has_nrip_save`]); the CPU leaves it `0` otherwise, and the
/// caller falls back to `guest_rip + insn_len` for the known-length
/// instructions the minimal loop skips (APM §15.9). Wrapping-add so a guest
/// `RIP` near the top of the address space cannot panic.
#[must_use]
pub const fn resume_rip_after(next_rip: u64, guest_rip: u64, insn_len: u64) -> u64 {
    if next_rip != 0 {
        next_rip
    } else {
        guest_rip.wrapping_add(insn_len)
    }
}

/// How enlil's minimal guest-run loop should treat a #VMEXIT: stop (and why),
/// or resume the guest after emulating-and-skipping the intercepted
/// instruction.
///
/// This is the single control-flow decision the [`run`-loop](exit_code) makes
/// per exit — kept pure and host-tested here (the ISA seam, LOCKED PRINCIPLE
/// 2) so the privileged `VMRUN` driver above it stays a thin dispatcher. The
/// two resumable variants differ in how the loop advances RIP:
/// [`Cpuid`](RunLoopExit::Cpuid) skips a fixed-length instruction via
/// [`resume_rip_after`]; [`Io`](RunLoopExit::Io) advances to the `EXITINFO2`
/// RIP the hardware saved past the `IN`/`OUT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunLoopExit {
    /// Guest executed `HLT` — the minimal guest's expected clean stop.
    Halted,
    /// Guest triggered `SHUTDOWN` (triple fault) — stop without resetting the
    /// host (the intercept is what keeps it contained).
    ShutDown,
    /// `VMRUN` reported [`INVALID`](exit_code::INVALID) guest state — a failed
    /// consistency check; the loop must abort rather than re-`VMRUN`.
    Invalid,
    /// Intercepted `CPUID` (LOCKED PRINCIPLE 1) — emulate the stealth answer,
    /// then resume past the fixed-length instruction.
    Cpuid,
    /// Intercepted port I/O — decode `EXITINFO1`/[`IoioExitInfo`], emulate the
    /// access, then resume at the `EXITINFO2` RIP.
    Io,
    /// Intercepted `RDMSR`/`WRMSR` — the MSR number is in guest `RCX`, the
    /// read/write direction in `EXITINFO1` ([`msr_exit_is_write`]); emulate the
    /// stealth value, then resume past the fixed-length instruction.
    Msr,
    /// Nested page fault — the guest touched a guest-physical address the NPT
    /// does not map. `EXITINFO1` is the fault error code ([`NptFaultInfo`]),
    /// `EXITINFO2` the faulting guest-physical address; the caller maps the page
    /// (demand paging) or emulates MMIO, then resumes *without* advancing RIP so
    /// the faulting instruction re-executes.
    Npf,
    /// An exit the minimal loop does not route yet — stop and report the raw
    /// code to the caller.
    Unhandled,
}

/// Length of the `RDMSR`/`WRMSR` instructions in bytes (opcodes `0F 32`/`0F 30`).
///
/// The amount to advance guest `RIP` past an intercepted MSR access when the
/// CPU does not save `NEXT_RIP` (see [`resume_rip_after`]).
pub const MSR_INSN_LEN: u64 = 2;

/// Classify a #VMEXIT code for the minimal guest-run loop (see
/// [`RunLoopExit`]).
///
/// An [`INVALID`](exit_code::INVALID) code maps to [`RunLoopExit::Invalid`];
/// `HLT`/`SHUTDOWN` to their terminal variants; `CPUID`/`IOIO`/`MSR` to their
/// resumable variants; everything else to [`RunLoopExit::Unhandled`].
#[must_use]
pub const fn classify_run_loop_exit(code: SvmExitCode) -> RunLoopExit {
    if code.is_invalid() {
        return RunLoopExit::Invalid;
    }
    match code.raw() {
        exit_code::HLT => RunLoopExit::Halted,
        exit_code::SHUTDOWN => RunLoopExit::ShutDown,
        exit_code::CPUID => RunLoopExit::Cpuid,
        exit_code::IOIO => RunLoopExit::Io,
        exit_code::MSR => RunLoopExit::Msr,
        exit_code::NPF => RunLoopExit::Npf,
        _ => RunLoopExit::Unhandled,
    }
}

/// Decoded `EXITINFO1` for an [`NPF`](exit_code::NPF) exit — a page-fault
/// error code (APM §15.25.6). `EXITINFO2` carries the faulting guest-physical
/// address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NptFaultInfo(u64);

impl NptFaultInfo {
    /// Wrap the raw `EXITINFO1` of an NPF exit.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Whether the faulting nested-page-table entry was present.
    #[must_use]
    pub const fn was_present(self) -> bool {
        self.0 & 1 != 0
    }

    /// Whether the guest access was a write (true) or read (false).
    #[must_use]
    pub const fn was_write(self) -> bool {
        self.0 & (1 << 1) != 0
    }

    /// Whether the access came from user mode (CPL 3) in the guest.
    #[must_use]
    pub const fn was_user(self) -> bool {
        self.0 & (1 << 2) != 0
    }

    /// Whether a reserved bit was set in a walked entry.
    #[must_use]
    pub const fn reserved_bit_set(self) -> bool {
        self.0 & (1 << 3) != 0
    }

    /// Whether the access was an instruction fetch.
    #[must_use]
    pub const fn was_instruction_fetch(self) -> bool {
        self.0 & (1 << 4) != 0
    }
}

/// Whether an [`MSR`](exit_code::MSR) exit's `EXITINFO1` says the access was
/// a `WRMSR` (1) rather than `RDMSR` (0) — APM §15.11.
#[must_use]
pub const fn msr_exit_is_write(exit_info_1: u64) -> bool {
    exit_info_1 & 1 != 0
}

// ---------------------------------------------------------------------------
// VMCB field programming
// ---------------------------------------------------------------------------
//
// The decode layer above reads a VMCB the hardware has written; this half
// *writes* one, encoding the control and state-save fields a `VMRUN` needs.
// Everything here operates on a plain VMCB byte region (little-endian, per APM
// Appendix B) so it stays host-testable — the privileged `VMRUN`/`VMSAVE`/
// `VMLOAD` ops layer on top and are the only non-testable part.

/// `TLB_CONTROL` value 1: flush the guest's entire TLB on the next `VMRUN`
/// (APM §15.16.1). Used on a guest's first entry / after an ASID reuse.
pub const TLB_CONTROL_FLUSH_ALL: u8 = 1;

/// x86 reset value of the PAT MSR, programmed into [`save::G_PAT`] so nested
/// paging has a valid guest PAT (WB/WT/UC-/UC/WB/WT/UC-/UC across the 8 slots).
pub const DEFAULT_PAT: u64 = 0x0007_0406_0007_0406;

/// The always-set reserved bit 1 of `RFLAGS` — the minimum legal value.
pub const RFLAGS_RESERVED_ONE: u64 = 1 << 1;

/// A minimal legal guest `CR0` for a real-mode guest.
///
/// `ET` is set (bit 4) with paging and protection off, and `CD`/`NW` both clear
/// (the `NW=1,CD=0` pairing is a `VMRUN` consistency-check failure, so leave
/// both clear).
pub const GUEST_CR0_REAL_MODE: u64 = 1 << 4;

/// A guest segment as stored in a 16-byte VMCB save-area slot (APM Table B-2).
///
/// The 16 bytes are selector (`u16`), packed attributes (`u16`), limit
/// (`u32`), base (`u64`). The attribute field is AMD's *compressed* form of the
/// segment-descriptor attributes: bits 7:0 are descriptor bits 47:40
/// (Type/S/DPL/P) and bits 11:8 are descriptor bits 55:52 (AVL/L/D-B/G).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VmcbSegment {
    /// Segment selector.
    pub selector: u16,
    /// Packed segment attributes (12 bits used).
    pub attrib: u16,
    /// Segment limit.
    pub limit: u32,
    /// Segment base address.
    pub base: u64,
}

impl VmcbSegment {
    /// A real-mode code segment at `base` (present, readable/executable,
    /// byte-granular, 64 KiB limit) — attributes `0x9B`.
    #[must_use]
    pub const fn real_mode_code(base: u64) -> Self {
        Self {
            selector: 0,
            attrib: 0x009B,
            limit: 0xFFFF,
            base,
        }
    }

    /// A real-mode data segment at `base` (present, read/write, byte-granular,
    /// 64 KiB limit) — attributes `0x93`.
    #[must_use]
    pub const fn real_mode_data(base: u64) -> Self {
        Self {
            selector: 0,
            attrib: 0x0093,
            limit: 0xFFFF,
            base,
        }
    }

    /// A long-mode (64-bit) code segment — the `L` bit set (attributes
    /// `0x29B`), base and limit ignored by the CPU in 64-bit mode.
    #[must_use]
    pub const fn long_mode_code() -> Self {
        Self {
            selector: 0x0008,
            attrib: 0x029B,
            limit: 0xFFFF,
            base: 0,
        }
    }

    /// A long-mode data segment (attributes `0x93`).
    #[must_use]
    pub const fn long_mode_data() -> Self {
        Self {
            selector: 0x0010,
            attrib: 0x0093,
            limit: 0xFFFF,
            base: 0,
        }
    }
}

/// The register state for bringing up a minimal guest that runs until it
/// executes `HLT` — the second live-boot sub-milestone's target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MinimalGuestSetup {
    /// Guest ASID (must be nonzero; ASID 0 is the host).
    pub asid: u32,
    /// Physical base of the guest's nested page-table root (`nCR3`).
    pub nested_cr3: u64,
    /// Initial guest `RIP` (offset within `CS`).
    pub entry_ip: u64,
    /// Guest `CS` base — the linear address the first instruction lives at.
    pub code_base: u64,
    /// Initial guest `RSP`.
    pub stack_pointer: u64,
}

/// Program a zeroed VMCB region to run a minimal real-mode guest until it
/// executes `HLT` (APM §15.5).
///
/// This sets the required `VMRUN` intercept plus `HLT`, `SHUTDOWN`, and `CPUID`
/// (LOCKED PRINCIPLE 1) intercepts, the ASID and a full-TLB flush, nested
/// paging pointed at `nested_cr3`, and a flat real-mode state-save area
/// entering at `entry_ip` within a `code_base`-based `CS`.
///
/// The guest `EFER.SVME` bit is set because `VMRUN` fails its consistency check
/// otherwise (APM §15.5.1). The region is left ready for `VMRUN`; the caller
/// still allocates the nested page tables and provides their root.
///
/// # Errors
///
/// Returns [`VmcbRegionError::TooSmall`] if `region` is shorter than a VMCB.
pub fn program_minimal_hlt_guest(
    region: &mut [u8],
    setup: &MinimalGuestSetup,
) -> Result<(), VmcbRegionError> {
    if region.len() < VMCB_SIZE {
        return Err(VmcbRegionError::TooSmall {
            provided: region.len(),
            required: VMCB_SIZE,
        });
    }

    // Control area.
    put_u32(
        region,
        control::INTERCEPT_MISC1,
        intercept1::HLT | intercept1::SHUTDOWN | intercept1::CPUID,
    );
    put_u32(region, control::INTERCEPT_MISC2, intercept2::VMRUN);
    put_u32(region, control::GUEST_ASID, setup.asid);
    put_u8(region, control::TLB_CONTROL, TLB_CONTROL_FLUSH_ALL);
    put_u64(region, control::NESTED_CTL, control::NESTED_CTL_NP_ENABLE);
    put_u64(region, control::NESTED_CR3, setup.nested_cr3);

    // State-save area: a flat real-mode guest.
    put_u64(region, save::G_PAT, DEFAULT_PAT);
    put_u64(region, save::EFER, EFER_SVME);
    put_u64(region, save::CR0, GUEST_CR0_REAL_MODE);
    put_u64(region, save::CR3, 0);
    put_u64(region, save::CR4, 0);
    put_u64(region, save::RFLAGS, RFLAGS_RESERVED_ONE);
    put_u64(region, save::RIP, setup.entry_ip);
    put_u64(region, save::RSP, setup.stack_pointer);

    write_segment(
        region,
        save::CS,
        VmcbSegment::real_mode_code(setup.code_base),
    );
    let data = VmcbSegment::real_mode_data(0);
    write_segment(region, save::DS, data);
    write_segment(region, save::ES, data);
    write_segment(region, save::SS, data);
    write_segment(region, save::FS, data);
    write_segment(region, save::GS, data);

    Ok(())
}

/// Write a [`VmcbSegment`] into the 16-byte save-area slot at `offset` (one of
/// the [`save`] segment offsets).
pub fn write_segment(region: &mut [u8], offset: usize, seg: VmcbSegment) {
    put_u16(region, offset, seg.selector);
    put_u16(region, offset + 2, seg.attrib);
    put_u32(region, offset + 4, seg.limit);
    put_u64(region, offset + 8, seg.base);
}

/// Read the [`VmcbSegment`] from the save-area slot at `offset`.
#[must_use]
pub fn read_segment(region: &[u8], offset: usize) -> VmcbSegment {
    VmcbSegment {
        selector: get_u16(region, offset),
        attrib: get_u16(region, offset + 2),
        limit: get_u32(region, offset + 4),
        base: get_u64(region, offset + 8),
    }
}

/// The #VMEXIT code the hardware wrote to [`control::EXIT_CODE`].
#[must_use]
pub fn exit_code(region: &[u8]) -> SvmExitCode {
    SvmExitCode::from_raw(get_u64(region, control::EXIT_CODE))
}

/// The first exit-information field ([`control::EXIT_INFO_1`]).
#[must_use]
pub fn exit_info_1(region: &[u8]) -> u64 {
    get_u64(region, control::EXIT_INFO_1)
}

/// The second exit-information field ([`control::EXIT_INFO_2`]).
#[must_use]
pub fn exit_info_2(region: &[u8]) -> u64 {
    get_u64(region, control::EXIT_INFO_2)
}

/// The next-sequential-RIP the hardware saved ([`control::NEXT_RIP`]), valid
/// when [`SvmFeatures::has_nrip_save`].
#[must_use]
pub fn next_rip(region: &[u8]) -> u64 {
    get_u64(region, control::NEXT_RIP)
}

/// The guest `RAX` from the save area ([`save::RAX`]).
#[must_use]
pub fn guest_rax(region: &[u8]) -> u64 {
    get_u64(region, save::RAX)
}

/// The guest `RIP` from the save area ([`save::RIP`]).
#[must_use]
pub fn guest_rip(region: &[u8]) -> u64 {
    get_u64(region, save::RIP)
}

/// Overwrite the guest `RAX` in the save area (e.g. an emulated `IN` result).
pub fn set_guest_rax(region: &mut [u8], value: u64) {
    put_u64(region, save::RAX, value);
}

/// Overwrite the guest `RIP` in the save area (e.g. to advance past an
/// emulated instruction using [`next_rip`]).
pub fn set_guest_rip(region: &mut [u8], value: u64) {
    put_u64(region, save::RIP, value);
}

// Little-endian field accessors. The VMCB is always a full page here, so the
// curated Appendix-B offsets are in bounds.

const fn put_u8(region: &mut [u8], offset: usize, value: u8) {
    region[offset] = value;
}

fn put_u16(region: &mut [u8], offset: usize, value: u16) {
    region[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(region: &mut [u8], offset: usize, value: u32) {
    region[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(region: &mut [u8], offset: usize, value: u64) {
    region[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn get_u16(region: &[u8], offset: usize) -> u16 {
    let mut buf = [0u8; 2];
    buf.copy_from_slice(&region[offset..offset + 2]);
    u16::from_le_bytes(buf)
}

fn get_u32(region: &[u8], offset: usize) -> u32 {
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&region[offset..offset + 4]);
    u32::from_le_bytes(buf)
}

fn get_u64(region: &[u8], offset: usize) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&region[offset..offset + 8]);
    u64::from_le_bytes(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VmExit;

    #[test]
    fn svm_features_decode() {
        // Revision 1, 32768 ASIDs, NP + NRIPS + VMCB_CLEAN + FLUSH_BY_ASID.
        let f = SvmFeatures::from_cpuid(
            0x0000_0001,
            32768,
            feature::NESTED_PAGING
                | feature::NRIP_SAVE
                | feature::VMCB_CLEAN
                | feature::FLUSH_BY_ASID,
        );
        assert_eq!(f.revision(), 1);
        assert_eq!(f.nr_asids(), 32768);
        assert!(f.has_nested_paging());
        assert!(f.has_nrip_save());
        assert!(f.has(feature::VMCB_CLEAN));
        assert!(!f.has(feature::AVIC));
    }

    #[test]
    fn vm_cr_gates() {
        assert!(!vm_cr_svm_disabled(0));
        assert!(vm_cr_svm_disabled(VM_CR_SVMDIS));
        assert!(!vm_cr_svm_locked(VM_CR_SVMDIS));
        assert!(vm_cr_svm_locked(VM_CR_SVMDIS | VM_CR_LOCK));
    }

    #[test]
    fn vmcb_region_init_zeroes_and_rejects_short_buffers() {
        let mut region = [0xAAu8; VMCB_SIZE];
        init_vmcb_region(&mut region).expect("a full page initializes");
        assert!(region.iter().all(|&b| b == 0));

        let mut short = [0u8; 128];
        assert_eq!(
            init_vmcb_region(&mut short),
            Err(VmcbRegionError::TooSmall {
                provided: 128,
                required: VMCB_SIZE,
            })
        );
    }

    #[test]
    fn vmcb_layout_matches_the_apm() {
        // Spot anchors from APM Appendix B, Table B-1/B-2 (the same offsets
        // Linux's struct vmcb_control_area / vmcb_save_area encode).
        assert_eq!(control::EXIT_CODE, 0x070);
        assert_eq!(control::NESTED_CR3, 0x0B0);
        assert_eq!(control::NEXT_RIP, 0x0C8);
        assert_eq!(save::ES, 0x400);
        assert_eq!(save::EFER, 0x4D0);
        assert_eq!(save::RIP, 0x578);
        assert_eq!(save::RSP, 0x5D8);
        assert_eq!(save::RAX, 0x5F8);
    }

    #[test]
    fn exit_code_classification() {
        assert!(SvmExitCode::from_raw(exit_code::INVALID).is_invalid());
        // #PF is exception vector 14 → code 0x4E.
        let excp = SvmExitCode::from_raw(0x4E);
        assert_eq!(excp.exception_vector(), Some(14));
        assert_eq!(excp.cr_read_index(), None);
        // CR3 read → 0x03; CR4 write → 0x14.
        assert_eq!(SvmExitCode::from_raw(0x03).cr_read_index(), Some(3));
        assert_eq!(SvmExitCode::from_raw(0x14).cr_write_index(), Some(4));
        assert_eq!(
            SvmExitCode::from_raw(exit_code::HLT).exception_vector(),
            None
        );
    }

    #[test]
    fn ioio_decode_out_byte() {
        // OUT 0x3F8, AL: bit0=0 (OUT), SZ8, port 0x3F8 in bits 31:16.
        let info = IoioExitInfo::from_raw((0x3F8 << 16) | (1 << 4));
        assert!(!info.is_in());
        assert!(!info.is_string());
        assert_eq!(info.access_size(), 1);
        assert_eq!(info.port(), 0x3F8);
        assert_eq!(
            ioio_to_vmexit(info, 0xDEAD_BE41),
            Some(VmExit::IoOut {
                port: 0x3F8,
                size: 1,
                data: 0x41,
            })
        );
    }

    #[test]
    fn ioio_decode_in_word_and_string_rejection() {
        // IN AX, 0x64: bit0=1 (IN), SZ16.
        let info = IoioExitInfo::from_raw((0x64 << 16) | (1 << 5) | 1);
        assert!(info.is_in());
        assert_eq!(info.access_size(), 2);
        assert_eq!(
            ioio_to_vmexit(info, 0),
            Some(VmExit::IoIn {
                port: 0x64,
                size: 2,
            })
        );
        // OUTS with REP: routed to the string-emulation path, not a VmExit.
        let outs = IoioExitInfo::from_raw((0x3F8 << 16) | (1 << 4) | (1 << 2) | (1 << 3));
        assert!(outs.is_string());
        assert!(outs.is_rep());
        assert_eq!(ioio_to_vmexit(outs, 0), None);
    }

    #[test]
    fn iopm_bit_addresses_the_right_byte_and_bit() {
        assert_eq!(iopm_bit(0), (0, 0b0000_0001));
        assert_eq!(iopm_bit(7), (0, 0b1000_0000));
        assert_eq!(iopm_bit(8), (1, 0b0000_0001));
        assert_eq!(iopm_bit(0x80), (16, 0b0000_0001));
        // Top port lands in the 8 KiB bitmap, inside the 12 KiB region.
        assert_eq!(iopm_bit(0xFFFF), (8191, 0b1000_0000));
    }

    #[test]
    fn set_and_query_iopm_intercept() {
        let mut map = [0u8; IOPM_SIZE];
        assert!(!iopm_intercepts(&map, 0x80));
        set_iopm_intercept(&mut map, 0x80);
        assert!(iopm_intercepts(&map, 0x80));
        // Only that port's bit flipped; a neighbor in the same byte is clear.
        assert!(!iopm_intercepts(&map, 0x81));
        // A too-short map neither panics nor reports an intercept.
        let mut tiny = [0u8; 4];
        set_iopm_intercept(&mut tiny, 0xFFFF);
        assert!(!iopm_intercepts(&tiny, 0xFFFF));
    }

    #[test]
    fn enable_io_intercept_arms_ioio_without_disturbing_other_intercepts() {
        let mut region = [0u8; VMCB_SIZE];
        let setup = MinimalGuestSetup {
            asid: 1,
            nested_cr3: 0x2000,
            entry_ip: 0,
            code_base: 0x1000,
            stack_pointer: 0,
        };
        program_minimal_hlt_guest(&mut region, &setup).unwrap();
        enable_io_intercept(&mut region, 0x9_000);
        let misc1 = get_u32(&region, control::INTERCEPT_MISC1);
        assert_ne!(misc1 & intercept1::IOIO_PROT, 0);
        // The minimal guest's HLT/SHUTDOWN/CPUID intercepts are preserved.
        assert_ne!(misc1 & intercept1::HLT, 0);
        assert_ne!(misc1 & intercept1::SHUTDOWN, 0);
        assert_ne!(misc1 & intercept1::CPUID, 0);
        assert_eq!(get_u64(&region, control::IOPM_BASE_PA), 0x9_000);
    }

    #[test]
    fn msrpm_read_bit_maps_the_three_windows() {
        // Low window (byte 0): 2 bits per MSR.
        assert_eq!(msrpm_read_bit(0x0000_0000), Some(0));
        assert_eq!(msrpm_read_bit(0x0000_0001), Some(2));
        // High window at byte 0x800 (bit 0x4000).
        assert_eq!(msrpm_read_bit(0xC000_0000), Some(0x800 * 8));
        assert_eq!(msrpm_read_bit(0xC000_0080), Some(0x800 * 8 + 0x80 * 2)); // EFER
        // Extended window at byte 0x1000.
        assert_eq!(msrpm_read_bit(0xC001_0000), Some(0x1000 * 8));
        // Outside all three windows.
        assert_eq!(msrpm_read_bit(0x0000_2000), None);
        assert_eq!(msrpm_read_bit(0xC000_2000), None);
    }

    #[test]
    fn set_and_query_msr_intercept() {
        let mut map = [0u8; MSRPM_SIZE];
        assert!(!msr_read_intercepted(&map, 0x10));
        set_msr_intercept(&mut map, 0x10, true, false);
        assert!(msr_read_intercepted(&map, 0x10));
        // The write bit was not set; a neighbor MSR is untouched.
        assert!(!msr_read_intercepted(&map, 0x11));
        // Read + write both arm from a high-window MSR without panicking.
        set_msr_intercept(&mut map, 0xC000_0100, true, true);
        assert!(msr_read_intercepted(&map, 0xC000_0100));
        // An out-of-window MSR is a no-op, not a panic.
        set_msr_intercept(&mut map, 0xDEAD_BEEF, true, true);
        assert!(!msr_read_intercepted(&map, 0xDEAD_BEEF));
    }

    #[test]
    fn enable_msr_intercept_arms_msr_prot_without_disturbing_other_intercepts() {
        let mut region = [0u8; VMCB_SIZE];
        let setup = MinimalGuestSetup {
            asid: 1,
            nested_cr3: 0x2000,
            entry_ip: 0,
            code_base: 0x1000,
            stack_pointer: 0,
        };
        program_minimal_hlt_guest(&mut region, &setup).unwrap();
        enable_msr_intercept(&mut region, 0xA_000);
        let misc1 = get_u32(&region, control::INTERCEPT_MISC1);
        assert_ne!(misc1 & intercept1::MSR_PROT, 0);
        assert_ne!(misc1 & intercept1::HLT, 0);
        assert_ne!(misc1 & intercept1::CPUID, 0);
        assert_eq!(get_u64(&region, control::MSRPM_BASE_PA), 0xA_000);
    }

    #[test]
    fn simple_exits_route_hlt_and_shutdown() {
        assert_eq!(
            simple_svm_exit_to_vmexit(SvmExitCode::from_raw(exit_code::HLT)),
            Some(VmExit::Hlt)
        );
        assert_eq!(
            simple_svm_exit_to_vmexit(SvmExitCode::from_raw(exit_code::SHUTDOWN)),
            Some(VmExit::Shutdown)
        );
        assert_eq!(
            simple_svm_exit_to_vmexit(SvmExitCode::from_raw(exit_code::CPUID)),
            None
        );
        assert_eq!(
            simple_svm_exit_to_vmexit(SvmExitCode::from_raw(exit_code::INVALID)),
            None
        );
    }

    #[test]
    fn resume_rip_prefers_next_rip_else_advances() {
        // NRIP-save present: NEXT_RIP is authoritative regardless of length.
        assert_eq!(resume_rip_after(0x7C05, 0x7C00, CPUID_INSN_LEN), 0x7C05);
        // NRIP-save absent (NEXT_RIP == 0): fall back to guest_rip + insn_len.
        assert_eq!(resume_rip_after(0, 0x7C00, CPUID_INSN_LEN), 0x7C02);
        // Wrapping-add cannot panic at the top of the address space.
        assert_eq!(resume_rip_after(0, u64::MAX, 2), 1);
    }

    #[test]
    fn classify_run_loop_exit_covers_the_control_flow() {
        use exit_code::{CPUID, HLT, IOIO, SHUTDOWN};
        assert_eq!(
            classify_run_loop_exit(SvmExitCode::from_raw(HLT)),
            RunLoopExit::Halted
        );
        assert_eq!(
            classify_run_loop_exit(SvmExitCode::from_raw(SHUTDOWN)),
            RunLoopExit::ShutDown
        );
        assert_eq!(
            classify_run_loop_exit(SvmExitCode::from_raw(CPUID)),
            RunLoopExit::Cpuid
        );
        assert_eq!(
            classify_run_loop_exit(SvmExitCode::from_raw(IOIO)),
            RunLoopExit::Io
        );
        assert_eq!(
            classify_run_loop_exit(SvmExitCode::from_raw(exit_code::MSR)),
            RunLoopExit::Msr
        );
        // Invalid guest state is distinct from an unrouted exit — the loop
        // aborts rather than re-VMRUNs.
        assert_eq!(
            classify_run_loop_exit(SvmExitCode::from_raw(exit_code::INVALID)),
            RunLoopExit::Invalid
        );
        // NPF is routed to demand-map the page; distinct from Invalid.
        assert_eq!(
            classify_run_loop_exit(SvmExitCode::from_raw(exit_code::NPF)),
            RunLoopExit::Npf
        );
        // A genuinely-unrouted code (VMMCALL here) is Unhandled.
        assert_eq!(
            classify_run_loop_exit(SvmExitCode::from_raw(exit_code::VMMCALL)),
            RunLoopExit::Unhandled
        );
    }

    #[test]
    fn npf_error_code_decode() {
        // Write to a not-present entry from supervisor mode.
        let npf = NptFaultInfo::from_raw(1 << 1);
        assert!(!npf.was_present());
        assert!(npf.was_write());
        assert!(!npf.was_user());
        assert!(!npf.was_instruction_fetch());
        // Present + user instruction fetch.
        let fetch = NptFaultInfo::from_raw(1 | (1 << 2) | (1 << 4));
        assert!(fetch.was_present());
        assert!(fetch.was_user());
        assert!(fetch.was_instruction_fetch());
    }

    #[test]
    fn msr_exit_direction() {
        assert!(!msr_exit_is_write(0));
        assert!(msr_exit_is_write(1));
    }

    #[test]
    fn segment_round_trips_through_the_save_area() {
        let mut region = [0u8; VMCB_SIZE];
        let cs = VmcbSegment::real_mode_code(0xF_0000);
        write_segment(&mut region, save::CS, cs);
        assert_eq!(read_segment(&region, save::CS), cs);
        // The 16-byte on-disk layout: selector, attrib, limit, base.
        assert_eq!(get_u16(&region, save::CS), 0);
        assert_eq!(get_u16(&region, save::CS + 2), 0x009B);
        assert_eq!(get_u32(&region, save::CS + 4), 0xFFFF);
        assert_eq!(get_u64(&region, save::CS + 8), 0xF_0000);
    }

    #[test]
    fn long_mode_segments_set_the_l_bit() {
        assert_eq!(VmcbSegment::long_mode_code().attrib, 0x029B);
        assert_eq!(VmcbSegment::long_mode_data().attrib, 0x0093);
    }

    #[test]
    fn minimal_guest_programs_the_expected_fields() {
        let mut region = [0u8; VMCB_SIZE];
        let setup = MinimalGuestSetup {
            asid: 1,
            nested_cr3: 0x200_0000,
            entry_ip: 0x7C00,
            code_base: 0,
            stack_pointer: 0x8000,
        };
        program_minimal_hlt_guest(&mut region, &setup).expect("full page programs");

        // Intercepts: HLT + SHUTDOWN + CPUID in vector 1, VMRUN in vector 2.
        assert_eq!(
            get_u32(&region, control::INTERCEPT_MISC1),
            intercept1::HLT | intercept1::SHUTDOWN | intercept1::CPUID
        );
        assert_eq!(
            get_u32(&region, control::INTERCEPT_MISC2),
            intercept2::VMRUN
        );

        // ASID, TLB flush, nested paging.
        assert_eq!(get_u32(&region, control::GUEST_ASID), 1);
        assert_eq!(region[control::TLB_CONTROL], TLB_CONTROL_FLUSH_ALL);
        assert_eq!(
            get_u64(&region, control::NESTED_CTL),
            control::NESTED_CTL_NP_ENABLE
        );
        assert_eq!(get_u64(&region, control::NESTED_CR3), 0x200_0000);

        // Save area: SVME set, real-mode CR0, entry point, stack.
        assert_eq!(get_u64(&region, save::EFER), EFER_SVME);
        assert_eq!(get_u64(&region, save::CR0), GUEST_CR0_REAL_MODE);
        assert_eq!(get_u64(&region, save::CR3), 0);
        assert_eq!(get_u64(&region, save::CR4), 0);
        assert_eq!(get_u64(&region, save::RFLAGS), RFLAGS_RESERVED_ONE);
        assert_eq!(guest_rip(&region), 0x7C00);
        assert_eq!(get_u64(&region, save::RSP), 0x8000);
        assert_eq!(get_u64(&region, save::G_PAT), DEFAULT_PAT);

        // Segments: CS is real-mode code, the rest real-mode data.
        assert_eq!(
            read_segment(&region, save::CS),
            VmcbSegment::real_mode_code(0)
        );
        for off in [save::DS, save::ES, save::SS, save::FS, save::GS] {
            assert_eq!(read_segment(&region, off), VmcbSegment::real_mode_data(0));
        }
    }

    #[test]
    fn minimal_guest_rejects_short_regions() {
        let mut short = [0u8; 512];
        let setup = MinimalGuestSetup {
            asid: 1,
            nested_cr3: 0,
            entry_ip: 0,
            code_base: 0,
            stack_pointer: 0,
        };
        assert_eq!(
            program_minimal_hlt_guest(&mut short, &setup),
            Err(VmcbRegionError::TooSmall {
                provided: 512,
                required: VMCB_SIZE,
            })
        );
    }

    #[test]
    fn exit_field_and_rax_rip_accessors() {
        let mut region = [0u8; VMCB_SIZE];
        put_u64(&mut region, control::EXIT_CODE, exit_code::HLT);
        put_u64(&mut region, control::EXIT_INFO_1, 0xAABB);
        put_u64(&mut region, control::EXIT_INFO_2, 0xCCDD);
        put_u64(&mut region, control::NEXT_RIP, 0x7C02);
        assert_eq!(exit_code(&region).raw(), exit_code::HLT);
        assert_eq!(exit_info_1(&region), 0xAABB);
        assert_eq!(exit_info_2(&region), 0xCCDD);
        assert_eq!(next_rip(&region), 0x7C02);

        set_guest_rax(&mut region, 0x1234);
        set_guest_rip(&mut region, 0x7C02);
        assert_eq!(guest_rax(&region), 0x1234);
        assert_eq!(guest_rip(&region), 0x7C02);
    }
}
