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

use crate::blockdev::BlockBackend;
use crate::nvme_dev::{NvmeDevice, NvmeIdentity};
use crate::pci_dev::{PciFunction, pci_target};
use crate::platform::{Platform, PlatformEvent};
use crate::svm::{CpuidRegs, sanitize_cpuid};
use crate::vdev::{PicPair, Pit8254};
use crate::virtio_blk::VirtioBlk;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
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

/// PCI device number of the virtual disk on bus 0.
pub const VIRTIO_BLK_SLOT: u8 = 3;
/// The legacy PIC line the virtual disk interrupts on.
pub const VIRTIO_BLK_IRQ: u8 = 10;
/// PCI device number of the virtual disk when it is an `NVMe` controller.
pub const NVME_SLOT: u8 = 4;
/// The legacy PIC line the `NVMe` controller interrupts on.
pub const NVME_IRQ: u8 = 11;

/// A platform device's MMIO window hit, with the offset within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformMmio {
    /// The local APIC.
    Lapic(u64),
    /// The I/O APIC.
    IoApic(u64),
}

/// A disk device's `BAR0` hit, with the offset within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskTarget {
    /// The virtio-blk device.
    Virtio(u64),
    /// The `NVMe` controller.
    Nvme(u64),
}

impl DiskTarget {
    /// The offset within the device's `BAR0`.
    #[must_use]
    pub const fn offset(self) -> u64 {
        match self {
            Self::Virtio(o) | Self::Nvme(o) => o,
        }
    }
}
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

/// The `EFER` MSR. Intercepted, never passed through: the hardware needs
/// `EFER.SVME` set in the guest state to `VMRUN` at all, so the guest's view of
/// the register and the real one differ by that bit.
pub const MSR_EFER: u32 = 0xC000_0080;
/// `EFER.SVME`: SVM enabled. Hidden from the guest, forced on in the VMCB.
pub const EFER_SVME: u64 = 1 << 12;
/// `EFER.LMA`: long mode active. Set by the processor, not by a `WRMSR`.
pub const EFER_LMA: u64 = 1 << 10;
/// The `EFER` bits a guest may write: `SCE`, `LME`, `NXE` and `FFXSR`.
pub const EFER_GUEST_WRITABLE: u64 = (1 << 0) | (1 << 8) | (1 << 11) | (1 << 14);

/// What a guest reads from `EFER` when the VMCB holds `vmcb_efer`: the real
/// value with `SVME` hidden, as on a machine that is not a host.
#[must_use]
pub const fn guest_efer_view(vmcb_efer: u64) -> u64 {
    vmcb_efer & !EFER_SVME
}

/// The VMCB's `EFER` after the guest writes `written`: its writable bits take
/// effect, `LMA` stays as the processor set it, and `SVME` stays on whatever the
/// guest wrote - without it the next `VMRUN` is refused.
#[must_use]
pub const fn vmcb_efer_after_write(vmcb_efer: u64, written: u64) -> u64 {
    (vmcb_efer & EFER_LMA) | EFER_SVME | (written & EFER_GUEST_WRITABLE)
}

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
    /// Return the guest's view of `EFER` (see [`guest_efer_view`]).
    Efer,
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
    /// The guest wrote `EFER`; apply it with [`vmcb_efer_after_write`].
    Efer(u64),
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

    /// An MSR state for a guest with a local APIC: `IA32_APIC_BASE` reports the
    /// APIC enabled at its architectural base, as the boot processor.
    #[must_use]
    pub fn with_apic() -> Self {
        let mut m = Self::default();
        m.shadow.insert(MSR_APIC_BASE, 0xFEE0_0900);
        m
    }

    /// What a guest `RDMSR` of `msr` returns.
    #[must_use]
    pub fn read(&self, msr: u32) -> MsrRead {
        if msr == MSR_TSC {
            return MsrRead::Tsc;
        }
        if msr == MSR_EFER {
            return MsrRead::Efer;
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
        if msr == MSR_EFER {
            // Reserved bits and the bits only the processor owns are not the
            // guest's to set.
            let settable = EFER_GUEST_WRITABLE | EFER_LMA | EFER_SVME;
            return if value & !settable == 0 {
                MsrWrite::Efer(value)
            } else {
                MsrWrite::Gp
            };
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
    /// The guest entered the ACPI soft-off state: power the machine off.
    PowerOff,
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
pub struct IoBus {
    /// COM1.
    pub uart: Uart16550,
    /// The master/slave interrupt controllers.
    pub pic: PicPair,
    /// The system timer (channel 0 + the speaker gate port).
    pub pit: Pit8254,
    /// The virtual disk on PCI bus 0, if the guest has one and it is a virtio
    /// device.
    pub virtio: Option<VirtioBlk>,
    /// The virtual disk, if the guest sees it as an `NVMe` controller.
    pub nvme: Option<Box<NvmeDevice>>,
    /// The PIC line each disk device currently drives (`[virtio, nvme]`), so a
    /// re-routed device releases the line it left.
    disk_irq: [Option<u8>; 2],
    /// The i440FX/PIIX platform a firmware guest expects (`None` for a guest
    /// booted directly into Linux, which is told about hardware by its zero
    /// page instead).
    platform: Option<Box<Platform>>,
    /// Bytes the firmware wrote to the QEMU debug port (`0x402`), until the run
    /// loop drains them. A DEBUG firmware build prints its log there.
    debug_bytes: Vec<u8>,
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
            virtio: None,
            nvme: None,
            disk_irq: [None; 2],
            platform: None,
            debug_bytes: Vec::new(),
            now: 0,
            cmos_index: 0,
            pci_addr: 0,
        }
    }

    /// Give the guest a virtual disk on PCI bus 0 (device [`VIRTIO_BLK_SLOT`],
    /// interrupt line [`VIRTIO_BLK_IRQ`]).
    pub fn attach_disk(&mut self, backend: Box<dyn BlockBackend>, read_only: bool) {
        self.virtio = Some(VirtioBlk::new(backend, VIRTIO_BLK_IRQ, read_only));
    }

    /// Give the guest a virtual disk that appears as an `NVMe` controller on PCI
    /// bus 0 (device [`NVME_SLOT`], interrupt line [`NVME_IRQ`]), presenting
    /// `identity`.
    pub fn attach_nvme(
        &mut self,
        backend: Box<dyn BlockBackend>,
        identity: NvmeIdentity,
        read_only: bool,
    ) {
        self.nvme = Some(Box::new(NvmeDevice::new(
            backend, identity, NVME_IRQ, read_only,
        )));
    }

    /// Which disk device's `BAR0` contains `gpa`, and the offset within it.
    #[must_use]
    pub fn disk_bar(&self, gpa: u64) -> Option<DiskTarget> {
        let inside = |bar: Option<(u64, u64)>| {
            let (base, len) = bar?;
            gpa.checked_sub(base).filter(|&o| o < len)
        };
        if let Some(off) = inside(self.virtio.as_ref().and_then(|v| v.pci.bar0())) {
            return Some(DiskTarget::Virtio(off));
        }
        inside(self.nvme.as_ref().and_then(|n| n.pci.bar0())).map(DiskTarget::Nvme)
    }

    /// A `size`-byte register read from a disk device.
    pub fn disk_read(&mut self, target: DiskTarget, size: u8) -> u64 {
        let value = match target {
            DiskTarget::Virtio(off) => self.virtio.as_mut().map_or(0, |v| v.mmio_read(off, size)),
            DiskTarget::Nvme(off) => self.nvme.as_ref().map_or(0, |n| n.mmio_read(off, size)),
        };
        // Reading the virtio ISR register clears the interrupt cause.
        self.refresh_disk_irqs();
        value
    }

    /// A `size`-byte register write to a disk device; the device may run its
    /// queues against guest memory `mem`.
    pub fn disk_write(
        &mut self,
        target: DiskTarget,
        size: u8,
        value: u64,
        mem: &dyn crate::virtio_blk::GuestMemory,
    ) {
        match target {
            DiskTarget::Virtio(off) => {
                if let Some(v) = self.virtio.as_mut() {
                    v.mmio_write(off, size, value, mem);
                }
            }
            DiskTarget::Nvme(off) => {
                if let Some(n) = self.nvme.as_mut() {
                    n.mmio_write(off, size, value, mem);
                }
            }
        }
        // A queue notification raises the line; an NVMe head doorbell lowers it.
        self.refresh_disk_irqs();
    }

    /// Give a platform guest its `fw_cfg` device.
    pub fn set_fwcfg(&mut self, fwcfg: crate::fwcfg::FwCfg) {
        if let Some(p) = self.platform.as_mut() {
            p.set_fwcfg(fwcfg);
        }
    }

    /// Give the guest the i440FX/PIIX platform firmware probes: host bridge,
    /// ISA bridge, ACPI PM timer and the CMOS RAM-size registers, for a machine
    /// with `ram_bytes` of RAM.
    pub fn enable_platform(&mut self, tsc_hz: u64, ram_bytes: u64) {
        self.platform = Some(Box::new(Platform::new(tsc_hz, ram_bytes)));
    }

    /// Hand every byte written to the debug port since the last call to `f`.
    pub fn drain_debug(&mut self, f: &mut impl FnMut(u8)) {
        for b in self.debug_bytes.drain(..) {
            f(b);
        }
    }

    /// Whether the firmware platform is present.
    #[must_use]
    pub const fn has_platform(&self) -> bool {
        self.platform.is_some()
    }

    /// The PCI function selected by the current `0xCF8` address, with the
    /// register offset, if one exists there.
    fn pci_function(&mut self) -> Option<(&mut PciFunction, usize)> {
        let (bus, dev, func, reg) = pci_target(self.pci_addr)?;
        if bus != 0 {
            return None;
        }
        if dev == VIRTIO_BLK_SLOT && func == 0 {
            return self.virtio.as_mut().map(|v| (&mut v.pci, reg));
        }
        if dev == NVME_SLOT && func == 0 {
            return self.nvme.as_mut().map(|n| (&mut n.pci, reg));
        }
        let platform = self.platform.as_mut()?;
        platform.function(dev, func).map(|f| (f, reg))
    }

    /// The `(device, function, register)` the current `0xCF8` address selects, if
    /// a function answers there.
    pub fn pci_present(&mut self) -> Option<(u8, u8, usize)> {
        let (_, dev, func, reg) = pci_target(self.pci_addr)?;
        self.pci_function().map(|(_, r)| (dev, func, r.max(reg)))
    }

    /// Tell the bus the current host time (TSC).
    pub const fn set_time(&mut self, now: u64) {
        self.now = now;
    }

    /// Let time-driven devices run: fire the PIT's IRQ 0 if due and mirror the
    /// UART's interrupt output onto IRQ 4.
    pub fn poll_devices(&mut self) {
        if let Some(p) = self.platform.as_mut() {
            p.lapic_mut().poll(self.now);
        }
        if self.pit.poll(self.now) {
            self.pic.set_irq(0, true);
            self.pic.set_irq(0, false);
            // On the I/O APIC the PIT's IRQ 0 arrives on pin 2.
            self.set_gsi(2, true);
            self.set_gsi(2, false);
        }
        let uart_level = self.uart.irq_line();
        self.pic.set_irq(4, uart_level);
        self.set_gsi(4, uart_level);
        self.refresh_disk_irqs();
    }

    /// Re-drive the disks' interrupt lines from the devices' current state.
    ///
    /// Called after every disk register access, not just at the next poll: a
    /// level-triggered line must fall the moment the guest clears its cause, or
    /// the I/O APIC delivers it again after the guest's end of interrupt and
    /// the guest sees an interrupt nobody raised.
    pub fn refresh_disk_irqs(&mut self) {
        let virtio_level = self.virtio.as_ref().is_some_and(VirtioBlk::irq_line);
        self.drive_disk_irq(0, VIRTIO_BLK_SLOT, VIRTIO_BLK_IRQ, virtio_level);
        let nvme_level = self.nvme.as_ref().is_some_and(|n| n.irq_line());
        self.drive_disk_irq(1, NVME_SLOT, NVME_IRQ, nvme_level);
    }

    /// Drive I/O APIC input `pin` (a platform guest's interrupt router).
    fn set_gsi(&mut self, pin: u8, level: bool) {
        if let Some(p) = self.platform.as_mut() {
            p.set_gsi(pin, level);
        }
    }

    /// Drive a disk device's interrupt level onto the PIC line it is routed to:
    /// the PIIX3 `PIRQ` routing firmware programmed when there is a platform,
    /// else the fixed `default` line. A device whose route changes releases the
    /// line it was on.
    fn drive_disk_irq(&mut self, which: usize, slot: u8, default: u8, level: bool) {
        let routed = self
            .platform
            .as_ref()
            .map_or(Some(default), |p| p.pirq_irq(slot, 1));
        if self.disk_irq[which] != routed
            && let Some(old) = self.disk_irq[which]
        {
            self.pic.set_irq(old, false);
        }
        self.disk_irq[which] = routed;
        if let Some(irq) = routed {
            self.pic.set_irq(irq, level);
        }
        // The same line on the I/O APIC: PCI INTx of slot `s` is pin 16 + (s & 3)
        // for INTA#, the routing the DSDT's _PRT describes.
        self.set_gsi(16 + (slot & 3), level);
    }

    /// The vector of the interrupt waiting to be delivered, if any: the local
    /// APIC's own (its timer) first, then the legacy PIC's, which reaches the CPU
    /// through the APIC's `LINT0` virtual wire once an APIC is on.
    #[must_use]
    pub fn pending_vector(&self) -> Option<u8> {
        if let Some(p) = &self.platform {
            if let Some(v) = p.lapic().pending_vector() {
                return Some(v);
            }
            if !p.lapic().extint_enabled() {
                return None;
            }
        }
        self.pic.pending_vector()
    }

    /// Acknowledge the next interrupt (INTA) and return its vector.
    pub fn ack(&mut self) -> Option<u8> {
        self.ack_source().map(|(vector, _)| vector)
    }

    /// Acknowledge the next interrupt and say where it came from: `true` for the
    /// local APIC (whose interrupts the guest's task priority masks), `false`
    /// for the legacy PIC (whose `ExtINT` interrupts ignore it).
    pub fn ack_source(&mut self) -> Option<(u8, bool)> {
        if let Some(p) = self.platform.as_mut() {
            if let Some(v) = p.lapic_mut().ack() {
                return Some((v, true));
            }
            if !p.lapic().extint_enabled() {
                return None;
            }
        }
        self.pic.ack().map(|v| (v, false))
    }

    /// The host TSC value at which a device next needs attention.
    #[must_use]
    pub fn next_deadline(&self) -> Option<u64> {
        let apic = self
            .platform
            .as_ref()
            .and_then(|p| p.lapic().next_deadline());
        match (self.pit.next_deadline(), apic) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// The offset within the local APIC window if `gpa` is inside it (and the
    /// guest has a platform).
    #[must_use]
    pub fn lapic_offset(&self, gpa: u64) -> Option<u64> {
        self.platform.as_ref()?;
        Platform::lapic_offset(gpa)
    }

    /// A `size`-byte local APIC register read at `off`.
    #[must_use]
    pub fn lapic_read(&self, off: u64, size: u8) -> u64 {
        self.platform.as_ref().map_or_else(
            || crate::insn::mask_to(u64::MAX, size),
            |p| p.lapic().read(off, size, self.now),
        )
    }

    /// A `size`-byte local APIC register write of `value` at `off`.
    pub fn lapic_write(&mut self, off: u64, size: u8, value: u64) {
        let now = self.now;
        if let Some(p) = self.platform.as_mut() {
            p.lapic_write(off, size, value, now);
        }
    }

    /// Which platform device's MMIO window contains `gpa` (a guest without the
    /// platform has none).
    #[must_use]
    pub fn platform_mmio(&self, gpa: u64) -> Option<PlatformMmio> {
        self.platform.as_ref()?;
        Platform::lapic_offset(gpa)
            .map(PlatformMmio::Lapic)
            .or_else(|| Platform::ioapic_offset(gpa).map(PlatformMmio::IoApic))
    }

    /// A `size`-byte register read from a platform device.
    #[must_use]
    pub fn platform_mmio_read(&self, target: PlatformMmio, size: u8) -> u64 {
        match target {
            PlatformMmio::Lapic(off) => self.lapic_read(off, size),
            PlatformMmio::IoApic(off) => self
                .platform
                .as_ref()
                .map_or(0, |p| p.ioapic_read(off, size)),
        }
    }

    /// A `size`-byte register write to a platform device.
    pub fn platform_mmio_write(&mut self, target: PlatformMmio, size: u8, value: u64) {
        match target {
            PlatformMmio::Lapic(off) => self.lapic_write(off, size, value),
            PlatformMmio::IoApic(off) => {
                if let Some(p) = self.platform.as_mut() {
                    p.ioapic_write(off, size, value);
                }
            }
        }
    }

    /// A fixed CMOS image: 2026-10-15 00:00:00, 24-hour BCD, no update in
    /// progress, battery good; a firmware guest also gets the RAM-size
    /// registers.
    fn cmos(&self, index: u8) -> u8 {
        if let Some(v) = self.platform.as_ref().and_then(|p| p.cmos(index)) {
            return v;
        }
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
        if let Some(p) = self.platform.as_mut()
            && p.claims(port)
        {
            return mask(p.io_read(port, size, self.now), size);
        }
        let v = match port {
            COM1_BASE..=COM1_END => u32::from(self.uart.read(port - COM1_BASE)),
            0x20 | 0x21 | 0xA0 | 0xA1 | 0x4D0 | 0x4D1 => {
                u32::from(self.pic.read(port).unwrap_or(0xFF))
            }
            0x40 => u32::from(self.pit.read_data(self.now)),
            // Channels 1 and 2 are not modelled; the control port is write-only.
            0x41 => 0,
            0x42 => u32::from(self.pit.read_data2(self.now)),
            0x61 => u32::from(self.pit.read_port61(self.now)),
            0x71 => u32::from(self.cmos(self.cmos_index)),
            // QEMU's debug port reads 0xE9 so firmware knows it can print there.
            0x402 if self.platform.is_some() => 0xE9,
            0xCF8 => self.pci_addr,
            0xCFC..=0xCFF => {
                let sub = usize::from(port - 0xCFC);
                match self.pci_function() {
                    Some((f, reg)) => f.read(reg + sub, size),
                    // No device there: an empty slot reads as all ones.
                    None => u32::MAX,
                }
            }
            // Every other port: an absent device reads as all ones.
            _ => u32::MAX,
        };
        mask(v, size)
    }

    /// Handle a guest `OUT` of `size` bytes to `port`; UART output goes to `tx`.
    pub fn write(&mut self, port: u16, size: u8, val: u32, tx: &mut impl FnMut(u8)) -> IoEvent {
        let val = mask(val, size);
        if let Some(p) = self.platform.as_mut()
            && p.claims(port)
        {
            return match p.io_write(port, size, val) {
                PlatformEvent::None => IoEvent::None,
                PlatformEvent::PowerOff => IoEvent::PowerOff,
            };
        }
        match port {
            COM1_BASE..=COM1_END => {
                self.uart.write(port - COM1_BASE, (val & 0xFF) as u8, tx);
            }
            0x20 | 0x21 | 0xA0 | 0xA1 | 0x4D0 | 0x4D1 => {
                self.pic.write(port, (val & 0xFF) as u8);
            }
            0x40 => self.pit.write_data((val & 0xFF) as u8, self.now),
            0x43 => self.pit.write_control((val & 0xFF) as u8, self.now),
            0x42 => self.pit.write_data2((val & 0xFF) as u8, self.now),
            0x61 => self.pit.write_port61((val & 0xFF) as u8, self.now),
            0x70 => self.cmos_index = (val & 0xFF) as u8,
            0x402 if self.platform.is_some() => self.debug_bytes.push((val & 0xFF) as u8),
            0xCF8 => self.pci_addr = val,
            0xCFC..=0xCFF => {
                let sub = usize::from(port - 0xCFC);
                if let Some((f, reg)) = self.pci_function() {
                    f.write(reg + sub, size, val);
                }
            }
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
/// Devices with state live elsewhere (the disk's BAR, the local APIC in the
/// platform); what is left reads as an absent device (all ones) and ignores
/// writes. The first touches of each distinct address are reported so that what
/// a guest probes can be seen and the devices it needs added deliberately.
#[derive(Debug, Clone, Default)]
pub struct MmioBus {
    seen: BTreeMap<u64, u32>,
    /// How many accesses to the disk's BAR have been logged so far.
    pub disk_logged: usize,
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
// Event trace
// ---------------------------------------------------------------------------

/// How many events the trace ring keeps.
pub const TRACE_DEPTH: usize = 192;

/// What a traced guest event was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceKind {
    /// `IN` from an I/O port.
    IoIn,
    /// `OUT` to an I/O port.
    IoOut,
    /// A load from an MMIO address.
    MmioRead,
    /// A store to an MMIO address.
    MmioWrite,
    /// `RDMSR`.
    Rdmsr,
    /// `WRMSR`.
    Wrmsr,
    /// `CPUID` (the address is the leaf).
    Cpuid,
}

/// One traced guest event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceEvent {
    /// What happened.
    pub kind: TraceKind,
    /// The port, MMIO address, MSR number or CPUID leaf.
    pub addr: u64,
    /// The value transferred.
    pub value: u64,
    /// The access width in bytes.
    pub size: u8,
    /// The guest instruction pointer at the access.
    pub rip: u64,
}

impl TraceEvent {
    const EMPTY: Self = Self {
        kind: TraceKind::IoIn,
        addr: 0,
        value: 0,
        size: 0,
        rip: 0,
    };
}

/// The most recent guest events, kept so that when a firmware guest stops the
/// last things it touched are known. Firmware that prints nothing leaves only
/// this to debug from.
#[derive(Debug, Clone)]
pub struct TraceRing {
    events: [TraceEvent; TRACE_DEPTH],
    next: usize,
    total: u64,
}

impl Default for TraceRing {
    fn default() -> Self {
        Self::new()
    }
}

impl TraceRing {
    /// An empty ring.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            events: [TraceEvent::EMPTY; TRACE_DEPTH],
            next: 0,
            total: 0,
        }
    }

    /// Record `event`, overwriting the oldest when full.
    pub const fn push(&mut self, event: TraceEvent) {
        self.events[self.next] = event;
        self.next = (self.next + 1) % TRACE_DEPTH;
        self.total += 1;
    }

    /// How many events were ever recorded (including overwritten ones).
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// The kept events, oldest first.
    pub fn recent(&self) -> impl Iterator<Item = TraceEvent> + '_ {
        let kept = usize::try_from(self.total).map_or(TRACE_DEPTH, |t| t.min(TRACE_DEPTH));
        let start = (self.next + TRACE_DEPTH - kept) % TRACE_DEPTH;
        (0..kept).map(move |i| self.events[(start + i) % TRACE_DEPTH])
    }
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
        BOOT_PARAMS_GPA, CMDLINE_GPA, DiskTarget, GDT_GPA, GUEST_RAM_BYTES, IoBus, IoEvent,
        LineBuf, MIB, MSR_PASSTHROUGH, MmioBus, MsrRead, MsrState, MsrWrite, PT_BYTES, PT_GPA,
        PlatformMmio, STACK_GPA, TraceEvent, TraceKind, TraceRing, cpuid_policy, e820_map,
        gdt_bytes, guest_efer_view, plan_layout, vmcb_efer_after_write,
    };
    use crate::blockdev::{BlockBackend, BlockError, DiskConfig, DiskMap, DiskModel, SECTOR};
    use crate::handoff::{BootHandoff, GuestFirmware, StagedFile};
    use crate::insn::{AccessKind, decode_mem_access, extend, mask_to, merge_reg, translate_gva};
    use crate::mmio::PhysMmio;
    use crate::nvme::NvmeController;
    use crate::nvme_dev::NvmeIdentity;
    use crate::platform::{RESET_CS_BASE, RESET_RIP, flash_layout};
    use crate::serial::SerialPort;
    use crate::svm::{GuestGprs, host_cpuid, vmrun};
    use crate::virtio_blk::GuestMemory;
    use alloc::alloc::{Layout as AllocLayout, alloc_zeroed};
    use alloc::boxed::Box;
    use alloc::collections::BTreeSet;
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::fmt::Write as _;
    use core::sync::atomic::Ordering;
    use enlil_hal::bzimage::build_boot_params;
    use enlil_hal::npt::{
        NptMapper, build_identity_npt_2mib, build_npt_2mib, set_npt_range_writable,
    };
    use enlil_hal::region::{IoPermissionsMap, MsrPermissionsMap, Vmcb};
    use enlil_hal::svm::{
        CPUID_INSN_LEN, IoioExitInfo, LongModeGuestSetup, MSR_INSN_LEN, MinimalGuestSetup,
        RunLoopExit, VMCB_SIZE, VmcbSegment, classify_run_loop_exit, control, enable_io_intercept,
        enable_msr_intercept, encode_event_inj, encode_vintr, event_type, exit_code, exit_info_1,
        exit_info_2, guest_rax, guest_rip, int_control, intercept2, msr_exit_is_write,
        msrpm_read_bit, next_rip, program_long_mode_hlt_guest, program_minimal_hlt_guest,
        read_save_u64, read_segment, resume_rip_after, save, set_event_inj, set_guest_rax,
        set_guest_rip, set_int_control, set_intr_intercept, set_tsc_offset, tsc_offset,
        write_save_u64, write_segment,
    };

    /// Command line used when the ESP has no `cmdline.txt`.
    const DEFAULT_CMDLINE: &str = "console=ttyS0,115200 earlycon=uart8250,io,0x3f8 \
         rdinit=/init acpi=off noapic nolapic pci=conf1 nokaslr panic=-1 loglevel=7";
    /// The host LAPIC-timer vector used to force a `#VMEXIT` when a device
    /// event (a PIT tick) is due while the guest is running.
    const TIMER_VECTOR: u8 = 0x40;
    /// `INT_CONTROL.V_IRQ`: a virtual interrupt is posted and not yet delivered.
    const V_IRQ: u64 = 1 << 8;
    /// `INT_CONTROL.V_IGN_TPR`: the virtual interrupt ignores the guest's task priority.
    const V_IGN_TPR: u64 = 1 << 20;
    /// `INT_CONTROL.V_INTR_MASKING`: left clear so the guest's own `IF` gates
    /// interrupts (the host runs with `IF = 0`).
    const V_INTR_MASKING: u64 = 1 << 24;
    /// Length of `HLT` (`F4`).
    const HLT_INSN_LEN: u64 = 1;
    /// Wall-clock cap on one run so a wedged guest cannot hang the boot.
    const RUN_SECONDS: u64 = 120;
    /// How many distinct unknown MSRs are logged before going quiet.
    const MSR_LOG_LIMIT: usize = 40;
    /// How many first-touch discovery lines a firmware guest logs per kind.
    const DISCOVER_LIMIT: usize = 90;
    /// Size of the firmware guest's nested page tables (RAM + both flash images).
    const FW_NPT_BYTES: usize = 16 * 4096;
    /// `CR0.PG`.
    const CR0_PG: u64 = 1 << 31;

    /// Why the guest stopped running.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Stop {
        /// The guest triple-faulted (`SHUTDOWN`).
        Shutdown,
        /// The guest asked the machine to reset.
        Reset,
        /// The guest entered the ACPI soft-off state.
        PowerOff,
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
        base: *mut u8,
        len: usize,
        /// The firmware flash images, readable for instruction fetch (a
        /// `len == 0` span is unused).
        flash: [FlashSpan; 2],
    }

    /// A staged flash image as the host sees it, and where the guest does.
    #[derive(Clone, Copy)]
    struct FlashSpan {
        gpa: u64,
        host: *const u8,
        len: usize,
    }

    impl FlashSpan {
        const NONE: Self = Self {
            gpa: 0,
            host: core::ptr::null(),
            len: 0,
        };
    }

    impl GuestMem {
        /// The in-bounds start index of `len` bytes at guest-physical `gpa`.
        fn span(self, gpa: u64, len: usize) -> Option<usize> {
            let start = usize::try_from(gpa).ok()?;
            (start.checked_add(len)? <= self.len).then_some(start)
        }
    }

    /// DMA access for the virtual devices. The guest is stopped (this runs on
    /// the host between `VMRUN`s on a single CPU), so plain copies are sound.
    impl GuestMemory for GuestMem {
        fn read(&self, gpa: u64, buf: &mut [u8]) -> bool {
            let Some(i) = self.span(gpa, buf.len()) else {
                return false;
            };
            // SAFETY: `span` proved `[i, i + len)` lies inside the live,
            // leaked guest-RAM window; `buf` is a disjoint host buffer.
            unsafe {
                core::ptr::copy_nonoverlapping(self.base.add(i), buf.as_mut_ptr(), buf.len());
            }
            true
        }

        fn write(&self, gpa: u64, data: &[u8]) -> bool {
            let Some(i) = self.span(gpa, data.len()) else {
                return false;
            };
            // SAFETY: as in `read`; `base` was derived from the mutable window.
            unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), self.base.add(i), data.len()) };
            true
        }
    }

    impl GuestMem {
        fn read_u8(self, gpa: u64) -> Option<u8> {
            if let Some(i) = usize::try_from(gpa).ok().filter(|&i| i < self.len) {
                // SAFETY: `i` is in bounds of the live, leaked guest-RAM window.
                return Some(unsafe { self.base.add(i).cast_const().read_volatile() });
            }
            self.flash.iter().find_map(|f| {
                let off = usize::try_from(gpa.checked_sub(f.gpa)?)
                    .ok()
                    .filter(|&o| o < f.len)?;
                // SAFETY: `off` is inside the staged image, which is live
                // `LOADER_DATA` the kernel never frees.
                Some(unsafe { f.host.add(off).read_volatile() })
            })
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

    /// Fetch up to 15 instruction bytes at guest-virtual `rip`. With paging on,
    /// each page is translated through the guest's own page tables; with it off
    /// (real mode, or firmware's flat protected mode) the address is `CS.base +
    /// RIP`. Returns the bytes and how many were readable.
    fn fetch_insn(mem: GuestMem, vmcb: &[u8], rip: u64) -> ([u8; 15], usize) {
        let mut buf = [0u8; 15];
        let paging = read_save_u64(vmcb, save::CR0) & CR0_PG != 0;
        let cr3 = read_save_u64(vmcb, save::CR3);
        let cs_base = read_segment(vmcb, save::CS).base;
        let read_entry = |gpa: u64| mem.read_u64(gpa);
        let mut n = 0;
        let mut page_gpa: Option<u64> = None;
        while n < buf.len() {
            let gva = rip.wrapping_add(n as u64);
            let byte = if paging {
                if n == 0 || gva.trailing_zeros() >= 12 {
                    page_gpa = translate_gva(cr3, gva & !0xFFF, &read_entry);
                }
                let Some(page) = page_gpa else { break };
                mem.read_u8(page + (gva & 0xFFF))
            } else {
                mem.read_u8(cs_base.wrapping_add(gva) & 0xFFFF_FFFF)
            };
            let Some(byte) = byte else { break };
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
        label: &str,
        mem: GuestMem,
        vmcb: &mut [u8],
        gprs: &mut GuestGprs,
        bus: &mut MmioBus,
        io: &mut IoBus,
        (trace, irq_trace): (&mut TraceRing, &mut TraceRing),
        gpa: u64,
    ) -> Result<(), Stop> {
        let rip = guest_rip(vmcb);
        let (bytes, n) = fetch_insn(mem, vmcb, rip);
        let Some(access) = decode_mem_access(&bytes[..n]) else {
            serial.write_str(&format!(
                "enlil kernel: {label}: undecodable MMIO instruction at RIP {rip:#x} (gpa {gpa:#x}): {:02x?}\n",
                &bytes[..n]
            ));
            return Err(Stop::MmioDecode(gpa));
        };
        let size = access.size;
        // Is the address inside a virtual disk's BAR (a real device)?
        let disk = io.disk_bar(gpa);
        let plat = io.platform_mmio(gpa);
        let first = disk.is_none() && bus.note(gpa);
        match access.kind {
            AccessKind::Load {
                reg,
                high8,
                sign_extend,
                dest_size,
            } => {
                let raw = match (disk, plat) {
                    (Some(target), _) => io.disk_read(target, size),
                    (None, Some(target)) => io.platform_mmio_read(target, size),
                    (None, None) => bus.read(gpa, size),
                };
                let value = extend(raw, size, dest_size, sign_extend);
                let event = TraceEvent {
                    kind: TraceKind::MmioRead,
                    addr: gpa,
                    value: raw,
                    size,
                    rip,
                };
                trace.push(event);
                if plat.is_some() || disk.is_some() {
                    irq_trace.push(event);
                }
                if let Some(target) = disk
                    && io.has_platform()
                    && bus.disk_logged < 60
                {
                    bus.disk_logged += 1;
                    serial.write_str(&format!(
                        "enlil kernel: {label}: disk bar read  {size}B +{:#x} -> {raw:#x} from RIP {rip:#x}\n",
                        target.offset()
                    ));
                }
                let (target, hi) = if high8 { (reg - 4, true) } else { (reg, false) };
                let merged = merge_reg(get_reg(vmcb, gprs, target), value, dest_size, hi);
                set_reg(vmcb, gprs, target, merged);
                if first {
                    let what = match plat {
                        Some(PlatformMmio::Lapic(_)) => "local APIC",
                        Some(PlatformMmio::IoApic(_)) => "I/O APIC",
                        None => "absent device",
                    };
                    serial.write_str(&format!(
                        "enlil kernel: {label}: MMIO read  {size}B at {gpa:#x} -> {value:#x} ({what}) from RIP {rip:#x}\n"
                    ));
                }
            }
            AccessKind::Store { reg, high8 } => {
                let (source, shift) = if high8 { (reg - 4, 8) } else { (reg, 0) };
                let value = mask_to(get_reg(vmcb, gprs, source) >> shift, size);
                let event = TraceEvent {
                    kind: TraceKind::MmioWrite,
                    addr: gpa,
                    value,
                    size,
                    rip,
                };
                trace.push(event);
                if plat.is_some() || disk.is_some() {
                    irq_trace.push(event);
                }
                mmio_store(
                    serial, label, mem, bus, io, disk, plat, gpa, size, value, rip, first,
                );
            }
            AccessKind::StoreImm { value } => {
                let value = mask_to(value, size);
                let event = TraceEvent {
                    kind: TraceKind::MmioWrite,
                    addr: gpa,
                    value,
                    size,
                    rip,
                };
                trace.push(event);
                if plat.is_some() || disk.is_some() {
                    irq_trace.push(event);
                }
                mmio_store(
                    serial, label, mem, bus, io, disk, plat, gpa, size, value, rip, first,
                );
            }
        }
        set_guest_rip(vmcb, rip.wrapping_add(access.len as u64));
        Ok(())
    }

    /// Perform an MMIO store: to the virtual disk if `disk` is set (a
    /// doorbell write runs the queue against guest memory), else to the
    /// absent-device bus.
    #[allow(clippy::too_many_arguments)]
    fn mmio_store(
        serial: &SerialPort,
        label: &str,
        mem: GuestMem,
        bus: &mut MmioBus,
        io: &mut IoBus,
        disk: Option<DiskTarget>,
        plat: Option<PlatformMmio>,
        gpa: u64,
        size: u8,
        value: u64,
        rip: u64,
        first: bool,
    ) {
        if let Some(target) = disk {
            if io.has_platform() && bus.disk_logged < 60 {
                bus.disk_logged += 1;
                serial.write_str(&format!(
                    "enlil kernel: {label}: disk bar write {size}B +{:#x} <- {value:#x} from RIP {rip:#x}\n",
                    target.offset()
                ));
            }
            io.disk_write(target, size, value, &mem);
        } else if let Some(target) = plat {
            io.platform_mmio_write(target, size, value);
        } else {
            bus.write(gpa, size, value);
            if first {
                serial.write_str(&format!(
                    "enlil kernel: {label}: MMIO write {size}B at {gpa:#x} = {value:#x} (ignored) from RIP {rip:#x}\n"
                ));
            }
        }
    }

    /// The host `NVMe` namespace backing a guest's virtual disk, in chunks the
    /// driver's staging buffer can carry.
    struct NvmeBackend {
        ctl: NvmeController<PhysMmio>,
        /// How the drive identifies itself: copied to the guest's controller so
        /// the guest believes it owns this drive.
        identity: NvmeIdentity,
    }

    /// An identify-structure ASCII field without its space/NUL padding.
    fn trimmed(field: &[u8]) -> Vec<u8> {
        let end = field
            .iter()
            .rposition(|&b| b != b' ' && b != 0)
            .map_or(0, |i| i + 1);
        field[..end].to_vec()
    }

    /// Bytes moved per `NVMe` command.
    const NVME_CHUNK: usize = 256 * 1024;

    impl BlockBackend for NvmeBackend {
        fn sectors(&self) -> u64 {
            self.ctl.namespace_blocks()
        }

        fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            if !buf.len().is_multiple_of(SECTOR) {
                return Err(BlockError::Unaligned);
            }
            let mut cur = lba;
            for chunk in buf.chunks_mut(NVME_CHUNK) {
                self.ctl
                    .read_blocks(cur, chunk)
                    .map_err(|_| BlockError::Io)?;
                cur += (chunk.len() / SECTOR) as u64;
            }
            Ok(())
        }

        fn write(&mut self, lba: u64, data: &[u8]) -> Result<(), BlockError> {
            if !data.len().is_multiple_of(SECTOR) {
                return Err(BlockError::Unaligned);
            }
            let mut cur = lba;
            for chunk in data.chunks(NVME_CHUNK) {
                self.ctl
                    .write_blocks(cur, chunk)
                    .map_err(|_| BlockError::Io)?;
                cur += (chunk.len() / SECTOR) as u64;
            }
            Ok(())
        }

        fn flush(&mut self) -> Result<(), BlockError> {
            self.ctl.flush().map_err(|_| BlockError::Io)
        }
    }

    /// Find the host `NVMe` disk whose serial number is `wanted`.
    ///
    /// Controllers are matched by *serial number*, never by position: handing a
    /// guest the first disk the scan happens to find could expose the machine's
    /// own system disk. Each candidate is reset and queried (no writes); those
    /// that do not match are shut down again.
    fn open_disk(serial: &SerialPort, label: &str, wanted: &str) -> Option<NvmeBackend> {
        let (count, controllers) = crate::pci::find_storage_controllers();
        for c in controllers.iter().take(count) {
            if !c.is_nvme || c.bar0_base == 0 {
                continue;
            }
            crate::pci::enable_mem_bus_master(c.bus, c.device, c.function);
            // SAFETY: runs on the kernel's own page tables, single CPU.
            if !unsafe { crate::paging::map_mmio_range(c.bar0_base, 0x4000) } {
                continue;
            }
            match NvmeController::init(PhysMmio::new(c.bar0_base), 1) {
                Ok(mut ctl) => {
                    let matched = ctl.serial_is(wanted);
                    serial.write_str(&format!(
                        "enlil kernel: {label}: disk: NVMe at {}:{}.{}, {} - {}\n",
                        c.bus,
                        c.device,
                        c.function,
                        core::str::from_utf8(ctl.model()).unwrap_or("?").trim(),
                        if matched {
                            "selected"
                        } else {
                            "not assigned, shut down"
                        }
                    ));
                    if matched {
                        // PCI vendor and device id of the real controller.
                        let ids = crate::pci::config_dword(c.bus, c.device, c.function, 0);
                        let identity = NvmeIdentity {
                            vendor: (ids & 0xFFFF) as u16,
                            device: (ids >> 16) as u16,
                            serial: trimmed(ctl.serial()),
                            model: trimmed(ctl.model()),
                            firmware: trimmed(ctl.firmware()),
                        };
                        return Some(NvmeBackend { ctl, identity });
                    }
                    ctl.disable();
                }
                Err(e) => serial.write_str(&format!(
                    "enlil kernel: {label}: disk: NVMe at {}:{}.{} did not initialize: {e}\n",
                    c.bus, c.device, c.function
                )),
            }
        }
        None
    }

    /// Attach the guest's virtual disk if the ESP assigned one: parse
    /// `disk.txt`, find the named host disk and wrap it in the allow-list.
    fn attach_disk(serial: &SerialPort, label: &str, file: StagedFile, io: &mut IoBus) {
        // SAFETY: the extent comes from the UEFI stage's handoff.
        let bytes = unsafe { staged(file) };
        if bytes.is_empty() {
            serial.write_str(&format!(
                "enlil kernel: {label}: no disk assignment (disk.txt) - the guest gets no disk\n"
            ));
            return;
        }
        let cfg = match core::str::from_utf8(bytes)
            .map_err(|_| String::from("not UTF-8"))
            .and_then(|t| DiskConfig::parse(t).map_err(|e| format!("{e:?}")))
        {
            Ok(c) => c,
            Err(e) => {
                serial.write_str(&format!(
                    "enlil kernel: {label}: bad disk.txt ({e}) - no disk\n"
                ));
                return;
            }
        };
        let Some(backend) = open_disk(serial, label, &cfg.serial) else {
            serial.write_str(&format!(
                "enlil kernel: {label}: no NVMe disk matches the assigned serial - no disk\n"
            ));
            return;
        };
        let sectors = backend.sectors();
        let ranges = cfg.clamped(sectors);
        let read_only = ranges.iter().all(|r| !r.writable);
        serial.write_str(&format!(
            "enlil kernel: {label}: virtual disk: {sectors} sectors, {} allowed range(s), {}\n",
            ranges.len(),
            if read_only {
                "read-only"
            } else {
                "partly writable"
            }
        ));
        let identity = backend.identity.clone();
        let disk = Box::new(DiskMap::new(backend, ranges));
        match cfg.model {
            DiskModel::Virtio => io.attach_disk(disk, read_only),
            DiskModel::Nvme => {
                serial.write_str(&format!(
                    "enlil kernel: {label}: the guest's disk is an NVMe controller presenting {} ({})\n",
                    String::from_utf8_lossy(&identity.model),
                    String::from_utf8_lossy(&identity.serial)
                ));
                io.attach_nvme(disk, identity, read_only);
            }
        }
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

    /// Boot the staged guest and run it until it stops, logging every console
    /// line it prints as `[linux] ...` (or `[ovmf] ...`). A staged Linux kernel is
    /// booted directly by the Linux boot protocol; otherwise a guest firmware
    /// image (OVMF) the config asks to boot is run from the x86 reset vector.
    /// Does nothing when neither was staged or SVM is not up.
    pub fn boot(serial: &SerialPort, handoff: &BootHandoff) {
        let firmware = handoff.firmware_boot_guest();
        if !handoff.linux_guest.has_kernel() && firmware.is_none() {
            return;
        }
        if !crate::kernel::SVM_READY.load(Ordering::Acquire) {
            serial.write_str("enlil kernel: linux: SVM is not available - cannot run a guest\n");
            return;
        }
        if handoff.linux_guest.has_kernel() {
            boot_linux_direct(serial, handoff);
        } else if let Some(entry) = firmware {
            boot_firmware(serial, handoff, entry);
        }
    }

    /// Everything [`launch`] needs to run a prepared machine.
    struct Launch {
        /// Tag on console lines relayed from this guest.
        label: &'static str,
        /// The programmed VMCB (CPU state, nested-paging root).
        vmcb: Vmcb,
        /// The I/O permission map (every port intercepted).
        iopm: IoPermissionsMap,
        /// The MSR permission map (unarmed; `launch` arms it).
        msrpm: MsrPermissionsMap,
        /// System-physical base of the guest's RAM window.
        ram_spa: u64,
        /// Initial general-purpose registers.
        gprs: GuestGprs,
        /// The staged virtual-disk policy, if the ESP assigned a disk.
        disk: StagedFile,
        /// The "entering the guest" line.
        banner: String,
        /// Give the guest the i440FX/PIIX platform (a firmware guest).
        platform: bool,
        /// The firmware flash images, for instruction fetch from ROM.
        flash: [FlashSpan; 2],
        /// The `fw_cfg` device a firmware guest reads its tables from.
        fwcfg: Option<crate::fwcfg::FwCfg>,
    }

    /// Boot a Linux `bzImage` directly by the Linux boot protocol (64-bit entry,
    /// zero page, identity-mapped RAM): the quickest way to a real OS guest.
    fn boot_linux_direct(serial: &SerialPort, handoff: &BootHandoff) {
        let payload = handoff.linux_guest;
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
        let (Ok(mut vmcb), Ok(iopm), Ok(msrpm)) = (
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
        write_segment(
            vmcb.as_bytes_mut(),
            save::GDTR,
            VmcbSegment {
                selector: 0,
                attrib: 0,
                limit: 0x17,
                base: GDT_GPA,
            },
        );
        launch(
            serial,
            Launch {
                label: "linux",
                vmcb,
                iopm,
                msrpm,
                ram_spa,
                gprs: GuestGprs {
                    rsi: BOOT_PARAMS_GPA,
                    ..GuestGprs::default()
                },
                disk: payload.disk,
                banner: format!(
                    "entering the guest at GPA {:#x} (RAM {} MiB at {:#x}, cmdline: {})",
                    layout.entry_gpa,
                    GUEST_RAM_BYTES / MIB,
                    ram_spa,
                    cmdline
                ),
                platform: false,
                flash: [FlashSpan::NONE; 2],
                fwcfg: None,
            },
        );
    }

    /// Boot a guest from its own UEFI firmware (OVMF): RAM at zero, the variable
    /// store and read-only code flash at the top of the 4 GiB space, and the CPU
    /// in its reset state, executing the reset vector in the last bytes of the
    /// code image - exactly what a real machine's firmware expects.
    fn boot_firmware(serial: &SerialPort, handoff: &BootHandoff, fw: GuestFirmware) {
        let id = fw.id_str();
        let say = |msg: &str| serial.write_str(&format!("enlil kernel: ovmf: {id}: {msg}\n"));
        // SAFETY: the UEFI stage staged both images into `LOADER_DATA` pages
        // (never freed); the kernel runs identity-mapped.
        let (code, vars) = unsafe {
            (
                staged(StagedFile {
                    base: fw.code_base,
                    len: fw.code_len,
                }),
                staged(StagedFile {
                    base: fw.vars_base,
                    len: fw.vars_len,
                }),
            )
        };
        let Some(flash) = flash_layout(code.len() as u64, vars.len() as u64, GUEST_RAM_BYTES)
        else {
            say("the firmware images are not page-sized (or too large) - cannot map them");
            return;
        };
        let Some(ram_spa) = crate::kernel::guest_ram::LINUX.take(GUEST_RAM_BYTES, 2 * MIB) else {
            say("no planned guest-RAM window for the guest");
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

        // Nested page tables: RAM, the variable store (read/write), and the code
        // image (read-only: a store to ROM must not change it).
        // SAFETY: nonzero, 4 KiB-aligned layout; the buffer is leaked below.
        let npt_raw = unsafe {
            alloc_zeroed(
                AllocLayout::from_size_align(FW_NPT_BYTES, 4096)
                    .unwrap_or(AllocLayout::new::<u8>()),
            )
        };
        if npt_raw.is_null() {
            say("no memory for the nested page tables");
            return;
        }
        let npt_pa = npt_raw as u64;
        // SAFETY: `npt_raw` owns FW_NPT_BYTES zeroed bytes, leaked for the VM's life.
        let npt_buf = unsafe { core::slice::from_raw_parts_mut(npt_raw, FW_NPT_BYTES) };
        // The flash is mapped a page at a time so every leaf is 4 KiB: the code
        // must be write-protectable per page whatever its alignment in memory.
        let map_pages = |m: &mut NptMapper<'_>, gpa: u64, spa: u64, len: u64| {
            (0..len / 4096).try_for_each(|i| m.map_range(gpa + i * 4096, spa + i * 4096, 4096, &[]))
        };
        let mapped = NptMapper::new(&mut *npt_buf, npt_pa).and_then(|mut m| {
            m.map_range(0, ram_spa, GUEST_RAM_BYTES, &[])?;
            map_pages(&mut m, flash.vars_gpa, fw.vars_base, flash.vars_len)?;
            map_pages(&mut m, flash.code_gpa, fw.code_base, flash.code_len)?;
            Ok(m.ncr3())
        });
        let Ok(ncr3) = mapped else {
            say("nested page tables did not fit");
            return;
        };
        if set_npt_range_writable(npt_buf, npt_pa, flash.code_gpa, flash.code_len, false).is_err() {
            say("could not write-protect the firmware code");
            return;
        }

        // The CPU in its architectural reset state: real mode, CS = 0xF000 with
        // base 0xFFFF0000, RIP = 0xFFF0 - the reset vector, 16 bytes below 4 GiB.
        let (Ok(mut vmcb), Ok(iopm), Ok(msrpm)) = (
            Vmcb::new(),
            IoPermissionsMap::intercept_all(),
            MsrPermissionsMap::new(),
        ) else {
            say("no memory for the VMCB / permission maps");
            return;
        };
        let setup = MinimalGuestSetup {
            asid: 1,
            nested_cr3: ncr3,
            entry_ip: RESET_RIP,
            code_base: RESET_CS_BASE,
            stack_pointer: 0,
        };
        if program_minimal_hlt_guest(vmcb.as_bytes_mut(), &setup).is_err() {
            say("VMCB programming failed");
            return;
        }
        let v = vmcb.as_bytes_mut();
        write_segment(
            v,
            save::CS,
            VmcbSegment {
                selector: 0xF000,
                attrib: 0x009B,
                limit: 0xFFFF,
                base: RESET_CS_BASE,
            },
        );
        for table in [save::GDTR, save::IDTR] {
            write_segment(
                v,
                table,
                VmcbSegment {
                    selector: 0,
                    attrib: 0,
                    limit: 0xFFFF,
                    base: 0,
                },
            );
        }
        launch(
            serial,
            Launch {
                label: "ovmf",
                vmcb,
                iopm,
                msrpm,
                ram_spa,
                // At reset EDX holds the CPU's family/model/stepping.
                gprs: GuestGprs {
                    rdx: u64::from(host_cpuid(1, 0).eax),
                    ..GuestGprs::default()
                },
                disk: fw.disk,
                banner: format!(
                    "{id}: entering the reset vector (RAM {} MiB at {ram_spa:#x}; flash: vars {:#x}+{:#x}, code {:#x}+{:#x})",
                    GUEST_RAM_BYTES / MIB,
                    flash.vars_gpa,
                    flash.vars_len,
                    flash.code_gpa,
                    flash.code_len
                ),
                platform: true,
                fwcfg: Some(machine_fwcfg(serial, handoff)),
                flash: [
                    FlashSpan {
                        gpa: flash.vars_gpa,
                        host: core::ptr::with_exposed_provenance::<u8>(fw.vars_base as usize),
                        len: fw.vars_len,
                    },
                    FlashSpan {
                        gpa: flash.code_gpa,
                        host: core::ptr::with_exposed_provenance::<u8>(fw.code_base as usize),
                        len: fw.code_len,
                    },
                ],
            },
        );
    }

    /// The `fw_cfg` device for a firmware guest: the ACPI tables that describe
    /// the machine, and the real board's SMBIOS identity when the firmware
    /// published one.
    fn machine_fwcfg(serial: &SerialPort, handoff: &BootHandoff) -> crate::fwcfg::FwCfg {
        use crate::acpi_tables::{
            AcpiParams, DSDT_AML, FILE_LOADER, FILE_RSDP, FILE_TABLES, build,
        };
        use crate::fwcfg::{FwCfg, MachineInfo};
        // SAFETY: the UEFI stage staged these into `LOADER_DATA` pages (never freed).
        let (smbios, anchor) =
            unsafe { (staged(handoff.smbios_tables), staged(handoff.smbios_anchor)) };
        let uuid = crate::smbios::system_uuid(smbios).unwrap_or(*b"enlil-guest-uuid");
        let mut fw = FwCfg::new(MachineInfo {
            uuid,
            ram_bytes: GUEST_RAM_BYTES,
            cpus: 1,
        });
        let acpi = build(&AcpiParams::default(), DSDT_AML);
        fw.add_file(FILE_LOADER, acpi.loader);
        fw.add_file(FILE_RSDP, acpi.rsdp);
        fw.add_file(FILE_TABLES, acpi.tables);
        if !smbios.is_empty() && !anchor.is_empty() {
            serial.write_str(&format!(
                "enlil kernel: ovmf: the guest sees the real machine's SMBIOS ({} bytes, {} structures)\n",
                smbios.len(),
                crate::smbios::structure_count(smbios)
            ));
            fw.add_file("etc/smbios/smbios-tables", smbios.to_vec());
            fw.add_file("etc/smbios/smbios-anchor", anchor.to_vec());
        } else {
            serial.write_str("enlil kernel: ovmf: no SMBIOS from the firmware - the guest firmware uses its own\n");
        }
        fw
    }

    /// Finish a prepared machine - intercepts, permission maps, timer gate, host
    /// save area - then run it until it stops and report why.
    fn launch(serial: &SerialPort, l: Launch) {
        let Launch {
            label,
            mut vmcb,
            iopm,
            mut msrpm,
            ram_spa,
            mut gprs,
            disk,
            banner,
            platform,
            flash,
            fwcfg,
        } = l;
        let tsc_hz = crate::kernel::TSC_HZ.load(Ordering::Acquire);
        let v = vmcb.as_bytes_mut();
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
            serial.write_str(&format!(
                "enlil kernel: {label}: no memory for the host-save page\n"
            ));
            return;
        }
        let vmcb_pa = vmcb.base_addr();
        core::mem::forget(vmcb);
        core::mem::forget(iopm);
        core::mem::forget(msrpm);

        serial.write_str(&format!("enlil kernel: {label}: {banner}\n"));

        let deadline = t0.saturating_add(tsc_hz.max(1).saturating_mul(RUN_SECONDS));
        let mem = GuestMem {
            base: core::ptr::with_exposed_provenance_mut::<u8>(ram_spa as usize),
            len: GUEST_RAM_BYTES as usize,
            flash,
        };
        // The guest's I/O bus, with its virtual disk if the ESP assigned one.
        let mut io = IoBus::new(tsc_hz.max(1));
        if platform {
            io.enable_platform(tsc_hz.max(1), GUEST_RAM_BYTES);
            if let Some(f) = fwcfg {
                io.set_fwcfg(f);
            }
        }
        attach_disk(serial, label, disk, &mut io);
        let (stop, counts, trace, irq_trace) = run(
            serial,
            label,
            vmcb_pa,
            host_save as u64,
            &mut gprs,
            deadline,
            mem,
            io,
        );
        report(
            serial,
            label,
            vmcb_pa,
            stop,
            &counts,
            platform.then_some((&*trace, &*irq_trace)),
        );
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
        label: &str,
        vmcb_pa: u64,
        host_save_pa: u64,
        gprs: &mut GuestGprs,
        deadline: u64,
        mem: GuestMem,
        mut bus: IoBus,
    ) -> (Stop, Counts, Box<TraceRing>, Box<TraceRing>) {
        // A firmware guest gets the platform devices around its CPU, and the
        // discovery log that shows what a silent firmware touches.
        let platform = bus.has_platform();
        let mut mmio = MmioBus::new();
        let mut trace = Box::new(TraceRing::new());
        // Accesses to the interrupt and timer hardware (PIC, PIT, local APIC),
        // kept apart so the console and polling loops cannot push them out.
        let mut irq_trace = Box::new(TraceRing::new());
        let mut pci_logged = 0usize;
        let mut pci_read_logged = 0usize;
        let mut io_seen: BTreeSet<(bool, u16)> = BTreeSet::new();
        let mut cpuid_seen: BTreeSet<u32> = BTreeSet::new();
        // The host timer deadline currently armed (avoids re-arming on every exit).
        let mut armed: Option<u64> = None;
        let mut msrs = if platform {
            MsrState::with_apic()
        } else {
            MsrState::new()
        };
        let mut console = LineBuf::new();
        let mut counts = Counts::default();
        let mut msr_seen: BTreeSet<(bool, u32)> = BTreeSet::new();
        let mut emit = |line: &str| {
            serial.write_str("[");
            serial.write_str(label);
            serial.write_str("] ");
            serial.write_str(line);
            serial.write_str("\n");
        };
        // The firmware's debug-port log (a DEBUG OVMF build prints here).
        let mut debug_console = LineBuf::new();
        let mut emit_debug = |line: &str| {
            serial.write_str("[");
            serial.write_str(label);
            serial.write_str("-dbg] ");
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
                    && let Some((vector, from_apic)) = bus.ack_source()
                {
                    // A local-APIC interrupt carries its vector's priority class and
                    // honours the guest's task priority (CR8, which Windows uses as
                    // its IRQL): the hardware holds it back while the priority is at
                    // or above that class. A legacy ExtINT interrupt ignores it.
                    let mut v = encode_vintr(vector, vector >> 4) & !V_INTR_MASKING;
                    if from_apic {
                        v &= !V_IGN_TPR;
                    }
                    set_int_control(pre, v);
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
            // Devices read the clock when the guest touches them (the PM timer, the
            // APIC timer's count): it must be now, not as of the last VMRUN.
            bus.set_time(crate::tsc::read_tsc());
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
                    let mut r = cpuid_policy(
                        leaf,
                        sub,
                        host_cpuid(leaf, sub),
                        read_save_u64(vmcb, save::CR4),
                    );
                    if platform {
                        // The local APIC is modelled, so the guest may see it.
                        if leaf == 1 {
                            r.edx |= 1 << 9;
                        }
                        trace.push(TraceEvent {
                            kind: TraceKind::Cpuid,
                            addr: u64::from(leaf),
                            value: u64::from(r.eax),
                            size: 4,
                            rip: guest_rip(vmcb),
                        });
                        if cpuid_seen.len() < DISCOVER_LIMIT && cpuid_seen.insert(leaf) {
                            serial.write_str(&format!(
                                "enlil kernel: {label}: cpuid leaf {leaf:#x}.{sub:#x} -> {:#x} {:#x} {:#x} {:#x} from RIP {:#x}\n",
                                r.eax, r.ebx, r.ecx, r.edx, guest_rip(vmcb)
                            ));
                        }
                    }
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
                        if platform {
                            trace.push(TraceEvent {
                                kind: if info.is_in() {
                                    TraceKind::IoIn
                                } else {
                                    TraceKind::IoOut
                                },
                                addr: u64::from(info.port()),
                                value: if info.is_rep() { gprs.rcx } else { 1 },
                                size: info.access_size(),
                                rip: guest_rip(vmcb),
                            });
                        }
                        let event = emulate_string_io(
                            mem,
                            vmcb,
                            gprs,
                            &mut bus,
                            &mut |b| console.push(b, &mut emit),
                            info,
                        );
                        match event {
                            Ok(IoEvent::Reset) => break Stop::Reset,
                            Ok(IoEvent::PowerOff) => break Stop::PowerOff,
                            Ok(IoEvent::None) => {}
                            Err(stop) => break stop,
                        }
                        bus.drain_debug(&mut |b| debug_console.push(b, &mut emit_debug));
                        set_guest_rip(vmcb, exit_info_2(vmcb));
                        continue;
                    }
                    let (port, size) = (info.port(), info.access_size());
                    if info.is_in() {
                        let val = u64::from(bus.read(port, size));
                        if platform
                            && matches!(port, 0xCFC..=0xCFF)
                            && pci_read_logged < 600
                            && let Some((dev, func, reg)) = bus.pci_present()
                            && dev == 3
                            && reg + usize::from(port - 0xCFC) >= 0x34
                        {
                            pci_read_logged += 1;
                            serial.write_str(&format!(
                                "enlil kernel: {label}: pci cfg read  {dev}:{func} reg {:#x} {size}B -> {val:#x}\n",
                                reg + usize::from(port - 0xCFC)
                            ));
                        }
                        if platform {
                            let event = TraceEvent {
                                kind: TraceKind::IoIn,
                                addr: u64::from(port),
                                value: val,
                                size,
                                rip: guest_rip(vmcb),
                            };
                            if irq_port(port) {
                                irq_trace.push(event);
                            }
                            if trace_worthy(port) {
                                trace.push(event);
                            }
                            if discover_port(&mut io_seen, true, port) {
                                serial.write_str(&format!(
                                    "enlil kernel: {label}: io IN  {port:#x} {size}B -> {val:#x} from RIP {:#x}\n",
                                    guest_rip(vmcb)
                                ));
                            }
                        }
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
                        if platform {
                            let event = TraceEvent {
                                kind: TraceKind::IoOut,
                                addr: u64::from(port),
                                value: u64::from(data),
                                size,
                                rip: guest_rip(vmcb),
                            };
                            if irq_port(port) {
                                irq_trace.push(event);
                            }
                            if trace_worthy(port) {
                                trace.push(event);
                            }
                            if discover_port(&mut io_seen, false, port) {
                                serial.write_str(&format!(
                                    "enlil kernel: {label}: io OUT {port:#x} {size}B <- {:#x} from RIP {:#x}\n",
                                    mask_to(u64::from(data), size),
                                    guest_rip(vmcb)
                                ));
                            }
                        }
                        // Config writes that assign something (not BAR sizing probes:
                        // all-ones, and the zero/original restore after them).
                        let assigns = !matches!(
                            mask_to(u64::from(data), size),
                            0 | 0xFFFF_FFFF | 0xFFFF_FFFE
                        );
                        if platform
                            && matches!(port, 0xCFC..=0xCFF)
                            && assigns
                            && pci_logged < DISCOVER_LIMIT * 4
                            && let Some((dev, func, reg)) = bus.pci_present()
                        {
                            pci_logged += 1;
                            serial.write_str(&format!(
                                "enlil kernel: {label}: pci cfg write {dev}:{func} reg {:#x} {size}B <- {:#x}\n",
                                reg + usize::from(port - 0xCFC),
                                mask_to(u64::from(data), size)
                            ));
                        }
                        let event =
                            bus.write(port, size, data, &mut |b| console.push(b, &mut emit));
                        if event == IoEvent::Reset {
                            break Stop::Reset;
                        }
                        if event == IoEvent::PowerOff {
                            break Stop::PowerOff;
                        }
                    }
                    bus.drain_debug(&mut |b| debug_console.push(b, &mut emit_debug));
                    set_guest_rip(vmcb, exit_info_2(vmcb));
                }
                RunLoopExit::Msr => {
                    counts.msr += 1;
                    let msr = u32::try_from(gprs.rcx & 0xFFFF_FFFF).unwrap_or(0);
                    let mut fault = false;
                    if msr_exit_is_write(exit_info_1(vmcb)) {
                        let val =
                            ((gprs.rdx & 0xFFFF_FFFF) << 32) | (guest_rax(vmcb) & 0xFFFF_FFFF);
                        trace.push(TraceEvent {
                            kind: TraceKind::Wrmsr,
                            addr: u64::from(msr),
                            value: val,
                            size: 8,
                            rip: guest_rip(vmcb),
                        });
                        match msrs.write(msr, val) {
                            MsrWrite::Ok => {}
                            MsrWrite::Tsc(t) => {
                                let offset = t.wrapping_sub(crate::tsc::read_tsc());
                                set_tsc_offset(vmcb, offset.cast_signed());
                            }
                            MsrWrite::Efer(v) => {
                                let cur = read_save_u64(vmcb, save::EFER);
                                write_save_u64(vmcb, save::EFER, vmcb_efer_after_write(cur, v));
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
                            MsrRead::Efer => Some(guest_efer_view(read_save_u64(vmcb, save::EFER))),
                            MsrRead::Gp => None,
                        };
                        trace.push(TraceEvent {
                            kind: TraceKind::Rdmsr,
                            addr: u64::from(msr),
                            value: value.unwrap_or(u64::MAX),
                            size: 8,
                            rip: guest_rip(vmcb),
                        });
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
                                "enlil kernel: {label}: guest {} of unmodelled MSR {msr:#x} at RIP {:#x} -> #GP\n",
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
                    if let Err(stop) = emulate_mmio(
                        serial,
                        label,
                        mem,
                        vmcb,
                        gprs,
                        &mut mmio,
                        &mut bus,
                        (&mut trace, &mut irq_trace),
                        gpa,
                    ) {
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
        debug_console.flush(&mut emit_debug);
        if platform {
            serial.write_str(&format!(
                "enlil kernel: {label}: interrupt state at stop: PIC IMR master {:#04x} slave {:#04x}, pending vector {:?}, next device deadline {:?} (now {})\n",
                bus.pic.read(0x21).unwrap_or(0xFF),
                bus.pic.read(0xA1).unwrap_or(0xFF),
                bus.pending_vector(),
                bus.next_deadline(),
                crate::tsc::read_tsc(),
            ));
        }
        (stop, counts, trace, irq_trace)
    }

    /// Emulate an `INS`/`OUTS` (optionally `REP`-prefixed) against the I/O bus:
    /// each element moves between the port and the guest memory `ES:rDI` (for
    /// `INS`) or `DS:rSI` (for `OUTS`), the index register and `rCX` are updated,
    /// and the direction flag is honoured. The caller advances `RIP`.
    fn emulate_string_io(
        mem: GuestMem,
        vmcb: &mut [u8],
        gprs: &mut GuestGprs,
        bus: &mut IoBus,
        tx: &mut impl FnMut(u8),
        info: IoioExitInfo,
    ) -> Result<IoEvent, Stop> {
        let raw = exit_info_1(vmcb);
        let (port, size) = (info.port(), info.access_size());
        let addr_mask = if raw & (1 << 9) != 0 {
            u64::MAX
        } else if raw & (1 << 8) != 0 {
            0xFFFF_FFFF
        } else {
            0xFFFF
        };
        let step = u64::from(size);
        let backwards = read_save_u64(vmcb, save::RFLAGS) & (1 << 10) != 0;
        let paging = read_save_u64(vmcb, save::CR0) & CR0_PG != 0;
        let cr3 = read_save_u64(vmcb, save::CR3);
        let seg = if info.is_in() { save::ES } else { save::DS };
        let seg_base = read_segment(vmcb, seg).base;
        let index = if info.is_in() { gprs.rdi } else { gprs.rsi };
        let count = if info.is_rep() {
            gprs.rcx & addr_mask
        } else {
            1
        };
        let read_entry = |gpa: u64| mem.read_u64(gpa);

        let mut addr = index & addr_mask;
        let mut done = 0u64;
        let mut event = IoEvent::None;
        while done < count {
            let linear = seg_base.wrapping_add(addr);
            let gpa = if paging {
                translate_gva(cr3, linear & !0xFFF, &read_entry).map(|page| page + (linear & 0xFFF))
            } else {
                Some(linear & 0xFFFF_FFFF)
            };
            let Some(gpa) = gpa else {
                return Err(Stop::StringIo(port));
            };
            if info.is_in() {
                let value = bus.read(port, size).to_le_bytes();
                if !mem.write(gpa, &value[..usize::from(size)]) {
                    return Err(Stop::StringIo(port));
                }
            } else {
                let mut value = [0u8; 4];
                if !mem.read(gpa, &mut value[..usize::from(size)]) {
                    return Err(Stop::StringIo(port));
                }
                event = bus.write(port, size, u32::from_le_bytes(value), tx);
            }
            addr = if backwards {
                addr.wrapping_sub(step)
            } else {
                addr.wrapping_add(step)
            } & addr_mask;
            done += 1;
            if event != IoEvent::None {
                break;
            }
        }
        let merged = |old: u64, new: u64| (old & !addr_mask) | (new & addr_mask);
        if info.is_in() {
            gprs.rdi = merged(gprs.rdi, addr);
        } else {
            gprs.rsi = merged(gprs.rsi, addr);
        }
        if info.is_rep() {
            gprs.rcx = merged(gprs.rcx, count - done);
        }
        Ok(event)
    }

    /// Whether `port` belongs to the interrupt and timer hardware.
    const fn irq_port(port: u16) -> bool {
        matches!(
            port,
            0x20 | 0x21 | 0xA0 | 0xA1 | 0x40..=0x43 | 0x4D0 | 0x4D1
        )
    }

    /// Whether an access to `port` belongs in the trace ring: the console and the
    /// PCI configuration mechanism run constantly and would push everything else
    /// out of it.
    const fn trace_worthy(port: u16) -> bool {
        !matches!(port, 0x3F8..=0x3FF | 0xCF8 | 0xCFC..=0xCFF)
    }

    /// Whether this is the first touch of `port` in direction `read` that is
    /// worth a discovery line: ports the model already handles every time
    /// (console, interrupt controllers, timer, CMOS, PCI config) are not.
    fn discover_port(seen: &mut BTreeSet<(bool, u16)>, read: bool, port: u16) -> bool {
        let known = matches!(
            port,
            0x20 | 0x21 | 0xA0 | 0xA1 | 0x40..=0x43 | 0x61 | 0x70 | 0x71 | 0xCF8 | 0xCFC..=0xCFF
                | 0x3F8..=0x3FF
        );
        !known && seen.len() < DISCOVER_LIMIT && seen.insert((read, port))
    }

    /// Log why the guest stopped, with enough VMCB state to debug it.
    fn report(
        serial: &SerialPort,
        label: &str,
        vmcb_pa: u64,
        stop: Stop,
        counts: &Counts,
        trace: Option<(&TraceRing, &TraceRing)>,
    ) {
        // SAFETY: `vmcb_pa` is our live, page-sized VMCB.
        let vmcb = unsafe {
            core::slice::from_raw_parts(
                core::ptr::with_exposed_provenance::<u8>(vmcb_pa as usize),
                VMCB_SIZE,
            )
        };
        let why = match stop {
            Stop::DeadHalt => String::from("halted with interrupts off (powered off)"),
            Stop::PowerOff => String::from("powered off (ACPI soft-off)"),
            other => format!("{other:?}"),
        };
        serial.write_str(&format!(
            "enlil kernel: {label}: guest stopped: {why} after {} exits (io {}, cpuid {}, msr {}, mmio {}, hlt {}, intr {}, irq-injected {})\n",
            counts.exits, counts.io, counts.cpuid, counts.msr, counts.mmio, counts.hlt, counts.intr, counts.injected
        ));
        serial.write_str(&format!(
            "enlil kernel: {label}:   RIP {:#x} RSP {:#x} RFLAGS {:#x} CR0 {:#x} CR3 {:#x} CR4 {:#x} EFER {:#x} exit {:#x}\n",
            guest_rip(vmcb),
            read_save_u64(vmcb, save::RSP),
            read_save_u64(vmcb, save::RFLAGS),
            read_save_u64(vmcb, save::CR0),
            read_save_u64(vmcb, save::CR3),
            read_save_u64(vmcb, save::CR4),
            read_save_u64(vmcb, save::EFER),
            exit_code(vmcb).raw(),
        ));
        if let Some((trace, irq_trace)) = trace {
            for (name, ring) in [("device (PIC/PIT/APIC/disk)", irq_trace), ("all", trace)] {
                // The interrupt ring can be long; the last 40 tell the story.
                let skip =
                    ring.recent()
                        .count()
                        .saturating_sub(if name == "all" { 192 } else { 120 });
                serial.write_str(&format!(
                    "enlil kernel: {label}: last {} of {} traced {name} guest events:\n",
                    ring.recent().count() - skip,
                    ring.total()
                ));
                for e in ring.recent().skip(skip) {
                    serial.write_str(&format!(
                        "enlil kernel: {label}:   {:?} {:#x} = {:#x} ({}B) rip {:#x}\n",
                        e.kind, e.addr, e.value, e.size, e.rip
                    ));
                }
            }
        }
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
    fn efer_is_emulated_so_the_guest_never_sees_or_clears_svme() {
        // The hardware refuses to VMRUN a guest whose EFER lacks SVME; a guest
        // that writes EFER = 0 (as a bootloader leaving long mode does) must not
        // be able to clear it, and must not see it set either.
        let mut m = MsrState::new();
        assert_eq!(m.read(MSR_EFER), MsrRead::Efer);
        assert!(!MSR_PASSTHROUGH.contains(&MSR_EFER));
        let live = EFER_SVME | EFER_LMA | (1 << 8) | (1 << 11) | 1;
        assert_eq!(guest_efer_view(live), live & !EFER_SVME);
        // Writing zero (leaving long mode) keeps SVME on and drops the rest.
        assert_eq!(m.write(MSR_EFER, 0), MsrWrite::Efer(0));
        assert_eq!(vmcb_efer_after_write(live, 0), EFER_SVME | EFER_LMA);
        // Setting LME and NXE (the Linux decompressor) takes effect.
        let after = vmcb_efer_after_write(EFER_SVME, (1 << 8) | (1 << 11));
        assert_eq!(after, EFER_SVME | (1 << 8) | (1 << 11));
        // The processor owns LMA: a guest's attempt to set it changes nothing.
        assert_eq!(vmcb_efer_after_write(EFER_SVME, EFER_LMA), EFER_SVME);
        // Reserved bits are a #GP, not silently dropped.
        assert_eq!(m.write(MSR_EFER, 1 << 40), MsrWrite::Gp);
        assert_eq!(m.write(MSR_EFER, 1 << 9), MsrWrite::Gp);
        // A guest that writes back exactly what it read is fine.
        assert_eq!(
            m.write(MSR_EFER, guest_efer_view(live)),
            MsrWrite::Efer(live & !EFER_SVME)
        );
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
    fn the_trace_ring_keeps_the_latest_events_in_order() {
        let ev = |n: u64| TraceEvent {
            kind: TraceKind::IoOut,
            addr: n,
            value: 0,
            size: 1,
            rip: 0,
        };
        let mut t = TraceRing::new();
        assert_eq!(t.recent().count(), 0);
        for n in 0..3 {
            t.push(ev(n));
        }
        assert_eq!(t.recent().map(|e| e.addr).collect::<Vec<_>>(), [0, 1, 2]);
        // Overflow: only the last TRACE_DEPTH remain, still oldest first.
        for n in 3..(TRACE_DEPTH as u64 + 10) {
            t.push(ev(n));
        }
        let kept: Vec<u64> = t.recent().map(|e| e.addr).collect();
        assert_eq!(kept.len(), TRACE_DEPTH);
        assert_eq!(kept[0], 10);
        assert_eq!(kept[TRACE_DEPTH - 1], TRACE_DEPTH as u64 + 9);
        assert_eq!(t.total(), TRACE_DEPTH as u64 + 10);
    }

    #[test]
    fn a_guest_with_an_apic_reads_it_enabled_at_the_architectural_base() {
        let m = MsrState::with_apic();
        assert_eq!(m.read(MSR_APIC_BASE), MsrRead::Value(0xFEE0_0900));
        // Without one the APIC reads disabled, as the Linux guest expects.
        assert_eq!(
            MsrState::new().read(MSR_APIC_BASE),
            MsrRead::Value(0xFEE0_0100)
        );
    }

    #[test]
    fn the_debug_port_collects_firmware_log_bytes_for_a_platform_guest() {
        let mut out = Vec::new();
        let mut bus = IoBus::new(1_000_000_000);
        // No platform: the port is an absent device, and nothing is collected.
        assert_eq!(bus.read(0x402, 1), 0xFF);
        bus.write(0x402, 1, u32::from(b'x'), &mut tx_to(&mut out));
        let mut got = Vec::new();
        bus.drain_debug(&mut |b| got.push(b));
        assert_eq!(got, b"");

        bus.enable_platform(1_000_000_000, 256 * MIB);
        assert_eq!(bus.read(0x402, 1), 0xE9, "QEMU's debug-port signature");
        for b in b"DXE" {
            bus.write(0x402, 1, u32::from(*b), &mut tx_to(&mut out));
        }
        bus.drain_debug(&mut |b| got.push(b));
        assert_eq!(got, b"DXE");
        // Drained bytes are gone, and nothing leaked to the UART transmitter.
        let mut again = Vec::new();
        bus.drain_debug(&mut |b| again.push(b));
        assert!(again.is_empty() && out.is_empty());
    }

    #[test]
    fn only_a_platform_guest_has_a_local_apic() {
        let mut bus = IoBus::new(1_000_000_000);
        assert_eq!(bus.lapic_offset(0xFEE0_0030), None);
        assert_eq!(bus.lapic_read(0x30, 4), 0xFFFF_FFFF);
        bus.enable_platform(1_000_000_000, 256 * MIB);
        assert_eq!(bus.lapic_offset(0xFEE0_00F0), Some(0xF0));
        assert_eq!(bus.lapic_offset(0xFEE0_1000), None, "past the window");
        assert_eq!(bus.lapic_read(0x30, 4), 0x0005_0014);
        bus.lapic_write(0xF0, 4, 0x1FF);
        assert_eq!(bus.lapic_read(0xF0, 4), 0x1FF);
        // The I/O APIC is not modelled yet: it still reads as an absent device.
        assert_eq!(MmioBus::new().read(0xFEC0_0000, 4), 0xFFFF_FFFF);
    }

    #[test]
    fn the_apic_timer_interrupts_the_guest_alongside_the_pic() {
        let mut bus = IoBus::new(1_000_000_000);
        bus.enable_platform(1_000_000_000, 256 * MIB);
        // OVMF's tick: periodic, vector 0x20, 1 ms at 1 GHz.
        bus.set_time(0);
        bus.lapic_write(0xF0, 4, 0x1FF);
        bus.lapic_write(0x3E0, 4, 0xB);
        bus.lapic_write(0x320, 4, 0x20020);
        bus.lapic_write(0x380, 4, 1_000_000);
        assert_eq!(bus.next_deadline(), Some(1_000_000));
        bus.set_time(1_000_000);
        bus.poll_devices();
        assert_eq!(bus.pending_vector(), Some(0x20));
        assert_eq!(bus.ack(), Some(0x20));
        bus.lapic_write(0xB0, 4, 0); // EOI
        // A PIC interrupt is held back by the APIC's masked virtual wire...
        let mut out = Vec::new();
        for (port, val) in [
            (0x20, 0x11),
            (0x21, 0x30),
            (0x21, 0x04),
            (0x21, 0x01),
            (0x21, 0x00),
        ] {
            bus.write(port, 1, val, &mut tx_to(&mut out));
        }
        bus.pic.set_irq(1, true);
        assert_eq!(
            bus.pending_vector(),
            None,
            "LINT0 is masked with the APIC on"
        );
        // ...and flows once firmware sets up the virtual wire.
        bus.lapic_write(0x350, 4, 0x700);
        assert_eq!(bus.pending_vector(), Some(0x31));
        assert_eq!(bus.ack(), Some(0x31));
    }

    /// Select `bus:dev.func` register `reg` through the legacy PCI ports.
    fn pci_cfg(dev: u8, func: u8, reg: u8) -> u32 {
        0x8000_0000 | (u32::from(dev) << 11) | (u32::from(func) << 8) | u32::from(reg)
    }

    #[test]
    fn a_firmware_platform_answers_pci_cmos_and_the_pm_timer() {
        let mut bus = IoBus::new(1_000_000_000);
        let mut out = Vec::new();
        // Without the platform nothing answers on the PCI bus.
        bus.write(0xCF8, 4, pci_cfg(0, 0, 0), &mut tx_to(&mut out));
        assert_eq!(bus.read(0xCFC, 4), u32::MAX);

        bus.enable_platform(1_000_000_000, 256 * MIB);
        assert!(bus.has_platform());
        // The i440FX host bridge, then the PIIX4 PM function.
        bus.write(0xCF8, 4, pci_cfg(0, 0, 0), &mut tx_to(&mut out));
        assert_eq!(bus.read(0xCFC, 4), 0x1237_8086);
        bus.write(0xCF8, 4, pci_cfg(1, 3, 0), &mut tx_to(&mut out));
        assert_eq!(bus.read(0xCFC, 4), 0x7113_8086);
        // CMOS: 256 MiB reads back as 0x0F00 in 64 KiB units above 16 MiB.
        bus.write(0x70, 1, 0x35, &mut tx_to(&mut out));
        assert_eq!(bus.read(0x71, 1), 0x0F);
        // The disk's slot still works next to the platform's.
        bus.write(
            0xCF8,
            4,
            pci_cfg(VIRTIO_BLK_SLOT, 0, 0),
            &mut tx_to(&mut out),
        );
        assert_eq!(bus.read(0xCFC, 4), u32::MAX, "no disk attached");

        // Firmware programs PMBA like OVMF; the timer then ticks with host time.
        bus.write(0xCF8, 4, pci_cfg(1, 3, 0x40), &mut tx_to(&mut out));
        bus.write(0xCFC, 4, 0xB000, &mut tx_to(&mut out));
        bus.write(0xCF8, 4, pci_cfg(1, 3, 0x80), &mut tx_to(&mut out));
        bus.write(0xCFC, 1, 1, &mut tx_to(&mut out));
        bus.set_time(10_000);
        let t0 = bus.read(0xB008, 4);
        bus.set_time(10_000 + 1_000_000);
        assert_eq!(bus.read(0xB008, 4) - t0, 3579, "1 ms of host time");
        // Soft-off through PM1_CNT surfaces as a power-off event.
        assert_eq!(
            bus.write(0xB004, 2, 1 << 13, &mut tx_to(&mut out)),
            IoEvent::PowerOff
        );
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

    /// Read a PCI config register through the legacy CF8/CFC mechanism.
    fn cfg_read(bus: &mut IoBus, dev: u8, func: u8, reg: u8, size: u8) -> u32 {
        let mut out = Vec::new();
        let addr =
            0x8000_0000 | (u32::from(dev) << 11) | (u32::from(func) << 8) | u32::from(reg & 0xFC);
        bus.write(0xCF8, 4, addr, &mut tx_to(&mut out));
        bus.read(0xCFC + u16::from(reg & 3), size)
    }

    fn cfg_write(bus: &mut IoBus, dev: u8, reg: u8, size: u8, val: u32) {
        let mut out = Vec::new();
        let addr = 0x8000_0000 | (u32::from(dev) << 11) | u32::from(reg & 0xFC);
        bus.write(0xCF8, 4, addr, &mut tx_to(&mut out));
        bus.write(0xCFC + u16::from(reg & 3), size, val, &mut tx_to(&mut out));
    }

    #[test]
    fn pci_bus_is_empty_until_a_disk_is_attached() {
        let mut bus = IoBus::new(TEST_HZ);
        assert_eq!(cfg_read(&mut bus, VIRTIO_BLK_SLOT, 0, 0, 4), u32::MAX);
        bus.attach_disk(Box::new(crate::blockdev::RamDisk::new(8)), false);
        assert_eq!(cfg_read(&mut bus, VIRTIO_BLK_SLOT, 0, 0, 4), 0x1042_1AF4);
    }

    #[test]
    fn the_disk_enumerates_like_real_pci_hardware() {
        let mut bus = IoBus::new(TEST_HZ);
        bus.attach_disk(Box::new(crate::blockdev::RamDisk::new(8)), false);
        let slot = VIRTIO_BLK_SLOT;
        // Other slots and functions stay empty.
        assert_eq!(cfg_read(&mut bus, 0, 0, 0, 4), u32::MAX);
        assert_eq!(cfg_read(&mut bus, slot + 1, 0, 0, 4), u32::MAX);
        assert_eq!(
            cfg_read(&mut bus, slot, 1, 0, 4),
            u32::MAX,
            "single-function device"
        );
        // Identity, with byte and word accesses through the data port window.
        assert_eq!(cfg_read(&mut bus, slot, 0, 0, 2), 0x1AF4);
        assert_eq!(cfg_read(&mut bus, slot, 0, 2, 2), 0x1042);
        assert_eq!(
            cfg_read(&mut bus, slot, 0, 0x0B, 1),
            0x01,
            "mass storage class"
        );
        assert_eq!(
            cfg_read(&mut bus, slot, 0, 0x3C, 1),
            u32::from(VIRTIO_BLK_IRQ)
        );
        // The standard BAR sizing probe sees a 16 KiB 64-bit memory BAR.
        cfg_write(&mut bus, slot, 0x10, 4, 0xFFFF_FFFF);
        cfg_write(&mut bus, slot, 0x14, 4, 0xFFFF_FFFF);
        let lo = cfg_read(&mut bus, slot, 0, 0x10, 4);
        assert_eq!(lo & 0xF, 0x4);
        assert_eq!(
            lo & !0xF,
            0xFFFF_C000,
            "16 KiB: low 14 bits are not address bits"
        );
        // Program it and enable memory decoding + bus mastering.
        cfg_write(&mut bus, slot, 0x10, 4, 0x1000_0000);
        cfg_write(&mut bus, slot, 0x14, 4, 0);
        assert_eq!(
            bus.virtio.as_ref().unwrap().pci.bar0(),
            None,
            "not decoding yet"
        );
        cfg_write(&mut bus, slot, 0x04, 2, 0x0006);
        assert_eq!(
            bus.virtio.as_ref().unwrap().pci.bar0(),
            Some((0x1000_0000, 0x4000))
        );
        assert!(bus.virtio.as_ref().unwrap().pci.bus_master());
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
