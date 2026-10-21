//! A real Linux guest on the bare-metal kernel (dual-OS milestone M1).
//!
//! Until now the kernel's guests were hand-written programs a few bytes long.
//! This module boots an actual Linux `bzImage` (+ initramfs) staged from the
//! ESP and runs it under SVM until it shuts down, so every later step toward
//! "run my Linux and my Windows side by side" has a real operating system to
//! test against.
//!
//! The module is split so the logic that decides *what the guest sees* is pure
//! and host-tested, and only the code that touches the CPU is `uefi`-only:
//!
//! - [`plan_layout`] / [`e820_map`] — where the kernel, initrd and boot
//!   structures sit in guest RAM, and the memory map the guest is told about;
//! - [`Uart16550`] — the serial port the guest console talks to;
//! - [`cpuid_policy`] / [`MsrState`] — what the guest is told about the CPU;
//! - [`IoBus`] — the legacy port-I/O space (UART, CMOS, PCI config, resets);
//! - [`LineBuf`] — turns the guest's console bytes into log lines.
//!
//! None of these allocate beyond a `BTreeMap`, and each is a state machine with
//! plain-data state, which is what live migration will need later.

extern crate alloc;

use crate::svm::{CpuidRegs, sanitize_cpuid};
use crate::vdev::{PicPair, Pit8254};
use alloc::collections::BTreeMap;
use enlil_hal::bzimage::{BOOT_PARAMS_SIZE, parse_bzimage_header};

// ---------------------------------------------------------------------------
// Guest memory layout
// ---------------------------------------------------------------------------

/// One MiB.
pub const MIB: u64 = 1024 * 1024;
/// Guest RAM given to the Linux guest.
pub const GUEST_RAM_BYTES: u64 = 256 * MIB;
/// Guest-physical address of the guest's GDT (3 descriptors).
pub const GDT_GPA: u64 = 0x1000;
/// Guest-physical address of the guest's identity page tables (3 pages).
pub const PT_GPA: u64 = 0x2000;
/// Bytes of guest page tables (`PML4` + `PDPT` + one `PD`, up to 1 GiB).
pub const PT_BYTES: usize = 3 * 4096;
/// The guest's initial `RSP` (scratch page below the zero page).
pub const STACK_GPA: u64 = 0x6FF0;
/// Guest-physical address of the `boot_params` zero page.
pub const BOOT_PARAMS_GPA: u64 = 0x7000;
/// Guest-physical address of the NUL-terminated kernel command line.
pub const CMDLINE_GPA: u64 = 0x8000;
/// Longest command line (excluding the NUL) that fits before the next page.
pub const CMDLINE_MAX: usize = 4095;
/// Preferred load address of the protected-mode kernel (16 MiB, 2 MiB aligned).
pub const KERNEL_LOAD_GPA: u64 = 0x100_0000;
/// End of the low usable RAM range advertised in the e820 map (640 KiB - 4 KiB).
pub const LOW_MEM_END: u64 = 0x9F000;
/// Start of extended memory.
pub const EXT_MEM_START: u64 = 0x10_0000;
/// Offset of the 64-bit entry point from the start of the protected-mode kernel.
pub const ENTRY_64_OFFSET: u64 = 0x200;

/// `XLF_KERNEL_64` in `xloadflags`: the kernel has a 64-bit entry at `+0x200`.
const XLF_KERNEL_64: u16 = 1;
/// Minimum boot-protocol version: 2.12 added `xloadflags` / the 64-bit entry.
const MIN_PROTOCOL: u16 = 0x020C;
// Setup-header fields (Linux `Documentation/arch/x86/boot.rst`).
const HDR_INITRD_ADDR_MAX: usize = 0x22C;
const HDR_KERNEL_ALIGNMENT: usize = 0x230;
const HDR_RELOCATABLE: usize = 0x234;
const HDR_XLOADFLAGS: usize = 0x236;
const HDR_PREF_ADDRESS: usize = 0x258;
const HDR_INIT_SIZE: usize = 0x260;

fn rd_u16(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

fn rd_u32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn rd_u64(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

const fn align_up(v: u64, a: u64) -> u64 {
    (v + (a - 1)) & !(a - 1)
}

const fn align_down(v: u64, a: u64) -> u64 {
    v & !(a - 1)
}

/// Why a kernel image cannot be placed in guest RAM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutError {
    /// Not a Linux `bzImage` (no boot signature / `HdrS` magic).
    NotALinuxImage,
    /// Boot protocol older than 2.12 (carries the version found).
    ProtocolTooOld(u16),
    /// The kernel does not advertise a 64-bit entry point.
    No64BitEntry,
    /// Not relocatable and not linked for [`KERNEL_LOAD_GPA`].
    FixedLoadAddress,
    /// The image has no protected-mode payload.
    ImageTooSmall,
    /// The command line is longer than [`CMDLINE_MAX`].
    CmdlineTooLong,
    /// The kernel, its decompression workspace and the initrd do not fit.
    RamTooSmall,
}

/// Where everything goes in guest RAM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// Guest RAM size in bytes.
    pub ram_bytes: u64,
    /// Guest-physical load address of the protected-mode kernel.
    pub kernel_gpa: u64,
    /// Offset in the image where the protected-mode payload starts.
    pub kernel_src_off: usize,
    /// Bytes of protected-mode payload to copy.
    pub kernel_len: usize,
    /// Guest-physical load address of the initrd (`0` when there is none).
    pub initrd_gpa: u64,
    /// Initrd size in bytes.
    pub initrd_len: u64,
    /// Guest-physical address of the 64-bit entry point.
    pub entry_gpa: u64,
}

/// Plan where a kernel `image`, an initrd of `initrd_len` bytes and a command
/// line of `cmdline_len` bytes go in `ram_bytes` of guest RAM.
///
/// The kernel is loaded at [`KERNEL_LOAD_GPA`] (honouring its alignment) with
/// its whole decompression workspace (`init_size`) kept clear; the initrd goes
/// as high as the kernel's `initrd_addr_max` and the RAM allow.
///
/// # Errors
///
/// See [`LayoutError`].
pub fn plan_layout(
    image: &[u8],
    initrd_len: u64,
    cmdline_len: usize,
    ram_bytes: u64,
) -> Result<Layout, LayoutError> {
    if cmdline_len > CMDLINE_MAX {
        return Err(LayoutError::CmdlineTooLong);
    }
    let info = parse_bzimage_header(image).ok_or(LayoutError::NotALinuxImage)?;
    if info.protocol_version < MIN_PROTOCOL {
        return Err(LayoutError::ProtocolTooOld(info.protocol_version));
    }
    let xload = rd_u16(image, HDR_XLOADFLAGS).unwrap_or(0);
    if xload & XLF_KERNEL_64 == 0 {
        return Err(LayoutError::No64BitEntry);
    }
    let relocatable = image.get(HDR_RELOCATABLE).copied().unwrap_or(0) != 0;
    let pref = rd_u64(image, HDR_PREF_ADDRESS).unwrap_or(0);
    if !relocatable && pref != KERNEL_LOAD_GPA {
        return Err(LayoutError::FixedLoadAddress);
    }

    let src_off = info.protected_mode_kernel_offset;
    if src_off >= image.len() {
        return Err(LayoutError::ImageTooSmall);
    }
    let kernel_len = image.len() - src_off;

    let alignment = match rd_u32(image, HDR_KERNEL_ALIGNMENT).unwrap_or(0) {
        0 => 0x20_0000,
        a => u64::from(a).next_power_of_two(),
    };
    let kernel_gpa = align_up(KERNEL_LOAD_GPA, alignment);
    let init_size = u64::from(rd_u32(image, HDR_INIT_SIZE).unwrap_or(0));
    let kernel_span = init_size.max(kernel_len as u64);
    let kernel_end = align_up(kernel_gpa + kernel_span, 0x1000);
    if kernel_end > ram_bytes {
        return Err(LayoutError::RamTooSmall);
    }

    let initrd_gpa = if initrd_len == 0 {
        0
    } else {
        let max = u64::from(rd_u32(image, HDR_INITRD_ADDR_MAX).unwrap_or(0x37FF_FFFF)) + 1;
        let limit = ram_bytes.min(max);
        let gpa = align_down(
            limit
                .checked_sub(initrd_len)
                .ok_or(LayoutError::RamTooSmall)?,
            0x1000,
        );
        if gpa < kernel_end {
            return Err(LayoutError::RamTooSmall);
        }
        gpa
    };

    Ok(Layout {
        ram_bytes,
        kernel_gpa,
        kernel_src_off: src_off,
        kernel_len,
        initrd_gpa,
        initrd_len,
        entry_gpa: kernel_gpa + ENTRY_64_OFFSET,
    })
}

/// E820 type: usable RAM.
pub const E820_RAM: u32 = 1;
/// E820 type: reserved.
pub const E820_RESERVED: u32 = 2;

/// The memory map the guest is told about: low RAM, the legacy hole, and
/// extended RAM up to `ram_bytes`.
#[must_use]
pub const fn e820_map(ram_bytes: u64) -> [(u64, u64, u32); 3] {
    [
        (0, LOW_MEM_END, E820_RAM),
        (LOW_MEM_END, EXT_MEM_START - LOW_MEM_END, E820_RESERVED),
        (EXT_MEM_START, ram_bytes - EXT_MEM_START, E820_RAM),
    ]
}

/// The size of the `boot_params` zero page (re-exported for the runner).
pub const ZERO_PAGE_BYTES: usize = BOOT_PARAMS_SIZE;

/// The guest's flat GDT: null, 64-bit code (`0x08`), data (`0x10`).
#[must_use]
pub const fn gdt_bytes() -> [u8; 24] {
    let code = 0x00AF_9B00_0000_FFFFu64.to_le_bytes();
    let data = 0x00CF_9300_0000_FFFFu64.to_le_bytes();
    let mut g = [0u8; 24];
    let mut i = 0;
    while i < 8 {
        g[8 + i] = code[i];
        g[16 + i] = data[i];
        i += 1;
    }
    g
}

// ---------------------------------------------------------------------------
// 16550 UART
// ---------------------------------------------------------------------------

/// Base I/O port of COM1.
pub const COM1_BASE: u16 = 0x3F8;
/// Last I/O port of COM1.
pub const COM1_END: u16 = 0x3FF;

const LSR_DR: u8 = 0x01;
const LSR_THRE_TEMT: u8 = 0x60;
const MCR_LOOP: u8 = 0x10;
const MCR_OUT2: u8 = 0x08;
const IER_THRI: u8 = 0x02;

/// A 16550A UART.
///
/// Enough of the register file for Linux's 8250 driver to detect it (scratch
/// register, FIFO, loopback self-test) and drive its console. Transmit is
/// instantaneous; there is no receive path from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Uart16550 {
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    dll: u8,
    dlm: u8,
    fcr: u8,
    rx: Option<u8>,
    thre_int: bool,
}

impl Uart16550 {
    /// A reset UART.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            ier: 0,
            lcr: 0,
            mcr: 0,
            scr: 0,
            dll: 0,
            dlm: 0,
            fcr: 0,
            rx: None,
            thre_int: false,
        }
    }

    const fn dlab(&self) -> bool {
        self.lcr & 0x80 != 0
    }

    /// Whether the UART is asserting its interrupt line (THRE interrupt
    /// enabled and pending, with the PC's `OUT2` gate open).
    #[must_use]
    pub const fn irq_line(&self) -> bool {
        self.ier & IER_THRI != 0 && self.thre_int && self.mcr & MCR_OUT2 != 0
    }

    /// Read register `reg` (port - [`COM1_BASE`], `0..=7`).
    pub fn read(&mut self, reg: u16) -> u8 {
        match reg & 7 {
            0 => {
                if self.dlab() {
                    self.dll
                } else {
                    self.rx.take().unwrap_or(0)
                }
            }
            1 => {
                if self.dlab() {
                    self.dlm
                } else {
                    self.ier
                }
            }
            2 => {
                let fifo = if self.fcr & 1 != 0 { 0xC0 } else { 0 };
                if self.ier & IER_THRI != 0 && self.thre_int {
                    self.thre_int = false;
                    fifo | 0x02
                } else {
                    fifo | 0x01
                }
            }
            3 => self.lcr,
            4 => self.mcr,
            5 => LSR_THRE_TEMT | if self.rx.is_some() { LSR_DR } else { 0 },
            6 => {
                if self.mcr & MCR_LOOP != 0 {
                    // Loopback wires the modem outputs back to the inputs:
                    // RTS->CTS, DTR->DSR, OUT1->RI, OUT2->DCD.
                    ((self.mcr & 0x02) << 3)
                        | ((self.mcr & 0x01) << 5)
                        | ((self.mcr & 0x04) << 4)
                        | ((self.mcr & 0x08) << 4)
                } else {
                    0xB0 // DCD | DSR | CTS
                }
            }
            _ => self.scr,
        }
    }

    /// Write `val` to register `reg`; a transmitted byte goes to `tx`.
    pub fn write(&mut self, reg: u16, val: u8, tx: &mut impl FnMut(u8)) {
        match reg & 7 {
            0 => {
                if self.dlab() {
                    self.dll = val;
                } else if self.mcr & MCR_LOOP != 0 {
                    self.rx = Some(val);
                } else {
                    tx(val);
                    self.thre_int = true;
                }
            }
            1 => {
                if self.dlab() {
                    self.dlm = val;
                } else {
                    let enabling = self.ier & IER_THRI == 0 && val & IER_THRI != 0;
                    self.ier = val & 0x0F;
                    if enabling {
                        self.thre_int = true;
                    }
                }
            }
            2 => self.fcr = val & 0x01,
            3 => self.lcr = val,
            4 => self.mcr = val & 0x1F,
            7 => self.scr = val,
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Console line buffering
// ---------------------------------------------------------------------------

/// Collects the guest's console bytes into lines for the host log.
#[derive(Debug, Clone, Copy)]
pub struct LineBuf {
    buf: [u8; Self::CAP],
    len: usize,
}

impl LineBuf {
    /// Longest line emitted in one piece; longer lines are split.
    pub const CAP: usize = 200;

    /// An empty buffer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buf: [0; Self::CAP],
            len: 0,
        }
    }

    /// Feed one console byte; completed lines go to `sink`. Carriage returns
    /// are dropped and non-printable bytes become `.` so the log stays text.
    pub fn push(&mut self, byte: u8, sink: &mut impl FnMut(&str)) {
        match byte {
            b'\r' => {}
            b'\n' => self.flush(sink),
            0x20..=0x7E | b'\t' => {
                self.buf[self.len] = byte;
                self.len += 1;
                if self.len == Self::CAP {
                    self.flush(sink);
                }
            }
            _ => {
                self.buf[self.len] = b'.';
                self.len += 1;
                if self.len == Self::CAP {
                    self.flush(sink);
                }
            }
        }
    }

    /// Emit any buffered partial line.
    pub fn flush(&mut self, sink: &mut impl FnMut(&str)) {
        if self.len > 0 {
            if let Ok(line) = core::str::from_utf8(&self.buf[..self.len]) {
                sink(line);
            }
            self.len = 0;
        }
    }
}

impl Default for LineBuf {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// CPUID policy
// ---------------------------------------------------------------------------

/// `CR4.OSXSAVE` — the guest has enabled XSAVE.
const CR4_OSXSAVE: u64 = 1 << 18;

/// What the guest is told in answer to `CPUID(leaf, subleaf)`.
///
/// Starts from the host's answer, applies the hypervisor stealth
/// ([`sanitize_cpuid`]), then hides everything the guest cannot be given: the
/// local APIC (the guest runs on the legacy PIC), the host's CPU count and
/// topology (the guest has one CPU), MCE/MTRR/PMU/thermal/CET features it has no
/// MSRs for, nested SVM, and the speculation-control MSRs. `OSXSAVE` mirrors
/// the *guest's* `CR4`, not the host's.
#[must_use]
pub fn cpuid_policy(leaf: u32, subleaf: u32, host: CpuidRegs, guest_cr4: u64) -> CpuidRegs {
    let mut r = sanitize_cpuid(leaf, host);
    match leaf {
        1 => {
            // One CPU, APIC id 0; keep the CLFLUSH line size (bits 15:8).
            r.ebx &= 0x0000_FFFF;
            // No APIC, MTRR, MCE/MCA; single-threaded (no HTT).
            r.edx &= !((1 << 9) | (1 << 12) | (1 << 7) | (1 << 14) | (1 << 28));
            // No x2APIC, TSC-deadline, MONITOR, PDCM.
            r.ecx &= !((1 << 21) | (1 << 24) | (1 << 3) | (1 << 15));
            // OSXSAVE reflects the guest's CR4.
            r.ecx = (r.ecx & !(1 << 27))
                | if guest_cr4 & CR4_OSXSAVE != 0 {
                    1 << 27
                } else {
                    0
                };
        }
        4 | 5 | 6 | 0xB | 0x1F => r = CpuidRegs::default(),
        7 if subleaf == 0 => {
            // No TSC_ADJUST, TSX (HLE/RTM); no PKU/OSPKE, WAITPKG, CET shadow
            // stacks; no IBT; no speculation-control MSRs.
            r.ebx &= !((1 << 1) | (1 << 4) | (1 << 11));
            r.ecx &= !((1 << 3) | (1 << 4) | (1 << 5) | (1 << 7));
            r.edx &= !((0xFFu32 << 24) | (1 << 20));
        }
        0xD if subleaf == 1 => {
            // No XSAVES / XFD: no IA32_XSS.
            r.eax &= !((1 << 3) | (1 << 4));
        }
        0x8000_0001 => {
            // No SVM, TOPOEXT, core/NB perf counters, IBS, MWAITX.
            r.ecx &= !((1 << 2) | (1 << 22) | (1 << 23) | (1 << 24) | (1 << 10) | (1 << 29));
        }
        0x8000_0008 => {
            // One core; no IBPB/IBRS/STIBP/SSBD.
            r.ecx = 0;
            r.ebx &= !((1 << 12) | (1 << 14) | (1 << 15) | (1 << 24) | (1 << 25) | (1 << 26));
        }
        0x8000_000A | 0x8000_001D | 0x8000_001E | 0x8000_001F | 0x8000_0021 | 0x8000_0026 => {
            r = CpuidRegs::default();
        }
        _ => {}
    }
    r
}

// ---------------------------------------------------------------------------
// MSR policy
// ---------------------------------------------------------------------------

/// MSRs the guest reads and writes natively: the CPU virtualizes them in the
/// VMCB save area (or, for `PAT`, as `G_PAT`), so no exit is needed.
pub const MSR_PASSTHROUGH: &[u32] = &[
    0xC000_0080, // EFER
    0xC000_0081, // STAR
    0xC000_0082, // LSTAR
    0xC000_0083, // CSTAR
    0xC000_0084, // SFMASK
    0xC000_0100, // FS_BASE
    0xC000_0101, // GS_BASE
    0xC000_0102, // KERNEL_GS_BASE
    0x0000_0174, // SYSENTER_CS
    0x0000_0175, // SYSENTER_ESP
    0x0000_0176, // SYSENTER_EIP
    0x0000_0277, // PAT
    0x0000_01D9, // DEBUGCTL
];

/// The IA32 `TSC` MSR.
pub const MSR_TSC: u32 = 0x10;
/// The IA32 `APIC_BASE` MSR.
pub const MSR_APIC_BASE: u32 = 0x1B;
/// The IA32 `TSC_ADJUST` MSR (hidden by CPUID, but modelled as zero for kernels
/// that read it regardless).
pub const MSR_TSC_ADJUST: u32 = 0x3B;

/// MSRs the guest may write; the value is remembered and read back, but never
/// reaches the host.
const MSR_WRITABLE_SHADOWED: &[u32] = &[
    MSR_APIC_BASE,
    MSR_TSC_ADJUST,
    0x0000_008B, // BIOS_SIGN_ID / patch level
    0xC000_0103, // TSC_AUX
    0xC001_0010, // SYSCFG
    0xC001_0015, // HWCR
    0xC001_001F, // NB_CFG
];

fn is_writable(msr: u32) -> bool {
    MSR_WRITABLE_SHADOWED.contains(&msr) || (0xC001_1020..=0xC001_102F).contains(&msr)
}

/// Result of a guest `RDMSR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsrRead {
    /// Return this value.
    Value(u64),
    /// Return the guest's virtual `TSC`.
    Tsc,
    /// Inject `#GP(0)`: the MSR does not exist for this guest.
    Gp,
}

/// Result of a guest `WRMSR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsrWrite {
    /// Accepted.
    Ok,
    /// The guest set its `TSC` to this value.
    Tsc(u64),
    /// Inject `#GP(0)`.
    Gp,
}

/// The guest's view of the MSRs that exit to the hypervisor.
#[derive(Debug, Clone, Default)]
pub struct MsrState {
    shadow: BTreeMap<u32, u64>,
}

impl MsrState {
    /// A fresh MSR state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// What a guest `RDMSR` of `msr` returns.
    #[must_use]
    pub fn read(&self, msr: u32) -> MsrRead {
        if msr == MSR_TSC {
            return MsrRead::Tsc;
        }
        if let Some(&v) = self.shadow.get(&msr) {
            return MsrRead::Value(v);
        }
        match msr {
            // BSP, local APIC disabled (the guest runs on the legacy PIC).
            MSR_APIC_BASE => MsrRead::Value(0xFEE0_0100),
            // Patch level, SYSCFG, HWCR, NB_CFG, TSC_AUX, DE_CFG-class: zero.
            MSR_TSC_ADJUST | 0x0000_008B | 0xC000_0103 | 0xC001_0010 | 0xC001_0015
            | 0xC001_001F => MsrRead::Value(0),
            m if (0xC001_1020..=0xC001_102F).contains(&m) => MsrRead::Value(0),
            _ => MsrRead::Gp,
        }
    }

    /// Apply a guest `WRMSR` of `value` to `msr`.
    pub fn write(&mut self, msr: u32, value: u64) -> MsrWrite {
        if msr == MSR_TSC {
            return MsrWrite::Tsc(value);
        }
        if is_writable(msr) {
            self.shadow.insert(msr, value);
            MsrWrite::Ok
        } else {
            MsrWrite::Gp
        }
    }
}

// ---------------------------------------------------------------------------
// Legacy port-I/O space
// ---------------------------------------------------------------------------

/// What an I/O write did beyond updating device state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoEvent {
    /// Nothing the run loop must act on.
    None,
    /// The guest asked the machine to reset (keyboard-controller pulse or
    /// `0xCF9`).
    Reset,
}

/// The guest's legacy I/O port space.
///
/// Models the UART, the PIC pair, the PIT, CMOS/RTC, the PCI configuration ports
/// (no devices) and the reset ports. Everything else reads as an absent device
/// (all ones) and ignores writes.
///
/// Devices that depend on time are driven from `set_time` (the host TSC) and
/// [`poll_devices`](Self::poll_devices), which also raises their interrupt lines
/// on the PIC; the run loop then asks [`pending_vector`](Self::pending_vector) /
/// [`ack`](Self::ack) what to inject.
#[derive(Debug, Clone, Copy)]
pub struct IoBus {
    /// COM1.
    pub uart: Uart16550,
    /// The master/slave interrupt controllers.
    pub pic: PicPair,
    /// The system timer (channel 0 + the speaker gate port).
    pub pit: Pit8254,
    now: u64,
    cmos_index: u8,
    pci_addr: u32,
}

/// Mask `v` to an access of `size` bytes.
const fn mask(v: u32, size: u8) -> u32 {
    match size {
        1 => v & 0xFF,
        2 => v & 0xFFFF,
        _ => v,
    }
}

impl IoBus {
    /// A reset I/O bus whose timers run against a host clock of `tsc_hz`.
    #[must_use]
    pub const fn new(tsc_hz: u64) -> Self {
        Self {
            uart: Uart16550::new(),
            pic: PicPair::new(),
            pit: Pit8254::new(tsc_hz),
            now: 0,
            cmos_index: 0,
            pci_addr: 0,
        }
    }

    /// Tell the bus the current host time (TSC).
    pub const fn set_time(&mut self, now: u64) {
        self.now = now;
    }

    /// Let time-driven devices run: fire the PIT's IRQ 0 if due and mirror the
    /// UART's interrupt output onto IRQ 4.
    pub const fn poll_devices(&mut self) {
        if self.pit.poll(self.now) {
            self.pic.set_irq(0, true);
            self.pic.set_irq(0, false);
        }
        self.pic.set_irq(4, self.uart.irq_line());
    }

    /// The vector of the interrupt waiting to be delivered, if any.
    #[must_use]
    pub const fn pending_vector(&self) -> Option<u8> {
        self.pic.pending_vector()
    }

    /// Acknowledge the next interrupt (INTA) and return its vector.
    pub const fn ack(&mut self) -> Option<u8> {
        self.pic.ack()
    }

    /// The host TSC value at which a device next needs attention.
    #[must_use]
    pub const fn next_deadline(&self) -> Option<u64> {
        self.pit.next_deadline()
    }

    /// A fixed CMOS image: 2026-10-15 00:00:00, 24-hour BCD, no update in
    /// progress, battery good.
    const fn cmos(index: u8) -> u8 {
        match index & 0x7F {
            0x06 => 0x04, // weekday
            0x07 => 0x15, // day
            0x08 => 0x10, // month
            // Year 2026, and status A (no update in progress): both 0x26.
            0x09 | 0x0A => 0x26,
            0x0B => 0x02, // status B: 24h, BCD
            0x0D => 0x80, // status D: valid RAM/time
            0x32 => 0x20, // century
            _ => 0,
        }
    }

    /// Handle a guest `IN` of `size` bytes from `port`.
    pub fn read(&mut self, port: u16, size: u8) -> u32 {
        let v = match port {
            COM1_BASE..=COM1_END => u32::from(self.uart.read(port - COM1_BASE)),
            0x20 | 0x21 | 0xA0 | 0xA1 | 0x4D0 | 0x4D1 => {
                u32::from(self.pic.read(port).unwrap_or(0xFF))
            }
            0x40 => u32::from(self.pit.read_data(self.now)),
            // Channels 1 and 2 are not modelled; the control port is write-only.
            0x41 | 0x42 => 0,
            0x61 => u32::from(self.pit.read_port61(self.now)),
            0x71 => u32::from(Self::cmos(self.cmos_index)),
            0xCF8 => self.pci_addr,
            // No PCI devices (config reads at 0xCFC..=0xCFF see an empty bus)
            // and every other port: an absent device reads as all ones.
            _ => u32::MAX,
        };
        mask(v, size)
    }

    /// Handle a guest `OUT` of `size` bytes to `port`; UART output goes to `tx`.
    pub fn write(&mut self, port: u16, size: u8, val: u32, tx: &mut impl FnMut(u8)) -> IoEvent {
        let val = mask(val, size);
        match port {
            COM1_BASE..=COM1_END => {
                self.uart.write(port - COM1_BASE, (val & 0xFF) as u8, tx);
            }
            0x20 | 0x21 | 0xA0 | 0xA1 | 0x4D0 | 0x4D1 => {
                self.pic.write(port, (val & 0xFF) as u8);
            }
            0x40 => self.pit.write_data((val & 0xFF) as u8, self.now),
            0x43 => self.pit.write_control((val & 0xFF) as u8, self.now),
            0x61 => self.pit.write_port61((val & 0xFF) as u8),
            0x70 => self.cmos_index = (val & 0xFF) as u8,
            0xCF8 => self.pci_addr = val,
            // Keyboard-controller reset pulse.
            0x64 if val & 0xFF == 0xFE => return IoEvent::Reset,
            // Reset control register: bit 2 triggers a reset.
            0xCF9 if val & 0x04 != 0 => return IoEvent::Reset,
            _ => {}
        }
        IoEvent::None
    }
}

// ---------------------------------------------------------------------------
// MMIO space
// ---------------------------------------------------------------------------

/// The guest's memory-mapped I/O space above RAM.
///
/// No devices are modelled yet, so every address reads as an absent device (all
/// ones) and ignores writes. The first touches of each distinct address are
/// reported so that what a guest probes can be seen and the devices it needs
/// added deliberately.
#[derive(Debug, Clone, Default)]
pub struct MmioBus {
    seen: BTreeMap<u64, u32>,
}

impl MmioBus {
    /// How many distinct addresses are reported before the log goes quiet.
    pub const LOG_LIMIT: usize = 48;

    /// An empty MMIO bus.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an access to `gpa`; `true` the first time an address is seen
    /// (while under [`LOG_LIMIT`](Self::LOG_LIMIT)), i.e. when it should be logged.
    pub fn note(&mut self, gpa: u64) -> bool {
        let first = !self.seen.contains_key(&gpa);
        *self.seen.entry(gpa).or_insert(0) += 1;
        first && self.seen.len() <= Self::LOG_LIMIT
    }

    /// A `size`-byte read at `gpa`: an absent device answers all ones.
    #[must_use]
    pub const fn read(&self, _gpa: u64, size: u8) -> u64 {
        crate::insn::mask_to(u64::MAX, size)
    }

    /// A `size`-byte write of `value` at `gpa`: ignored.
    pub const fn write(&mut self, _gpa: u64, _size: u8, _value: u64) {}
}

// ---------------------------------------------------------------------------
// Hardware runner (uefi only)
// ---------------------------------------------------------------------------

#[cfg(target_os = "uefi")]
pub use hw::boot;

// This module only compiles for x86_64-unknown-uefi, where `usize` is 64 bits, so
// `u64 as usize` is lossless; the remaining casts are bounded register fields
// (sizes, vectors, offsets) and the guest-memory accessors check bounds first.
#[cfg(target_os = "uefi")]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_ptr_alignment,
    clippy::too_many_lines
)]
mod hw {
    use super::{
        BOOT_PARAMS_GPA, CMDLINE_GPA, GDT_GPA, GUEST_RAM_BYTES, IoBus, IoEvent, LineBuf, MIB,
        MSR_PASSTHROUGH, MmioBus, MsrRead, MsrState, MsrWrite, PT_BYTES, PT_GPA, STACK_GPA,
        cpuid_policy, e820_map, gdt_bytes, plan_layout,
    };
    use crate::handoff::{BootHandoff, StagedFile};
    use crate::insn::{AccessKind, decode_mem_access, extend, mask_to, merge_reg, translate_gva};
    use crate::serial::SerialPort;
    use crate::svm::{GuestGprs, host_cpuid, vmrun};
    use alloc::alloc::{Layout as AllocLayout, alloc_zeroed};
    use alloc::collections::BTreeSet;
    use alloc::format;
    use alloc::string::String;
    use core::fmt::Write as _;
    use core::sync::atomic::Ordering;
    use enlil_hal::bzimage::build_boot_params;
    use enlil_hal::npt::{build_identity_npt_2mib, build_npt_2mib};
    use enlil_hal::region::{IoPermissionsMap, MsrPermissionsMap, Vmcb};
    use enlil_hal::svm::{
        CPUID_INSN_LEN, IoioExitInfo, LongModeGuestSetup, MSR_INSN_LEN, RunLoopExit, VMCB_SIZE,
        VmcbSegment, classify_run_loop_exit, control, enable_io_intercept, enable_msr_intercept,
        encode_event_inj, encode_vintr, event_type, exit_code, exit_info_1, exit_info_2, guest_rax,
        guest_rip, int_control, intercept2, msr_exit_is_write, msrpm_read_bit, next_rip,
        program_long_mode_hlt_guest, read_save_u64, resume_rip_after, save, set_event_inj,
        set_guest_rax, set_guest_rip, set_int_control, set_intr_intercept, set_tsc_offset,
        tsc_offset, write_save_u64, write_segment,
    };

    /// Command line used when the ESP has no `cmdline.txt`.
    const DEFAULT_CMDLINE: &str = "console=ttyS0,115200 earlycon=uart8250,io,0x3f8 \
         rdinit=/init acpi=off noapic nolapic nokaslr panic=-1 loglevel=7";
    /// The host LAPIC-timer vector used to force a `#VMEXIT` when a device
    /// event (a PIT tick) is due while the guest is running.
    const TIMER_VECTOR: u8 = 0x40;
    /// `INT_CONTROL.V_IRQ`: a virtual interrupt is posted and not yet delivered.
    const V_IRQ: u64 = 1 << 8;
    /// `INT_CONTROL.V_INTR_MASKING`: left clear so the guest's own `IF` gates
    /// interrupts (the host runs with `IF = 0`).
    const V_INTR_MASKING: u64 = 1 << 24;
    /// Length of `HLT` (`F4`).
    const HLT_INSN_LEN: u64 = 1;
    /// Wall-clock cap on one run so a wedged guest cannot hang the boot.
    const RUN_SECONDS: u64 = 120;
    /// How many distinct unknown MSRs are logged before going quiet.
    const MSR_LOG_LIMIT: usize = 40;

    /// Why the guest stopped running.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Stop {
        /// The guest triple-faulted (`SHUTDOWN`).
        Shutdown,
        /// The guest asked the machine to reset.
        Reset,
        /// `HLT` with interrupts disabled: nothing can ever wake it.
        DeadHalt,
        /// `HLT` with interrupts enabled but no wake source modelled yet.
        IdleHalt,
        /// A nested page fault inside guest RAM, which is fully mapped (carries
        /// the faulting GPA): a bug, not a device access.
        Npf(u64),
        /// An MMIO access by an instruction the decoder does not handle
        /// (carries the guest-physical address of the access).
        MmioDecode(u64),
        /// `VMRUN` rejected the VMCB.
        Invalid,
        /// A string I/O instruction the I/O bus does not model.
        StringIo(u16),
        /// An exit the runner does not handle (carries the raw code).
        Unhandled(u64),
        /// The wall-clock cap elapsed.
        Timeout,
    }

    /// Per-run exit counters.
    #[derive(Debug, Default)]
    struct Counts {
        exits: u64,
        io: u64,
        cpuid: u64,
        msr: u64,
        hlt: u64,
        mmio: u64,
        intr: u64,
        injected: u64,
    }

    /// View `file`'s staged bytes.
    ///
    /// # Safety
    ///
    /// `file` must describe live `LOADER_DATA` pages (it does: the UEFI stage
    /// staged them and the planner reserves them).
    const unsafe fn staged(file: StagedFile) -> &'static [u8] {
        if !file.is_staged() {
            return &[];
        }
        // SAFETY: delegated to the caller; the kernel runs identity-mapped.
        unsafe {
            core::slice::from_raw_parts(
                core::ptr::with_exposed_provenance(file.base as usize),
                file.len,
            )
        }
    }

    /// Volatile access to guest RAM: the guest mutates it behind the
    /// compiler's back, so reads must not be cached or reordered.
    #[derive(Clone, Copy)]
    struct GuestMem {
        base: *const u8,
        len: usize,
    }

    impl GuestMem {
        fn read_u8(self, gpa: u64) -> Option<u8> {
            let i = usize::try_from(gpa).ok().filter(|&i| i < self.len)?;
            // SAFETY: `i` is in bounds of the live, leaked guest-RAM window.
            Some(unsafe { self.base.add(i).read_volatile() })
        }

        fn read_u64(self, gpa: u64) -> Option<u64> {
            let i = usize::try_from(gpa)
                .ok()
                .filter(|&i| i + 8 <= self.len && i % 8 == 0)?;
            // SAFETY: bounds and 8-byte alignment checked above.
            Some(unsafe { self.base.add(i).cast::<u64>().read_volatile() })
        }
    }

    /// Read a guest general-purpose register by x86 encoding number.
    fn get_reg(vmcb: &[u8], g: &GuestGprs, reg: u8) -> u64 {
        match reg {
            0 => guest_rax(vmcb),
            1 => g.rcx,
            2 => g.rdx,
            3 => g.rbx,
            4 => read_save_u64(vmcb, save::RSP),
            5 => g.rbp,
            6 => g.rsi,
            7 => g.rdi,
            8 => g.r8,
            9 => g.r9,
            10 => g.r10,
            11 => g.r11,
            12 => g.r12,
            13 => g.r13,
            14 => g.r14,
            _ => g.r15,
        }
    }

    /// Write a guest general-purpose register by x86 encoding number.
    fn set_reg(vmcb: &mut [u8], g: &mut GuestGprs, reg: u8, value: u64) {
        match reg {
            0 => set_guest_rax(vmcb, value),
            1 => g.rcx = value,
            2 => g.rdx = value,
            3 => g.rbx = value,
            4 => write_save_u64(vmcb, save::RSP, value),
            5 => g.rbp = value,
            6 => g.rsi = value,
            7 => g.rdi = value,
            8 => g.r8 = value,
            9 => g.r9 = value,
            10 => g.r10 = value,
            11 => g.r11 = value,
            12 => g.r12 = value,
            13 => g.r13 = value,
            14 => g.r14 = value,
            _ => g.r15 = value,
        }
    }

    /// Fetch up to 15 instruction bytes at guest-virtual `rip`, translating each
    /// page through the guest's own page tables. Returns the bytes and how many
    /// were readable.
    fn fetch_insn(mem: GuestMem, cr3: u64, rip: u64) -> ([u8; 15], usize) {
        let mut buf = [0u8; 15];
        let read_entry = |gpa: u64| mem.read_u64(gpa);
        let mut n = 0;
        let mut page_gpa: Option<u64> = None;
        while n < buf.len() {
            let gva = rip.wrapping_add(n as u64);
            if n == 0 || gva.trailing_zeros() >= 12 {
                page_gpa = translate_gva(cr3, gva & !0xFFF, &read_entry);
            }
            let Some(page) = page_gpa else { break };
            let Some(byte) = mem.read_u8(page + (gva & 0xFFF)) else {
                break;
            };
            buf[n] = byte;
            n += 1;
        }
        (buf, n)
    }

    /// Emulate the guest instruction that took a nested page fault at `gpa`
    /// (an MMIO access): decode it, perform the access on `bus`, update the
    /// destination register and advance `RIP`.
    fn emulate_mmio(
        serial: &SerialPort,
        mem: GuestMem,
        vmcb: &mut [u8],
        gprs: &mut GuestGprs,
        bus: &mut MmioBus,
        gpa: u64,
    ) -> Result<(), Stop> {
        let rip = guest_rip(vmcb);
        let (bytes, n) = fetch_insn(mem, read_save_u64(vmcb, save::CR3), rip);
        let Some(access) = decode_mem_access(&bytes[..n]) else {
            serial.write_str(&format!(
                "enlil kernel: linux: undecodable MMIO instruction at RIP {rip:#x} (gpa {gpa:#x}): {:02x?}\n",
                &bytes[..n]
            ));
            return Err(Stop::MmioDecode(gpa));
        };
        let size = access.size;
        let first = bus.note(gpa);
        match access.kind {
            AccessKind::Load {
                reg,
                high8,
                sign_extend,
                dest_size,
            } => {
                let value = extend(bus.read(gpa, size), size, dest_size, sign_extend);
                let (target, hi) = if high8 { (reg - 4, true) } else { (reg, false) };
                let merged = merge_reg(get_reg(vmcb, gprs, target), value, dest_size, hi);
                set_reg(vmcb, gprs, target, merged);
                if first {
                    serial.write_str(&format!(
                        "enlil kernel: linux: MMIO read  {size}B at {gpa:#x} -> {value:#x} (absent device) from RIP {rip:#x}\n"
                    ));
                }
            }
            AccessKind::Store { reg, high8 } => {
                let (source, shift) = if high8 { (reg - 4, 8) } else { (reg, 0) };
                let value = mask_to(get_reg(vmcb, gprs, source) >> shift, size);
                bus.write(gpa, size, value);
                if first {
                    serial.write_str(&format!(
                        "enlil kernel: linux: MMIO write {size}B at {gpa:#x} = {value:#x} (ignored) from RIP {rip:#x}\n"
                    ));
                }
            }
            AccessKind::StoreImm { value } => {
                let value = mask_to(value, size);
                bus.write(gpa, size, value);
                if first {
                    serial.write_str(&format!(
                        "enlil kernel: linux: MMIO write {size}B at {gpa:#x} = {value:#x} (ignored) from RIP {rip:#x}\n"
                    ));
                }
            }
        }
        set_guest_rip(vmcb, rip.wrapping_add(access.len as u64));
        Ok(())
    }

    /// Clear both bits of every passthrough MSR in a fully-armed MSRPM.
    fn arm_msrpm(map: &mut [u8]) {
        map.fill(0xFF);
        for &msr in MSR_PASSTHROUGH {
            if let Some(bit) = msrpm_read_bit(msr)
                && let Some(cell) = map.get_mut(bit / 8)
            {
                *cell &= !(0b11 << (bit % 8));
            }
        }
    }

    fn or_u32(region: &mut [u8], off: usize, bits: u32) {
        if let Some(cell) = region.get_mut(off..off + 4)
            && let Ok(arr) = <[u8; 4]>::try_from(&*cell)
        {
            cell.copy_from_slice(&(u32::from_le_bytes(arr) | bits).to_le_bytes());
        }
    }

    /// Boot the staged Linux guest and run it until it stops, logging every
    /// console line it prints as `[linux] ...`. Does nothing when no kernel was
    /// staged or SVM is not up.
    pub fn boot(serial: &SerialPort, handoff: &BootHandoff) {
        let payload = handoff.linux_guest;
        if !payload.has_kernel() {
            return;
        }
        if !crate::kernel::SVM_READY.load(Ordering::Acquire) {
            serial.write_str("enlil kernel: linux: SVM is not available - cannot run a guest\n");
            return;
        }
        // SAFETY: the staged extents come from the UEFI stage's handoff.
        let (image, initrd, cmdline_raw) = unsafe {
            (
                staged(payload.kernel),
                staged(payload.initrd),
                staged(payload.cmdline),
            )
        };
        serial.write_str(&format!(
            "enlil kernel: linux: staged kernel {} KiB, initrd {} KiB\n",
            image.len() / 1024,
            initrd.len() / 1024
        ));

        let tsc_hz = crate::kernel::TSC_HZ.load(Ordering::Acquire);
        let mut cmdline = match core::str::from_utf8(cmdline_raw) {
            Ok(s) if !s.trim().is_empty() => String::from(s.trim()),
            _ => String::from(DEFAULT_CMDLINE),
        };
        if tsc_hz != 0 && !cmdline.contains("tsc_early_khz") {
            let _ = write!(cmdline, " tsc_early_khz={}", tsc_hz / 1000);
        }

        let layout = match plan_layout(image, initrd.len() as u64, cmdline.len(), GUEST_RAM_BYTES) {
            Ok(l) => l,
            Err(e) => {
                serial.write_str(&format!(
                    "enlil kernel: linux: cannot place the kernel: {e:?}\n"
                ));
                return;
            }
        };
        let Some(ram_spa) = crate::kernel::guest_ram::LINUX.take(GUEST_RAM_BYTES, 2 * MIB) else {
            serial.write_str("enlil kernel: linux: no planned guest-RAM window for the guest\n");
            return;
        };
        // SAFETY: the planner carved this span for exactly this guest, reserved
        // it, and `take` hands it to one caller; it is identity-mapped.
        let ram = unsafe {
            core::slice::from_raw_parts_mut(
                core::ptr::with_exposed_provenance_mut::<u8>(ram_spa as usize),
                GUEST_RAM_BYTES as usize,
            )
        };
        ram.fill(0);

        // Kernel, initrd, command line, zero page, GDT, page tables.
        let k = layout.kernel_gpa as usize;
        ram[k..k + layout.kernel_len].copy_from_slice(&image[layout.kernel_src_off..]);
        if layout.initrd_gpa != 0 {
            let i = layout.initrd_gpa as usize;
            ram[i..i + initrd.len()].copy_from_slice(initrd);
        }
        let c = CMDLINE_GPA as usize;
        ram[c..c + cmdline.len()].copy_from_slice(cmdline.as_bytes());
        let Some(zero_page) = build_boot_params(
            image,
            CMDLINE_GPA as u32,
            layout.initrd_gpa as u32,
            initrd.len() as u32,
            &e820_map(GUEST_RAM_BYTES),
        ) else {
            serial.write_str("enlil kernel: linux: image too short for a zero page\n");
            return;
        };
        let bp = BOOT_PARAMS_GPA as usize;
        ram[bp..bp + zero_page.len()].copy_from_slice(&zero_page);
        let g = GDT_GPA as usize;
        ram[g..g + 24].copy_from_slice(&gdt_bytes());
        let pt = PT_GPA as usize;
        let Ok(guest_pt) =
            build_identity_npt_2mib(&mut ram[pt..pt + PT_BYTES], PT_GPA, GUEST_RAM_BYTES)
        else {
            serial.write_str("enlil kernel: linux: guest page tables did not fit\n");
            return;
        };
        let guest_cr3 = guest_pt.ncr3;

        // Nested page tables: guest-physical [0, RAM) -> the carved window.
        // SAFETY: nonzero, 4 KiB-aligned layout; the buffer is leaked below.
        let npt_raw = unsafe {
            alloc_zeroed(
                AllocLayout::from_size_align(PT_BYTES, 4096).unwrap_or(AllocLayout::new::<u8>()),
            )
        };
        if npt_raw.is_null() {
            serial.write_str("enlil kernel: linux: no memory for the nested page tables\n");
            return;
        }
        let npt_pa = npt_raw as u64;
        // SAFETY: `npt_raw` owns PT_BYTES zeroed bytes, leaked for the VM's life.
        let npt_buf = unsafe { core::slice::from_raw_parts_mut(npt_raw, PT_BYTES) };
        let Ok(npt) = build_npt_2mib(npt_buf, npt_pa, ram_spa, GUEST_RAM_BYTES) else {
            serial.write_str("enlil kernel: linux: nested page tables did not fit\n");
            return;
        };

        // The VMCB: a long-mode guest entering the kernel's 64-bit entry point.
        let (Ok(mut vmcb), Ok(iopm), Ok(mut msrpm)) = (
            Vmcb::new(),
            IoPermissionsMap::intercept_all(),
            MsrPermissionsMap::new(),
        ) else {
            serial.write_str("enlil kernel: linux: no memory for the VMCB / permission maps\n");
            return;
        };
        let setup = LongModeGuestSetup {
            asid: 1,
            nested_cr3: npt.ncr3,
            guest_cr3,
            entry_ip: layout.entry_gpa,
            stack_pointer: STACK_GPA,
        };
        if program_long_mode_hlt_guest(vmcb.as_bytes_mut(), &setup).is_err() {
            serial.write_str("enlil kernel: linux: VMCB programming failed\n");
            return;
        }
        let v = vmcb.as_bytes_mut();
        write_segment(
            v,
            save::GDTR,
            VmcbSegment {
                selector: 0,
                attrib: 0,
                limit: 0x17,
                base: GDT_GPA,
            },
        );
        enable_io_intercept(v, iopm.base_addr());
        // A host timer interrupt must exit the guest (never be delivered into
        // it): it is how a due device event pre-empts a running guest.
        set_intr_intercept(v);
        crate::idt::install_timer_gate(TIMER_VECTOR);
        arm_msrpm(msrpm.as_bytes_mut());
        enable_msr_intercept(v, msrpm.base_addr());
        // Privileged SVM instructions are not the guest's to run.
        or_u32(
            v,
            control::INTERCEPT_MISC2,
            intercept2::VMMCALL
                | intercept2::VMLOAD
                | intercept2::VMSAVE
                | intercept2::STGI
                | intercept2::CLGI
                | intercept2::SKINIT,
        );
        // The guest's TSC starts near zero; the host's absolute TSC never leaks.
        let t0 = crate::tsc::read_tsc();
        set_tsc_offset(v, t0.cast_signed().wrapping_neg());

        // SAFETY: nonzero, 4 KiB-aligned; leaked so it outlives every VMRUN.
        let host_save = unsafe {
            alloc_zeroed(
                AllocLayout::from_size_align(4096, 4096).unwrap_or(AllocLayout::new::<u8>()),
            )
        };
        if host_save.is_null() {
            serial.write_str("enlil kernel: linux: no memory for the host-save page\n");
            return;
        }
        let vmcb_pa = vmcb.base_addr();
        core::mem::forget(vmcb);
        core::mem::forget(iopm);
        core::mem::forget(msrpm);

        serial.write_str(&format!(
            "enlil kernel: linux: entering the guest at GPA {:#x} (RAM {} MiB at {:#x}, cmdline: {})\n",
            layout.entry_gpa,
            GUEST_RAM_BYTES / MIB,
            ram_spa,
            cmdline
        ));

        let mut gprs = GuestGprs {
            rsi: BOOT_PARAMS_GPA,
            ..GuestGprs::default()
        };
        let deadline = t0.saturating_add(tsc_hz.max(1).saturating_mul(RUN_SECONDS));
        let mem = GuestMem {
            base: core::ptr::with_exposed_provenance::<u8>(ram_spa as usize),
            len: GUEST_RAM_BYTES as usize,
        };
        let (stop, counts) = run(
            serial,
            vmcb_pa,
            host_save as u64,
            &mut gprs,
            deadline,
            mem,
            tsc_hz.max(1),
        );
        report(serial, vmcb_pa, stop, &counts);
    }

    /// An exclusive view of the VMCB page at `pa`.
    ///
    /// # Safety
    ///
    /// `pa` must be the live VMCB and no `VMRUN` may be in flight while the view
    /// is used.
    const unsafe fn vmcb_view(pa: u64) -> &'static mut [u8] {
        // SAFETY: delegated to the caller.
        unsafe {
            core::slice::from_raw_parts_mut(
                core::ptr::with_exposed_provenance_mut::<u8>(pa as usize),
                VMCB_SIZE,
            )
        }
    }

    /// The guest executed `HLT` with interrupts enabled: wait on the host until
    /// something can wake it (a posted interrupt, or a device event coming
    /// due). Returns a stop reason when nothing ever can, or the run timed out.
    fn wait_for_wake(bus: &mut IoBus, vmcb: &[u8], deadline: u64) -> Option<Stop> {
        loop {
            let now = crate::tsc::read_tsc();
            bus.set_time(now);
            bus.poll_devices();
            if int_control(vmcb) & V_IRQ != 0 || bus.pending_vector().is_some() {
                return None;
            }
            if now > deadline {
                return Some(Stop::Timeout);
            }
            match bus.next_deadline() {
                // Nothing scheduled and nothing pending: the guest slept
                // forever (every interrupt source is masked or stopped).
                None => return Some(Stop::IdleHalt),
                Some(due) if due > now => {
                    while crate::tsc::read_tsc() < due {
                        core::hint::spin_loop();
                    }
                }
                Some(_) => {}
            }
        }
    }

    /// The `#VMEXIT` dispatch loop for the Linux guest.
    fn run(
        serial: &SerialPort,
        vmcb_pa: u64,
        host_save_pa: u64,
        gprs: &mut GuestGprs,
        deadline: u64,
        mem: GuestMem,
        tsc_hz: u64,
    ) -> (Stop, Counts) {
        let mut bus = IoBus::new(tsc_hz);
        let mut mmio = MmioBus::new();
        // The host timer deadline currently armed (avoids re-arming on every exit).
        let mut armed: Option<u64> = None;
        let mut msrs = MsrState::new();
        let mut console = LineBuf::new();
        let mut counts = Counts::default();
        let mut msr_seen: BTreeSet<(bool, u32)> = BTreeSet::new();
        let mut emit = |line: &str| {
            serial.write_str("[linux] ");
            serial.write_str(line);
            serial.write_str("\n");
        };

        let stop = loop {
            // Let devices run, post the next interrupt for the guest to take
            // when it can, and make sure the host timer will pre-empt it in
            // time for the next device event.
            {
                let now = crate::tsc::read_tsc();
                bus.set_time(now);
                bus.poll_devices();
                // SAFETY: `vmcb_pa` is our live, page-sized VMCB; no VMRUN is
                // in flight, so this exclusive view is not aliased.
                let pre = unsafe { vmcb_view(vmcb_pa) };
                if int_control(pre) & V_IRQ == 0
                    && let Some(vector) = bus.ack()
                {
                    set_int_control(pre, encode_vintr(vector, 0xF) & !V_INTR_MASKING);
                    counts.injected += 1;
                }
                if let Some(due) = bus.next_deadline()
                    && armed != Some(due)
                {
                    crate::apic::arm_tsc_deadline_timer(TIMER_VECTOR, due.saturating_sub(now));
                    armed = Some(due);
                }
            }
            // SAFETY: the VMCB is VMRUN-ready, `gprs` is a live local and
            // `host_save_pa` is our owned zeroed page.
            unsafe { vmrun(vmcb_pa, host_save_pa, core::ptr::from_mut(gprs)) };
            counts.exits += 1;
            // SAFETY: `vmcb_pa` is our live, page-sized VMCB.
            let vmcb = unsafe {
                core::slice::from_raw_parts_mut(
                    core::ptr::with_exposed_provenance_mut::<u8>(vmcb_pa as usize),
                    VMCB_SIZE,
                )
            };
            set_event_inj(vmcb, 0);
            if counts.exits % 4096 == 0 && crate::tsc::read_tsc() > deadline {
                break Stop::Timeout;
            }

            let code = exit_code(vmcb);
            match classify_run_loop_exit(code) {
                RunLoopExit::Cpuid => {
                    counts.cpuid += 1;
                    let leaf = u32::try_from(guest_rax(vmcb) & 0xFFFF_FFFF).unwrap_or(0);
                    let sub = u32::try_from(gprs.rcx & 0xFFFF_FFFF).unwrap_or(0);
                    let r = cpuid_policy(
                        leaf,
                        sub,
                        host_cpuid(leaf, sub),
                        read_save_u64(vmcb, save::CR4),
                    );
                    set_guest_rax(vmcb, u64::from(r.eax));
                    gprs.rbx = u64::from(r.ebx);
                    gprs.rcx = u64::from(r.ecx);
                    gprs.rdx = u64::from(r.edx);
                    set_guest_rip(
                        vmcb,
                        resume_rip_after(next_rip(vmcb), guest_rip(vmcb), CPUID_INSN_LEN),
                    );
                }
                RunLoopExit::Io => {
                    counts.io += 1;
                    let info = IoioExitInfo::from_raw(exit_info_1(vmcb));
                    if info.is_string() {
                        break Stop::StringIo(info.port());
                    }
                    let (port, size) = (info.port(), info.access_size());
                    if info.is_in() {
                        let val = u64::from(bus.read(port, size));
                        let rax = guest_rax(vmcb);
                        set_guest_rax(
                            vmcb,
                            match size {
                                1 => (rax & !0xFF) | val,
                                2 => (rax & !0xFFFF) | val,
                                _ => val,
                            },
                        );
                    } else {
                        let data = u32::try_from(guest_rax(vmcb) & 0xFFFF_FFFF).unwrap_or(0);
                        let event =
                            bus.write(port, size, data, &mut |b| console.push(b, &mut emit));
                        if event == IoEvent::Reset {
                            break Stop::Reset;
                        }
                    }
                    set_guest_rip(vmcb, exit_info_2(vmcb));
                }
                RunLoopExit::Msr => {
                    counts.msr += 1;
                    let msr = u32::try_from(gprs.rcx & 0xFFFF_FFFF).unwrap_or(0);
                    let mut fault = false;
                    if msr_exit_is_write(exit_info_1(vmcb)) {
                        let val =
                            ((gprs.rdx & 0xFFFF_FFFF) << 32) | (guest_rax(vmcb) & 0xFFFF_FFFF);
                        match msrs.write(msr, val) {
                            MsrWrite::Ok => {}
                            MsrWrite::Tsc(t) => {
                                let offset = t.wrapping_sub(crate::tsc::read_tsc());
                                set_tsc_offset(vmcb, offset.cast_signed());
                            }
                            MsrWrite::Gp => fault = true,
                        }
                    } else {
                        let value = match msrs.read(msr) {
                            MsrRead::Value(v) => Some(v),
                            MsrRead::Tsc => Some(
                                crate::tsc::read_tsc()
                                    .wrapping_add(tsc_offset(vmcb).cast_unsigned()),
                            ),
                            MsrRead::Gp => None,
                        };
                        match value {
                            Some(v) => {
                                set_guest_rax(vmcb, v & 0xFFFF_FFFF);
                                gprs.rdx = v >> 32;
                            }
                            None => fault = true,
                        }
                    }
                    if fault {
                        // Report each distinct unmodelled MSR once.
                        let is_write = msr_exit_is_write(exit_info_1(vmcb));
                        if msr_seen.len() < MSR_LOG_LIMIT && msr_seen.insert((is_write, msr)) {
                            serial.write_str(&format!(
                                "enlil kernel: linux: guest {} of unmodelled MSR {msr:#x} at RIP {:#x} -> #GP\n",
                                if is_write { "WRMSR" } else { "RDMSR" },
                                guest_rip(vmcb)
                            ));
                        }
                        // Inject #GP(0) and retry nothing: the faulting
                        // instruction is not skipped.
                        set_event_inj(vmcb, encode_event_inj(13, event_type::EXCEPTION, Some(0)));
                    } else {
                        set_guest_rip(
                            vmcb,
                            resume_rip_after(next_rip(vmcb), guest_rip(vmcb), MSR_INSN_LEN),
                        );
                    }
                }
                RunLoopExit::Halted => {
                    counts.hlt += 1;
                    // The intercept leaves RIP on the HLT: step past it.
                    set_guest_rip(
                        vmcb,
                        resume_rip_after(next_rip(vmcb), guest_rip(vmcb), HLT_INSN_LEN),
                    );
                    if read_save_u64(vmcb, save::RFLAGS) & (1 << 9) == 0 {
                        // HLT with interrupts off: nothing can ever wake it.
                        break Stop::DeadHalt;
                    }
                    if let Some(stop) = wait_for_wake(&mut bus, vmcb, deadline) {
                        break stop;
                    }
                }
                RunLoopExit::ShutDown => break Stop::Shutdown,
                RunLoopExit::Invalid => break Stop::Invalid,
                RunLoopExit::Npf => {
                    let gpa = exit_info_2(vmcb);
                    // Guest RAM is fully mapped, so a fault inside it is a bug;
                    // anything above it is a device access.
                    if gpa < GUEST_RAM_BYTES {
                        break Stop::Npf(gpa);
                    }
                    counts.mmio += 1;
                    if let Err(stop) = emulate_mmio(serial, mem, vmcb, gprs, &mut mmio, gpa) {
                        break stop;
                    }
                }
                RunLoopExit::Intr => {
                    counts.intr += 1;
                    armed = None;
                    // Take the pending host timer interrupt (its handler EOIs):
                    // open the interrupt window for one instruction.
                    // SAFETY: the timer gate is installed; this only lets the
                    // already-pending LAPIC timer interrupt be serviced.
                    unsafe {
                        core::arch::asm!("sti", "nop", "cli", options(nomem, nostack));
                    }
                }
                RunLoopExit::Vmmcall | RunLoopExit::Unhandled
                    if (0x81..=0x86).contains(&code.raw()) =>
                {
                    // A privileged SVM instruction: the guest gets #UD.
                    set_event_inj(vmcb, encode_event_inj(6, event_type::EXCEPTION, None));
                }
                RunLoopExit::Vmmcall
                | RunLoopExit::Unhandled
                | RunLoopExit::Rdtsc
                | RunLoopExit::Exception { .. } => break Stop::Unhandled(code.raw()),
            }
        };
        console.flush(&mut emit);
        (stop, counts)
    }

    /// Log why the guest stopped, with enough VMCB state to debug it.
    fn report(serial: &SerialPort, vmcb_pa: u64, stop: Stop, counts: &Counts) {
        // SAFETY: `vmcb_pa` is our live, page-sized VMCB.
        let vmcb = unsafe {
            core::slice::from_raw_parts(
                core::ptr::with_exposed_provenance::<u8>(vmcb_pa as usize),
                VMCB_SIZE,
            )
        };
        let why = match stop {
            Stop::DeadHalt => String::from("halted with interrupts off (powered off)"),
            other => format!("{other:?}"),
        };
        serial.write_str(&format!(
            "enlil kernel: linux: guest stopped: {why} after {} exits (io {}, cpuid {}, msr {}, mmio {}, hlt {}, intr {}, irq-injected {})\n",
            counts.exits, counts.io, counts.cpuid, counts.msr, counts.mmio, counts.hlt, counts.intr, counts.injected
        ));
        serial.write_str(&format!(
            "enlil kernel: linux:   RIP {:#x} RSP {:#x} RFLAGS {:#x} CR0 {:#x} CR3 {:#x} CR4 {:#x} EFER {:#x} exit {:#x}\n",
            guest_rip(vmcb),
            read_save_u64(vmcb, save::RSP),
            read_save_u64(vmcb, save::RFLAGS),
            read_save_u64(vmcb, save::CR0),
            read_save_u64(vmcb, save::CR3),
            read_save_u64(vmcb, save::CR4),
            read_save_u64(vmcb, save::EFER),
            exit_code(vmcb).raw(),
        ));
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)] // test fixtures build small, bounded values
mod tests {
    extern crate alloc;

    use super::*;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    /// A minimal valid bzImage header: boot flag, `HdrS`, protocol 2.15,
    /// 64-bit entry, relocatable, 2 MiB alignment, with `payload` bytes after
    /// 5 setup sectors.
    fn image(payload: usize, init_size: u32, initrd_max: u32) -> Vec<u8> {
        let mut img = vec![0u8; 0xA00 + payload];
        img[0x1F1] = 4; // setup_sects -> protected mode at (4+1)*512
        img[0x1FE..0x200].copy_from_slice(&0xAA55u16.to_le_bytes());
        img[0x202..0x206].copy_from_slice(b"HdrS");
        img[0x206..0x208].copy_from_slice(&0x020Fu16.to_le_bytes());
        img[0x211] = 1; // LOADED_HIGH
        img[HDR_INITRD_ADDR_MAX..HDR_INITRD_ADDR_MAX + 4]
            .copy_from_slice(&initrd_max.to_le_bytes());
        img[HDR_KERNEL_ALIGNMENT..HDR_KERNEL_ALIGNMENT + 4]
            .copy_from_slice(&0x20_0000u32.to_le_bytes());
        img[HDR_RELOCATABLE] = 1;
        img[HDR_XLOADFLAGS..HDR_XLOADFLAGS + 2].copy_from_slice(&1u16.to_le_bytes());
        img[HDR_INIT_SIZE..HDR_INIT_SIZE + 4].copy_from_slice(&init_size.to_le_bytes());
        img
    }

    #[test]
    fn layout_places_kernel_at_16mib_and_initrd_at_the_top() {
        let img = image(8 * MIB as usize, 40 * MIB as u32, 0x7FFF_FFFF);
        let l = plan_layout(&img, 4 * MIB, 64, 256 * MIB).unwrap();
        assert_eq!(l.kernel_gpa, 0x100_0000);
        assert_eq!(l.entry_gpa, 0x100_0200);
        assert_eq!(l.kernel_src_off, 0xA00);
        assert_eq!(l.kernel_len, 8 * MIB as usize);
        // initrd ends exactly at the top of RAM and clears the workspace.
        assert_eq!(l.initrd_gpa + l.initrd_len, 256 * MIB);
        assert!(l.initrd_gpa >= 0x100_0000 + 40 * MIB);
    }

    #[test]
    fn layout_respects_initrd_addr_max() {
        let img = image(MIB as usize, 8 * MIB as u32, 0x0FFF_FFFF); // < 256 MiB
        let l = plan_layout(&img, MIB, 0, 512 * MIB).unwrap();
        assert_eq!(l.initrd_gpa + l.initrd_len, 0x1000_0000);
    }

    #[test]
    fn layout_without_an_initrd_leaves_it_zero() {
        let img = image(MIB as usize, 8 * MIB as u32, 0x7FFF_FFFF);
        assert_eq!(plan_layout(&img, 0, 0, 256 * MIB).unwrap().initrd_gpa, 0);
    }

    #[test]
    fn layout_rejects_bad_images_and_overflows() {
        let good = image(MIB as usize, 8 * MIB as u32, 0x7FFF_FFFF);
        assert_eq!(
            plan_layout(&[0; 64], 0, 0, 256 * MIB),
            Err(LayoutError::NotALinuxImage)
        );
        let mut old = good.clone();
        old[0x206..0x208].copy_from_slice(&0x0206u16.to_le_bytes());
        assert_eq!(
            plan_layout(&old, 0, 0, 256 * MIB),
            Err(LayoutError::ProtocolTooOld(0x0206))
        );
        let mut no64 = good.clone();
        no64[HDR_XLOADFLAGS] = 0;
        assert_eq!(
            plan_layout(&no64, 0, 0, 256 * MIB),
            Err(LayoutError::No64BitEntry)
        );
        let mut fixed = good.clone();
        fixed[HDR_RELOCATABLE] = 0;
        assert_eq!(
            plan_layout(&fixed, 0, 0, 256 * MIB),
            Err(LayoutError::FixedLoadAddress)
        );
        assert_eq!(
            plan_layout(&good, 0, CMDLINE_MAX + 1, 256 * MIB),
            Err(LayoutError::CmdlineTooLong)
        );
        // Kernel workspace alone exceeds RAM.
        let big = image(MIB as usize, 400 * MIB as u32, 0x7FFF_FFFF);
        assert_eq!(
            plan_layout(&big, 0, 0, 256 * MIB),
            Err(LayoutError::RamTooSmall)
        );
        // Initrd would overlap the kernel workspace.
        assert_eq!(
            plan_layout(&good, 250 * MIB, 0, 256 * MIB),
            Err(LayoutError::RamTooSmall)
        );
    }

    #[test]
    fn a_non_relocatable_kernel_linked_for_16mib_is_accepted() {
        let mut img = image(MIB as usize, 8 * MIB as u32, 0x7FFF_FFFF);
        img[HDR_RELOCATABLE] = 0;
        img[HDR_PREF_ADDRESS..HDR_PREF_ADDRESS + 8].copy_from_slice(&KERNEL_LOAD_GPA.to_le_bytes());
        assert!(plan_layout(&img, 0, 0, 256 * MIB).is_ok());
    }

    #[test]
    fn e820_covers_ram_with_a_reserved_legacy_hole() {
        let m = e820_map(256 * MIB);
        assert_eq!(m[0], (0, 0x9F000, E820_RAM));
        assert_eq!(m[1], (0x9F000, 0x61000, E820_RESERVED));
        assert_eq!(m[2], (0x10_0000, 256 * MIB - 0x10_0000, E820_RAM));
        // Contiguous, no gaps or overlaps.
        assert_eq!(m[0].0 + m[0].1, m[1].0);
        assert_eq!(m[1].0 + m[1].1, m[2].0);
        // The boot structures live in the low usable range.
        assert!(CMDLINE_GPA + (CMDLINE_MAX as u64) < LOW_MEM_END);
        assert!(BOOT_PARAMS_GPA + ZERO_PAGE_BYTES as u64 <= CMDLINE_GPA);
        assert!(PT_GPA + PT_BYTES as u64 <= STACK_GPA);
    }

    #[test]
    fn gdt_has_flat_64bit_code_and_data() {
        let g = gdt_bytes();
        assert_eq!(&g[..8], &[0; 8]);
        assert_eq!(
            u64::from_le_bytes(g[8..16].try_into().unwrap()),
            0x00AF_9B00_0000_FFFF
        );
        assert_eq!(
            u64::from_le_bytes(g[16..24].try_into().unwrap()),
            0x00CF_9300_0000_FFFF
        );
    }

    const TEST_HZ: u64 = 3_000_000_000;

    fn tx_to(out: &mut Vec<u8>) -> impl FnMut(u8) + '_ {
        |b| out.push(b)
    }

    #[test]
    fn uart_transmits_and_reports_ready() {
        let mut u = Uart16550::new();
        let mut out = Vec::new();
        for &b in b"hi" {
            u.write(0, b, &mut tx_to(&mut out));
        }
        assert_eq!(out, b"hi");
        assert_eq!(u.read(5), 0x60, "THRE|TEMT");
    }

    #[test]
    fn uart_scratch_register_and_divisor_latch() {
        let mut u = Uart16550::new();
        let mut out = Vec::new();
        u.write(7, 0xA5, &mut tx_to(&mut out));
        assert_eq!(u.read(7), 0xA5);
        u.write(3, 0x80, &mut tx_to(&mut out)); // DLAB
        u.write(0, 0x01, &mut tx_to(&mut out));
        u.write(1, 0x00, &mut tx_to(&mut out));
        assert_eq!((u.read(0), u.read(1)), (1, 0));
        u.write(3, 0x03, &mut tx_to(&mut out)); // 8N1, DLAB off
        assert!(out.is_empty(), "divisor writes do not transmit");
    }

    #[test]
    fn uart_fifo_detect_reports_16550a() {
        let mut u = Uart16550::new();
        let mut out = Vec::new();
        assert_eq!(u.read(2) & 0xC0, 0, "no FIFO before FCR");
        u.write(2, 0x01, &mut tx_to(&mut out));
        assert_eq!(u.read(2) & 0xC0, 0xC0, "FIFOs enabled -> 16550A");
    }

    #[test]
    fn uart_loopback_self_test_matches_linux_expectation() {
        // Linux's autoconfig: MCR = LOOP|OUT2|RTS (0x1A) -> MSR & 0xF0 == 0x90.
        let mut u = Uart16550::new();
        let mut out = Vec::new();
        u.write(4, 0x1A, &mut tx_to(&mut out));
        assert_eq!(u.read(6) & 0xF0, 0x90);
        // A byte written in loopback comes back on the receive side.
        u.write(0, 0x5A, &mut tx_to(&mut out));
        assert!(out.is_empty(), "loopback does not transmit");
        assert_eq!(u.read(5) & 1, 1, "data ready");
        assert_eq!(u.read(0), 0x5A);
        assert_eq!(u.read(5) & 1, 0, "reading clears data-ready");
        // Out of loopback the modem lines read DCD|DSR|CTS.
        u.write(4, 0x03, &mut tx_to(&mut out));
        assert_eq!(u.read(6), 0xB0);
    }

    #[test]
    fn uart_thre_interrupt_pends_after_enable_and_clears_on_iir_read() {
        let mut u = Uart16550::new();
        let mut out = Vec::new();
        u.write(4, 0x08, &mut tx_to(&mut out)); // OUT2 opens the IRQ gate
        assert!(!u.irq_line());
        u.write(1, 0x02, &mut tx_to(&mut out)); // enable THRE interrupt
        assert!(u.irq_line());
        assert_eq!(u.read(2) & 0x0F, 0x02, "IIR names THRE");
        assert!(!u.irq_line(), "reading IIR acknowledges it");
        u.write(0, b'x', &mut tx_to(&mut out));
        assert!(u.irq_line(), "a transmit re-arms it");
    }

    #[test]
    fn uart_irq_needs_the_out2_gate() {
        let mut u = Uart16550::new();
        let mut out = Vec::new();
        u.write(1, 0x02, &mut tx_to(&mut out));
        assert!(!u.irq_line(), "OUT2 closed");
    }

    fn lines(bytes: &[u8]) -> Vec<String> {
        let mut lb = LineBuf::new();
        let mut got = Vec::new();
        for &b in bytes {
            lb.push(b, &mut |l: &str| got.push(String::from(l)));
        }
        lb.flush(&mut |l: &str| got.push(String::from(l)));
        got
    }

    #[test]
    fn linebuf_splits_on_newline_and_drops_cr() {
        assert_eq!(lines(b"one\r\ntwo\nthree"), ["one", "two", "three"]);
    }

    #[test]
    fn linebuf_masks_control_bytes_and_splits_long_lines() {
        assert_eq!(lines(b"a\x01b\n"), ["a.b"]);
        let long = vec![b'x'; LineBuf::CAP + 5];
        let got = lines(&long);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].len(), LineBuf::CAP);
        assert_eq!(got[1].len(), 5);
    }

    fn host() -> CpuidRegs {
        CpuidRegs {
            eax: 0x00A6_0F12,
            ebx: 0x0120_0800 | (32 << 16),
            ecx: u32::MAX,
            edx: u32::MAX,
        }
    }

    #[test]
    fn cpuid_leaf1_hides_apic_topology_and_hypervisor_bit() {
        let r = cpuid_policy(1, 0, host(), 0);
        assert_eq!(r.ebx >> 16, 0, "APIC id 0, one logical CPU");
        assert_eq!(r.ebx & 0xFF00, 0x0800, "CLFLUSH size kept");
        for bit in [9, 12, 7, 14, 28] {
            assert_eq!(r.edx & (1 << bit), 0, "edx bit {bit} must be hidden");
        }
        for bit in [21, 24, 3, 15, 31] {
            assert_eq!(r.ecx & (1 << bit), 0, "ecx bit {bit} must be hidden");
        }
        assert_ne!(r.edx & (1 << 25), 0, "SSE still advertised");
    }

    #[test]
    fn cpuid_osxsave_mirrors_the_guest_cr4_not_the_host() {
        let mut h = host();
        h.ecx |= 1 << 27;
        assert_eq!(cpuid_policy(1, 0, h, 0).ecx & (1 << 27), 0);
        assert_ne!(cpuid_policy(1, 0, h, CR4_OSXSAVE).ecx & (1 << 27), 0);
        h.ecx &= !(1 << 27);
        assert_ne!(cpuid_policy(1, 0, h, CR4_OSXSAVE).ecx & (1 << 27), 0);
    }

    #[test]
    fn cpuid_hides_host_topology_and_unsupported_leaves() {
        for leaf in [
            4,
            5,
            6,
            0xB,
            0x1F,
            0x8000_000A,
            0x8000_001D,
            0x8000_001E,
            0x8000_001F,
        ] {
            assert_eq!(
                cpuid_policy(leaf, 0, host(), 0),
                CpuidRegs::default(),
                "leaf {leaf:#x}"
            );
        }
        assert_eq!(cpuid_policy(0x8000_0008, 0, host(), 0).ecx, 0, "one core");
        let r = cpuid_policy(0x8000_0001, 0, host(), 0);
        assert_eq!(
            r.ecx & ((1 << 2) | (1 << 22) | (1 << 23)),
            0,
            "no SVM/TOPOEXT/perf"
        );
    }

    #[test]
    fn cpuid_hypervisor_range_is_zeroed() {
        assert_eq!(
            cpuid_policy(0x4000_0000, 0, host(), 0),
            CpuidRegs::default()
        );
    }

    #[test]
    fn cpuid_leaf7_hides_tsx_pku_and_speculation_control() {
        let r = cpuid_policy(7, 0, host(), 0);
        assert_eq!(
            r.ebx & ((1 << 1) | (1 << 4) | (1 << 11)),
            0,
            "TSC_ADJUST/HLE/RTM"
        );
        assert_eq!(
            r.ecx & ((1 << 3) | (1 << 4) | (1 << 7)),
            0,
            "PKU/OSPKE/CET_SS"
        );
        assert_eq!(r.edx >> 24, 0);
        assert_eq!(r.edx & (1 << 20), 0, "IBT");
        // Another subleaf is untouched.
        assert_eq!(cpuid_policy(7, 1, host(), 0), sanitize_cpuid(7, host()));
    }

    #[test]
    fn msr_policy_reads_known_values_and_faults_unknown() {
        let m = MsrState::new();
        assert_eq!(m.read(MSR_TSC), MsrRead::Tsc);
        assert_eq!(m.read(MSR_APIC_BASE), MsrRead::Value(0xFEE0_0100));
        assert_eq!(m.read(0xC001_0015), MsrRead::Value(0));
        assert_eq!(m.read(MSR_TSC_ADJUST), MsrRead::Value(0));
        assert_eq!(m.read(0xDEAD_BEEF), MsrRead::Gp);
    }

    #[test]
    fn msr_policy_shadows_permitted_writes() {
        let mut m = MsrState::new();
        assert_eq!(m.write(0xC001_0015, 0x1234), MsrWrite::Ok);
        assert_eq!(m.read(0xC001_0015), MsrRead::Value(0x1234));
        assert_eq!(m.write(0xC001_1029, 7), MsrWrite::Ok, "DE_CFG-class range");
        assert_eq!(m.read(0xC001_1029), MsrRead::Value(7));
        assert_eq!(m.write(0xDEAD_BEEF, 1), MsrWrite::Gp);
        assert_eq!(m.write(MSR_TSC, 99), MsrWrite::Tsc(99));
    }

    #[test]
    fn passthrough_msrs_never_overlap_the_emulated_set() {
        let m = MsrState::new();
        for &msr in MSR_PASSTHROUGH {
            assert_eq!(m.read(msr), MsrRead::Gp, "{msr:#x} is native, not emulated");
        }
    }

    #[test]
    fn io_bus_routes_com1_and_ignores_the_rest() {
        let mut bus = IoBus::new(TEST_HZ);
        let mut out = Vec::new();
        bus.write(0x3F8, 1, u32::from(b'A'), &mut tx_to(&mut out));
        assert_eq!(out, b"A");
        assert_eq!(bus.read(0x3FD, 1), 0x60);
        // Absent COM2: all ones, writes ignored.
        assert_eq!(bus.read(0x2F8, 1), 0xFF);
        assert_eq!(bus.write(0x2F8, 1, 5, &mut tx_to(&mut out)), IoEvent::None);
        assert_eq!(out, b"A");
        // Access size masks the "no device" value.
        assert_eq!(bus.read(0x1234, 2), 0xFFFF);
        assert_eq!(bus.read(0x1234, 4), u32::MAX);
    }

    #[test]
    fn io_bus_cmos_reports_a_valid_time_with_no_update_in_progress() {
        let mut bus = IoBus::new(TEST_HZ);
        let mut out = Vec::new();
        for (idx, want) in [
            (0x0A, 0x26),
            (0x0B, 0x02),
            (0x0D, 0x80),
            (0x09, 0x26),
            (0x32, 0x20),
        ] {
            bus.write(0x70, 1, idx, &mut tx_to(&mut out));
            assert_eq!(bus.read(0x71, 1), want, "cmos {idx:#x}");
        }
        // NMI-disable bit in the index does not change the register.
        bus.write(0x70, 1, 0x80 | 0x0A, &mut tx_to(&mut out));
        assert_eq!(bus.read(0x71, 1), 0x26);
        assert_eq!(bus.read(0x71, 1) & 0x80, 0, "UIP clear");
    }

    #[test]
    fn io_bus_pci_config_sees_an_empty_bus() {
        let mut bus = IoBus::new(TEST_HZ);
        let mut out = Vec::new();
        bus.write(0xCF8, 4, 0x8000_0000, &mut tx_to(&mut out));
        assert_eq!(
            bus.read(0xCF8, 4),
            0x8000_0000,
            "address register reads back"
        );
        assert_eq!(bus.read(0xCFC, 4), u32::MAX, "no device answers");
    }

    #[test]
    fn mmio_bus_reads_all_ones_and_logs_first_touches_only() {
        let mut m = MmioBus::new();
        assert_eq!(m.read(0xFED8_03C0, 4), 0xFFFF_FFFF);
        assert_eq!(m.read(0xFED8_03C0, 1), 0xFF);
        assert_eq!(m.read(0xFED8_03C0, 8), u64::MAX);
        assert!(m.note(0xFED8_03C0), "first touch is logged");
        assert!(!m.note(0xFED8_03C0), "repeats are not");
        assert!(m.note(0xFED8_0400), "a new address is");
        m.write(0xFED8_03C0, 4, 0x1234);
        assert_eq!(m.read(0xFED8_03C0, 4), 0xFFFF_FFFF, "writes are ignored");
    }

    #[test]
    fn mmio_log_goes_quiet_after_the_limit() {
        let mut m = MmioBus::new();
        let logged = (0..(MmioBus::LOG_LIMIT as u64 + 20))
            .filter(|&i| m.note(0xFE00_0000 + i * 8))
            .count();
        assert_eq!(logged, MmioBus::LOG_LIMIT);
    }

    /// Program the PICs like Linux (vectors 0x20/0x28) and unmask `irq`.
    fn pic_ready(bus: &mut IoBus, irq: u8) {
        let mut out = Vec::new();
        for (port, val) in [
            (0x20, 0x11),
            (0x21, 0x20),
            (0x21, 0x04),
            (0x21, 0x01),
            (0xA0, 0x11),
            (0xA1, 0x28),
            (0xA1, 0x02),
            (0xA1, 0x01),
        ] {
            bus.write(port, 1, val, &mut tx_to(&mut out));
        }
        bus.write(0x21, 1, u32::from(!(1u8 << irq)), &mut tx_to(&mut out));
    }

    #[test]
    fn io_bus_routes_the_pic_and_pit_ports() {
        let mut bus = IoBus::new(TEST_HZ);
        let mut out = Vec::new();
        bus.write(0x21, 1, 0xFF, &mut tx_to(&mut out));
        assert_eq!(bus.read(0x21, 1), 0xFF, "mask register");
        // PIT: mode 2, lo/hi, count 1193 (~1 ms), then latch + read.
        bus.set_time(0);
        bus.write(0x43, 1, 0x34, &mut tx_to(&mut out));
        bus.write(0x40, 1, 0xA9, &mut tx_to(&mut out));
        bus.write(0x40, 1, 0x04, &mut tx_to(&mut out));
        assert!(bus.next_deadline().is_some());
        bus.set_time(TEST_HZ / 4000); // 0.25 ms
        bus.write(0x43, 1, 0x00, &mut tx_to(&mut out)); // latch
        let lo = bus.read(0x40, 1);
        let hi = bus.read(0x40, 1);
        let count = (hi << 8) | lo;
        assert!((1193 - 298..=1193 - 296).contains(&count), "{count}");
        assert_eq!(bus.read(0x41, 1), 0, "channel 1 not modelled");
    }

    #[test]
    fn pit_tick_becomes_an_irq0_vector_through_the_pic() {
        let mut bus = IoBus::new(TEST_HZ);
        pic_ready(&mut bus, 0);
        let mut out = Vec::new();
        bus.set_time(0);
        bus.write(0x43, 1, 0x34, &mut tx_to(&mut out));
        bus.write(0x40, 1, 0xA9, &mut tx_to(&mut out));
        bus.write(0x40, 1, 0x04, &mut tx_to(&mut out));
        bus.set_time(TEST_HZ / 2000); // 0.5 ms: not yet
        bus.poll_devices();
        assert_eq!(bus.pending_vector(), None);
        bus.set_time(TEST_HZ / 500); // 2 ms: due
        bus.poll_devices();
        assert_eq!(bus.pending_vector(), Some(0x20));
        assert_eq!(bus.ack(), Some(0x20));
        bus.write(0x20, 1, 0x20, &mut tx_to(&mut out)); // EOI
        assert_eq!(bus.pending_vector(), None);
    }

    #[test]
    fn uart_thre_interrupt_reaches_the_guest_as_irq4() {
        let mut bus = IoBus::new(TEST_HZ);
        pic_ready(&mut bus, 4);
        let mut out = Vec::new();
        bus.write(0x3FC, 1, 0x08, &mut tx_to(&mut out)); // MCR OUT2
        bus.write(0x3F9, 1, 0x02, &mut tx_to(&mut out)); // IER: THRE interrupt
        bus.poll_devices();
        assert_eq!(bus.pending_vector(), Some(0x24));
        assert_eq!(bus.ack(), Some(0x24));
        // The handler reads IIR (clearing the cause) then EOIs.
        assert_eq!(bus.read(0x3FA, 1) & 0x0F, 0x02);
        bus.poll_devices();
        bus.write(0x20, 1, 0x20, &mut tx_to(&mut out));
        assert_eq!(
            bus.pending_vector(),
            None,
            "line dropped after the IIR read"
        );
        // Transmitting re-raises it.
        bus.write(0x3F8, 1, u32::from(b'x'), &mut tx_to(&mut out));
        bus.poll_devices();
        assert_eq!(bus.pending_vector(), Some(0x24));
    }

    #[test]
    fn io_bus_reset_ports_request_a_reset() {
        let mut bus = IoBus::new(TEST_HZ);
        let mut out = Vec::new();
        assert_eq!(
            bus.write(0x64, 1, 0xFE, &mut tx_to(&mut out)),
            IoEvent::Reset
        );
        assert_eq!(
            bus.write(0x64, 1, 0x20, &mut tx_to(&mut out)),
            IoEvent::None
        );
        assert_eq!(
            bus.write(0xCF9, 1, 0x06, &mut tx_to(&mut out)),
            IoEvent::Reset
        );
        assert_eq!(
            bus.write(0xCF9, 1, 0x00, &mut tx_to(&mut out)),
            IoEvent::None
        );
    }
}
