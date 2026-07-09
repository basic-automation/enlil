//! KVM hypervisor backend.
//!
//! This is the Phase 0.2 backend that unlocks Phase 5 (Windows guest support):
//! a thin, safe wrapper over the `kvm-ioctls` crate that creates a VM, maps
//! guest memory, creates vCPUs, and drives the vCPU run loop, translating
//! `kvm-ioctls` exits into the hypervisor-agnostic [`GuestExit`] used by the
//! rest of `enlil-core`.
//!
//! The backend itself is Linux-only (it talks to `/dev/kvm`). The
//! platform-independent exit model ([`GuestExit`], [`RunOutcome`],
//! [`VmExitHandler`]) is compiled on every target so the exit-dispatch logic
//! can be unit-tested without KVM, and so other backends (WHP on Windows,
//! bare-metal VMX in Phase 6) can reuse the same vocabulary.
//!
//! # Design
//!
//! Reads/writes from the guest are delivered to a caller-supplied
//! [`VmExitHandler`]. On an I/O-in or MMIO-read exit the handler fills the
//! supplied buffer in place; KVM hands those bytes back to the guest on the
//! next `KVM_RUN`. This mirrors the data flow in Cloud Hypervisor and
//! Firecracker and keeps device emulation cleanly decoupled from the backend.

/// A hypervisor-agnostic summary of why a vCPU stopped executing guest code.
///
/// This is an *owned* description (no borrows into the KVM run page), suitable
/// for logging, metrics, and tests. The actual data transfer for read exits
/// happens through [`VmExitHandler`] before this value is produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestExit {
    /// Guest executed `in` from a port; `size` bytes were requested.
    IoIn { port: u16, size: usize },
    /// Guest executed `out` to a port with `size` bytes of data.
    IoOut { port: u16, size: usize },
    /// Guest read from an MMIO address; `size` bytes were requested.
    MmioRead { addr: u64, size: usize },
    /// Guest wrote `size` bytes to an MMIO address.
    MmioWrite { addr: u64, size: usize },
    /// Guest executed `rdmsr` on a model-specific register forwarded to
    /// userspace; the handler supplied (or refused) the value before this was
    /// produced. Only fires when userspace MSR forwarding is enabled (see
    /// [`KvmBackend::enable_userspace_msr_exits`]).
    MsrRead { msr: u32 },
    /// Guest executed `wrmsr` on a forwarded model-specific register with
    /// `value`; the handler accepted (or refused) it before this was produced.
    MsrWrite { msr: u32, value: u64 },
    /// Guest executed `hlt`.
    Halted,
    /// Guest requested shutdown / triple fault.
    Shutdown,
    /// Hardware debug event (breakpoint, single-step).
    Debug,
    /// An interrupt arrived for the host; the run loop should continue.
    Interrupted,
    /// KVM reported an internal error.
    InternalError,
    /// VM-entry failed; carries the hardware failure reason.
    FailedEntry(u64),
    /// An exit reason this build does not model yet; carries the raw value.
    Unsupported(u32),
}

/// What the run loop should do after a single `KVM_RUN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// Re-enter the guest.
    Continue,
    /// The guest stopped for good (halt/shutdown/fatal error).
    Stopped,
}

impl GuestExit {
    /// Whether this exit means the guest should stop running.
    ///
    /// `hlt`, shutdown, fatal entry failures and internal errors are terminal;
    /// I/O, MMIO, debug and interrupt exits are resumable.
    #[must_use]
    pub const fn outcome(&self) -> RunOutcome {
        match self {
            Self::Halted | Self::Shutdown | Self::InternalError | Self::FailedEntry(_) => {
                RunOutcome::Stopped
            }
            Self::IoIn { .. }
            | Self::IoOut { .. }
            | Self::MmioRead { .. }
            | Self::MmioWrite { .. }
            | Self::MsrRead { .. }
            | Self::MsrWrite { .. }
            | Self::Debug
            | Self::Interrupted
            | Self::Unsupported(_) => RunOutcome::Continue,
        }
    }
}

/// Handles guest I/O and MMIO accesses surfaced by vCPU exits.
///
/// For read exits (`io_in`, `mmio_read`) the implementation must fill `data`
/// in place; those bytes are returned to the guest. The default
/// implementations leave the buffer untouched (reads return all-ones from the
/// caller's perspective if it pre-fills, or zeroes otherwise) and ignore
/// writes, which is the correct behaviour for an unmapped address.
pub trait VmExitHandler {
    /// Guest read `data.len()` bytes from I/O `port`; fill `data`.
    fn io_in(&mut self, port: u16, data: &mut [u8]) {
        let _ = (port, data);
    }
    /// Guest wrote `data` to I/O `port`.
    fn io_out(&mut self, port: u16, data: &[u8]) {
        let _ = (port, data);
    }
    /// Guest read `data.len()` bytes from MMIO `addr`; fill `data`.
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) {
        let _ = (addr, data);
    }
    /// Guest wrote `data` to MMIO `addr`.
    fn mmio_write(&mut self, addr: u64, data: &[u8]) {
        let _ = (addr, data);
    }
    /// Guest executed `rdmsr` on model-specific register `msr`. Return
    /// `Some(value)` to supply the 64-bit contents, or `None` to inject a
    /// `#GP` into the guest (the architecturally honest answer for an MSR this
    /// hypervisor does not model). The default refuses every MSR.
    fn rdmsr(&mut self, msr: u32) -> Option<u64> {
        let _ = msr;
        None
    }
    /// Guest executed `wrmsr` of `value` to model-specific register `msr`.
    /// Return `true` if the write is accepted, or `false` to inject a `#GP`
    /// (an MSR this hypervisor does not model). The default refuses every MSR.
    fn wrmsr(&mut self, msr: u32, value: u64) -> bool {
        let _ = (msr, value);
        false
    }
}

/// A [`VmExitHandler`] that records every access, for tests and tracing.
#[derive(Debug, Default)]
pub struct RecordingHandler {
    /// `(port, bytes)` for each `out` access, in order.
    pub io_out: Vec<(u16, Vec<u8>)>,
    /// `(port, size)` for each `in` access, in order.
    pub io_in: Vec<(u16, usize)>,
    /// `(addr, bytes)` for each MMIO write, in order.
    pub mmio_write: Vec<(u64, Vec<u8>)>,
    /// `(addr, size)` for each MMIO read, in order.
    pub mmio_read: Vec<(u64, usize)>,
    /// Each `rdmsr` index, in order.
    pub msr_read: Vec<u32>,
    /// `(msr, value)` for each `wrmsr`, in order.
    pub msr_write: Vec<(u32, u64)>,
}

impl VmExitHandler for RecordingHandler {
    fn io_in(&mut self, port: u16, data: &mut [u8]) {
        self.io_in.push((port, data.len()));
    }
    fn io_out(&mut self, port: u16, data: &[u8]) {
        self.io_out.push((port, data.to_vec()));
    }
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) {
        self.mmio_read.push((addr, data.len()));
    }
    fn mmio_write(&mut self, addr: u64, data: &[u8]) {
        self.mmio_write.push((addr, data.to_vec()));
    }
    fn rdmsr(&mut self, msr: u32) -> Option<u64> {
        self.msr_read.push(msr);
        Some(0)
    }
    fn wrmsr(&mut self, msr: u32, value: u64) -> bool {
        self.msr_write.push((msr, value));
        true
    }
}

// ===========================================================================
// Linux / KVM implementation
// ===========================================================================

#[cfg(target_os = "linux")]
mod linux {
    use super::{GuestExit, VmExitHandler};
    use crate::error::{Error, Result};
    use enlil_devices::usb::xhci::transfer::DmaMemory;
    use kvm_bindings::kvm_userspace_memory_region;
    use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};
    use std::alloc::{alloc_zeroed, dealloc, handle_alloc_error, Layout};
    use std::ptr::NonNull;
    // `ioctl_iow_nr!` expands to a bare `ioctl_ioc_nr!` call, so both macros
    // (and the `ioctl_with_ref` helper) must be in scope for KVM_X86_SET_MSR_FILTER.
    use vmm_sys_util::ioctl::ioctl_with_ref;
    use vmm_sys_util::ioctl_ioc_nr;

    /// Host page size (4 KiB on x86-64). KVM requires a memory region's guest
    /// physical base, size, and backing host address to all be multiples of
    /// this — `KVM_SET_USER_MEMORY_REGION` returns `EINVAL` otherwise.
    pub const HOST_PAGE_SIZE: usize = 4096;

    // `KVM_X86_SET_MSR_FILTER` has no `kvm-ioctls` 0.19 wrapper, so declare the
    // request number directly (`_IOW(KVMIO, 0xc6, struct kvm_msr_filter)`).
    vmm_sys_util::ioctl_iow_nr!(
        KVM_X86_SET_MSR_FILTER,
        kvm_bindings::KVMIO,
        0xc6,
        kvm_bindings::kvm_msr_filter
    );

    /// Returns `true` if this host exposes a usable `/dev/kvm`.
    ///
    /// Used to gate integration tests and to fail fast with a clear message
    /// when nested virtualization is unavailable.
    #[must_use]
    pub fn is_kvm_available() -> bool {
        Kvm::new().is_ok()
    }

    /// A page-aligned, zeroed host buffer suitable for backing guest RAM.
    ///
    /// `KVM_SET_USER_MEMORY_REGION` requires the `userspace_addr` of a guest
    /// memory slot to be page-aligned; a plain `Vec<u8>` is only byte-aligned,
    /// so it cannot legally back a KVM memory region (the ioctl rejects it with
    /// `EINVAL`). `GuestRam` owns a page-aligned allocation, rounded up to a
    /// whole number of pages, and hands out its host base address and a slice
    /// over the bytes. Drop releases the allocation.
    pub struct GuestRam {
        ptr: NonNull<u8>,
        layout: Layout,
    }

    impl GuestRam {
        /// Allocate `size` bytes of zeroed, page-aligned host memory. `size`
        /// is rounded up to a whole number of pages.
        ///
        /// # Panics
        /// Panics if `size` is zero, if the page-rounded size overflows a
        /// valid [`Layout`], or if the allocation fails.
        #[must_use]
        pub fn new(size: usize) -> Self {
            assert!(size != 0, "guest RAM size must be non-zero");
            let pages = size.div_ceil(HOST_PAGE_SIZE);
            let alloc_size = pages * HOST_PAGE_SIZE;
            let layout = Layout::from_size_align(alloc_size, HOST_PAGE_SIZE)
                .expect("page-aligned guest RAM layout");
            // SAFETY: `layout` has a non-zero size (size != 0 ⇒ pages ≥ 1).
            let raw = unsafe { alloc_zeroed(layout) };
            let ptr = NonNull::new(raw).unwrap_or_else(|| handle_alloc_error(layout));
            Self { ptr, layout }
        }

        /// Host virtual base address as a `u64` — the page-aligned
        /// `userspace_addr` to register with [`KvmBackend::map_memory`].
        #[must_use]
        pub fn host_addr(&self) -> u64 {
            self.ptr.as_ptr() as u64
        }

        /// Allocated size in bytes (the requested size rounded up to a whole
        /// number of pages).
        #[must_use]
        pub fn len(&self) -> usize {
            self.layout.size()
        }

        /// Always `false` — a [`GuestRam`] is never empty (size is non-zero).
        #[must_use]
        pub const fn is_empty(&self) -> bool {
            false
        }

        /// The backing bytes as a shared slice.
        #[must_use]
        pub fn as_slice(&self) -> &[u8] {
            // SAFETY: `ptr` is valid for `layout.size()` zeroed bytes we own
            // and keep alive for `&self`.
            unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.layout.size()) }
        }

        /// The backing bytes as a mutable slice.
        #[must_use]
        pub fn as_mut_slice(&mut self) -> &mut [u8] {
            // SAFETY: as `as_slice`, with unique access via `&mut self`.
            unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size()) }
        }
    }

    impl Drop for GuestRam {
        fn drop(&mut self) {
            // SAFETY: `ptr`/`layout` came from `alloc_zeroed(layout)` and the
            // region has not been freed elsewhere.
            unsafe { dealloc(self.ptr.as_ptr(), self.layout) }
        }
    }

    /// Validate that a memory region meets KVM's page-alignment requirements
    /// *before* the ioctl, so a misaligned region produces an actionable error
    /// instead of the kernel's opaque `EINVAL`.
    ///
    /// # Errors
    /// Returns [`Error::HypervisorError`] if `size` is zero or any of
    /// `guest_phys_addr`, `host_addr`, or `size` is not page-aligned.
    pub(super) fn validate_region(guest_phys_addr: u64, host_addr: u64, size: u64) -> Result<()> {
        const PAGE_MASK: u64 = HOST_PAGE_SIZE as u64 - 1;
        if size == 0 {
            return Err(Error::HypervisorError(
                "guest memory region size must be non-zero".to_string(),
            ));
        }
        if guest_phys_addr & PAGE_MASK != 0 || host_addr & PAGE_MASK != 0 || size & PAGE_MASK != 0 {
            return Err(Error::HypervisorError(format!(
                "KVM memory region must be page-aligned ({HOST_PAGE_SIZE:#x}): \
                 guest_phys_addr={guest_phys_addr:#x}, host_addr={host_addr:#x}, size={size:#x}"
            )));
        }
        Ok(())
    }

    /// A guest memory region registered with KVM.
    #[derive(Debug, Clone, Copy)]
    pub struct MemSlot {
        /// KVM memory slot index.
        pub slot: u32,
        /// Guest physical address the region is mapped at.
        pub guest_phys_addr: u64,
        /// Region size in bytes.
        pub size: u64,
        /// Host virtual base address backing this region — the same
        /// `host_addr` passed to [`KvmBackend::map_memory`]. Retained so the
        /// hypervisor can translate guest-physical addresses back to host
        /// pointers for device DMA (see [`GuestMemory`]).
        pub host_addr: u64,
    }

    /// A device-facing view over a VM's registered guest-physical memory.
    ///
    /// Emulated devices (xHCI rings/contexts, virtio queues, …) DMA into and
    /// out of guest RAM through the [`DmaMemory`] seam. This view translates a
    /// guest-physical address to the host pointer of whichever registered
    /// [`MemSlot`] contains it and copies through it. Obtain one with
    /// [`KvmBackend::guest_memory`].
    ///
    /// It borrows the backend's slot table, so it cannot outlive the VM; the
    /// raw-pointer accesses are sound by the same contract that
    /// [`KvmBackend::map_memory`] already requires (each registered region is a
    /// valid host mapping for the VM's lifetime). A single access must fall
    /// entirely within one slot, mirroring how the guest sees discontiguous
    /// physical regions.
    pub struct GuestMemory<'a> {
        slots: &'a [MemSlot],
    }

    impl<'a> GuestMemory<'a> {
        /// Build a DMA view over a slot table. Usually obtained via
        /// [`KvmBackend::guest_memory`]; exposed directly so a slot table can
        /// be wrapped without a live VM (e.g. in tests).
        #[must_use]
        pub fn new(slots: &'a [MemSlot]) -> Self {
            Self { slots }
        }

        /// Host pointer for `[addr, addr + len)` if it lies wholly within one
        /// registered slot, else `None`.
        fn host_ptr(&self, addr: u64, len: usize) -> Option<*mut u8> {
            let len = len as u64;
            for s in self.slots {
                let Some(offset) = addr.checked_sub(s.guest_phys_addr) else {
                    continue;
                };
                if offset.checked_add(len).is_some_and(|end| end <= s.size) {
                    // `host_addr` is page-aligned and `offset < size`, so this
                    // stays within the registered region.
                    return Some((s.host_addr + offset) as *mut u8);
                }
            }
            None
        }
    }

    impl DmaMemory for GuestMemory<'_> {
        fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
            let Some(ptr) = self.host_ptr(addr, buf.len()) else {
                return false;
            };
            // SAFETY: `host_ptr` returned a pointer to `buf.len()` bytes inside
            // a registered region, whose host mapping `map_memory`'s contract
            // guarantees valid for the VM lifetime (which outlives this view).
            unsafe { std::ptr::copy_nonoverlapping(ptr, buf.as_mut_ptr(), buf.len()) };
            true
        }

        fn write(&mut self, addr: u64, data: &[u8]) -> bool {
            let Some(ptr) = self.host_ptr(addr, data.len()) else {
                return false;
            };
            // SAFETY: as `read`, with the copy going into guest memory.
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len()) };
            true
        }
    }

    /// The context-relevant MSRs captured and restored as part of a vCPU state
    /// snapshot ([`KvmVcpuState`]). These are pure architectural registers a
    /// context switch or S3 resume must preserve; `IA32_EFER` already lives in
    /// `kvm_sregs`, and `IA32_TSC` is deliberately *excluded* — the guest TSC is
    /// owned by the timing-stealth layer ([`crate::run_loop::StealthRunLoop`]),
    /// and re-writing it from a snapshot would fight the TSC offset it maintains.
    const CONTEXT_MSRS: [u32; 11] = [
        0xC000_0100, // IA32_FS_BASE
        0xC000_0101, // IA32_GS_BASE
        0xC000_0102, // IA32_KERNEL_GS_BASE
        0xC000_0081, // IA32_STAR
        0xC000_0082, // IA32_LSTAR
        0xC000_0083, // IA32_CSTAR
        0xC000_0084, // IA32_FMASK
        0x0000_0174, // IA32_SYSENTER_CS
        0x0000_0175, // IA32_SYSENTER_ESP
        0x0000_0176, // IA32_SYSENTER_EIP
        0x0000_0277, // IA32_PAT
    ];

    /// A captured snapshot of one vCPU's architectural state — the general
    /// register file, the special/segment/control registers, the XSAVE area
    /// (x87 / SSE / AVX … extended state), and the context-relevant MSRs
    /// ([`CONTEXT_MSRS`]) — enough to pause a vCPU on a scheduling-quantum
    /// expiry (item 2.3) or across an ACPI S3 suspend (item 5.7) and later
    /// resume it exactly. Produced by [`KvmBackend::save_vcpu_state`] and
    /// re-applied by [`KvmBackend::restore_vcpu_state`].
    ///
    /// Not `Clone`/`Debug`: `kvm_xsave` carries a trailing flexible-array member,
    /// so the snapshot is a move-only transient held only long enough to restore.
    pub struct KvmVcpuState {
        /// General-purpose registers, `RIP`, and `RFLAGS` (`KVM_GET_REGS`).
        pub regs: kvm_bindings::kvm_regs,
        /// Segment/control/descriptor-table registers, `EFER`, `APIC_BASE`
        /// (`KVM_GET_SREGS`).
        pub sregs: kvm_bindings::kvm_sregs,
        /// The XSAVE extended-state area (`KVM_GET_XSAVE`): x87, SSE, and any
        /// AVX/AVX-512 state the guest was using.
        pub xsave: kvm_bindings::kvm_xsave,
        /// `(index, value)` for each [`CONTEXT_MSRS`] entry the host KVM serves
        /// (`KVM_GET_MSRS`); unsupported MSRs are omitted.
        pub msrs: Vec<(u32, u64)>,
    }

    /// A live KVM virtual machine plus its vCPUs.
    pub struct KvmBackend {
        kvm: Kvm,
        vm: VmFd,
        vcpus: Vec<VcpuFd>,
        /// The x2APIC ID each vCPU was created with (`apic_ids[i]` is vCPU `i`'s),
        /// the value [`create_vcpu`](Self::create_vcpu) passed to
        /// `KVM_CREATE_VCPU`. Kept so [`apply_topology_stealth`](Self::apply_topology_stealth)
        /// can stamp each vCPU's own initial/x2APIC ID into its CPUID — KVM only
        /// fills those leaves per-vCPU when an in-kernel LAPIC exists, so without
        /// this every vCPU would otherwise report the same APIC ID (an SMP tell)
        /// under `new_without_irqchip` and on the future bare-metal backend.
        apic_ids: Vec<u64>,
        slots: Vec<MemSlot>,
        next_slot: u32,
    }

    impl KvmBackend {
        /// Open `/dev/kvm`, verify the API version, and create a VM with the
        /// in-kernel IRQ chip and the x86 TSS/identity-map scratch regions set
        /// up (required before creating vCPUs on Intel hosts).
        ///
        /// This is the production constructor: the in-kernel IRQ chip
        /// (LAPIC/IOAPIC/PIC) is what real guests need to take interrupts
        /// without a userspace round-trip per IRQ. **Note its effect on
        /// `HLT`:** with the in-kernel local APIC present, `HLT` is handled
        /// *inside* KVM — the vCPU parks waiting for an interrupt and
        /// [`run_vcpu`](Self::run_vcpu) does **not** return
        /// [`GuestExit::Halted`]. Code that wants `HLT` to surface to userspace
        /// (e.g. a "run a blob until it halts" smoke test) must use
        /// [`new_without_irqchip`](Self::new_without_irqchip) instead.
        ///
        /// # Errors
        /// Returns [`Error::HypervisorError`] if KVM is unavailable, the API
        /// version is unexpected, or any setup ioctl fails.
        pub fn new() -> Result<Self> {
            Self::with_irqchip(true)
        }

        /// Like [`new`](Self::new) but **without** the in-kernel IRQ chip.
        ///
        /// Without an in-kernel local APIC, `HLT` exits to userspace as
        /// [`GuestExit::Halted`] (the classic `KVM_EXIT_HLT`). This is the
        /// right choice for running a self-contained code blob that signals
        /// completion by halting, and for any flow that drives interrupts from
        /// userspace. It cannot deliver in-kernel interrupts, so it is not
        /// suitable for booting a full interrupt-driven guest.
        ///
        /// # Errors
        /// As [`new`](Self::new).
        pub fn new_without_irqchip() -> Result<Self> {
            Self::with_irqchip(false)
        }

        /// Shared constructor body. `create_irqchip` selects whether the
        /// in-kernel IRQ chip is created — see [`new`](Self::new) vs
        /// [`new_without_irqchip`](Self::new_without_irqchip) for the
        /// behavioural difference (notably `HLT` handling).
        fn with_irqchip(create_irqchip: bool) -> Result<Self> {
            let kvm =
                Kvm::new().map_err(|e| Error::HypervisorError(format!("open /dev/kvm: {e}")))?;

            let api = kvm.get_api_version();
            if api != 12 {
                return Err(Error::HypervisorError(format!(
                    "unexpected KVM API version {api} (expected 12)"
                )));
            }

            let vm = kvm
                .create_vm()
                .map_err(|e| Error::HypervisorError(format!("KVM_CREATE_VM: {e}")))?;

            // x86 requires a TSS region and an identity map page below 4 GiB
            // before the first vCPU is created. These addresses are the
            // conventional ones used by QEMU/Firecracker.
            vm.set_tss_address(0xfffb_d000)
                .map_err(|e| Error::HypervisorError(format!("KVM_SET_TSS_ADDR: {e}")))?;

            // In-kernel IRQ chip (LAPIC/IOAPIC/PIC) so we can later route
            // interrupts without userspace round-trips. When present it also
            // makes the in-kernel APIC handle `HLT`, so callers that need
            // `HLT` to exit to userspace opt out via `new_without_irqchip`.
            if create_irqchip {
                vm.create_irq_chip()
                    .map_err(|e| Error::HypervisorError(format!("KVM_CREATE_IRQCHIP: {e}")))?;
            }

            Ok(Self {
                kvm,
                vm,
                vcpus: Vec::new(),
                apic_ids: Vec::new(),
                slots: Vec::new(),
                next_slot: 0,
            })
        }

        /// Forward guest MSR accesses KVM does not itself emulate to userspace
        /// as [`GuestExit::MsrRead`] / [`GuestExit::MsrWrite`] exits, routed
        /// through the handler's [`rdmsr`](VmExitHandler::rdmsr) /
        /// [`wrmsr`](VmExitHandler::wrmsr) hooks.
        ///
        /// This enables `KVM_CAP_X86_USER_SPACE_MSR` for the *unknown* and
        /// *filter* reasons. With it on, an `rdmsr`/`wrmsr` of an MSR KVM does
        /// not recognise traps to userspace instead of immediately `#GP`-ing in
        /// the guest — the seam the Phase 5 timing/PMC stealth shadows
        /// (APERF/MPERF, RDPMC, LBR DEBUGCTL) plug into. The `filter` reason is
        /// requested too so a future `KVM_X86_SET_MSR_FILTER` can also redirect
        /// MSRs KVM *does* emulate; until a filter list is installed it has no
        /// effect.
        ///
        /// Call before creating vCPUs.
        ///
        /// # Errors
        /// Returns [`Error::HypervisorError`] if the host lacks
        /// `KVM_CAP_X86_USER_SPACE_MSR` or the enable ioctl fails.
        pub fn enable_userspace_msr_exits(&self) -> Result<()> {
            use kvm_ioctls::{Cap, MsrExitReason};
            if !self.vm.check_extension(Cap::X86UserSpaceMsr) {
                return Err(Error::HypervisorError(
                    "KVM_CAP_X86_USER_SPACE_MSR unavailable".to_string(),
                ));
            }
            let reasons = MsrExitReason::Unknown | MsrExitReason::Filter;
            let cap = kvm_bindings::kvm_enable_cap {
                cap: Cap::X86UserSpaceMsr as u32,
                args: [u64::from(reasons.bits()), 0, 0, 0],
                ..Default::default()
            };
            self.vm
                .enable_cap(&cap)
                .map_err(|e| Error::HypervisorError(format!("enable X86UserSpaceMsr: {e}")))
        }

        /// Redirect the given MSR ranges — even ones KVM itself emulates — to
        /// userspace via `KVM_X86_SET_MSR_FILTER`.
        ///
        /// [`enable_userspace_msr_exits`](Self::enable_userspace_msr_exits)
        /// alone only forwards MSRs KVM does *not* know; the timing/PMC stealth
        /// MSRs (APERF/MPERF, the PMC counters, `IA32_DEBUGCTL`) are KVM-known,
        /// so they need an explicit filter. This installs a **default-allow**
        /// filter (every other MSR keeps its in-kernel behaviour) whose only
        /// ranges *deny* the listed MSRs; with the *filter* exit reason enabled,
        /// a denied access traps to userspace as a `MsrRead`/`MsrWrite` exit
        /// instead of `#GP` — so the [`StealthMsrRouter`] gets to answer it.
        ///
        /// Each `(base, count)` covers `count` consecutive MSRs from `base`. At
        /// most [`KVM_MSR_FILTER_MAX_RANGES`](kvm_bindings::KVM_MSR_FILTER_MAX_RANGES)
        /// (16) ranges are allowed. Call after
        /// `enable_userspace_msr_exits` and before running the vCPUs.
        ///
        /// [`StealthMsrRouter`]: crate::stealth_msr::StealthMsrRouter
        ///
        /// # Errors
        /// Returns [`Error::HypervisorError`] if more than 16 ranges are given
        /// or the ioctl fails.
        pub fn forward_msrs_to_userspace(&self, ranges: &[(u32, u32)]) -> Result<()> {
            use kvm_bindings::{
                kvm_msr_filter, kvm_msr_filter_range, KVM_MSR_FILTER_DEFAULT_ALLOW,
                KVM_MSR_FILTER_MAX_RANGES, KVM_MSR_FILTER_READ, KVM_MSR_FILTER_WRITE,
            };
            if ranges.len() > KVM_MSR_FILTER_MAX_RANGES as usize {
                return Err(Error::HypervisorError(format!(
                    "MSR filter supports at most {KVM_MSR_FILTER_MAX_RANGES} ranges, got {}",
                    ranges.len()
                )));
            }
            // One all-zero bitmap per range: a clear bit denies that MSR (→
            // forward to userspace). The bitmaps must stay alive across the
            // ioctl, so hold them in `bitmaps` until after the call.
            let bitmaps: Vec<Vec<u8>> = ranges
                .iter()
                .map(|&(_, count)| vec![0u8; (count as usize).div_ceil(8)])
                .collect();

            // SAFETY: `kvm_msr_filter` is plain-old-data; an all-zero value is a
            // valid default-allow filter with no ranges.
            let mut filter: kvm_msr_filter = unsafe { std::mem::zeroed() };
            filter.flags = KVM_MSR_FILTER_DEFAULT_ALLOW;
            for (i, (&(base, count), bitmap)) in ranges.iter().zip(&bitmaps).enumerate() {
                filter.ranges[i] = kvm_msr_filter_range {
                    flags: KVM_MSR_FILTER_READ | KVM_MSR_FILTER_WRITE,
                    nmsrs: count,
                    base,
                    bitmap: bitmap.as_ptr().cast_mut(),
                };
            }

            // SAFETY: `filter` is a valid `kvm_msr_filter` and every range's
            // bitmap pointer refers to a live allocation in `bitmaps`, which
            // outlives this call.
            let ret = unsafe { ioctl_with_ref(&self.vm, KVM_X86_SET_MSR_FILTER(), &filter) };
            drop(bitmaps);
            if ret < 0 {
                return Err(Error::HypervisorError(format!(
                    "KVM_X86_SET_MSR_FILTER: {}",
                    std::io::Error::last_os_error()
                )));
            }
            Ok(())
        }

        /// Clear the CPUID **hypervisor-present** tell on every vCPU.
        ///
        /// Starts from KVM's `KVM_GET_SUPPORTED_CPUID`, clears leaf `0x1` ECX
        /// bit 31 (the hypervisor-present bit — the single most-checked VM tell;
        /// no bare-metal CPU sets it), and installs the result on each vCPU via
        /// `KVM_SET_CPUID2`. KVM's supported set does not enumerate the
        /// `0x4000_00xx` hypervisor leaves, so they are absent (an out-of-range
        /// leaf, exactly like bare metal) without extra work.
        ///
        /// This is the Phase 5.3 baseline; the full vendor/brand/topology
        /// rewrite (`enlil_devices::stealth::cpuid::CpuidStealthTable`) is a
        /// later step that must merge into — not exceed — KVM's ≤80-entry set.
        ///
        /// Call after creating vCPUs and before running them.
        ///
        /// # Errors
        /// Returns [`Error::Vcpu`] if querying the supported CPUID or setting it
        /// on a vCPU fails.
        pub fn clear_cpuid_hypervisor_bit(&self) -> Result<()> {
            use kvm_bindings::KVM_MAX_CPUID_ENTRIES;
            let mut cpuid = self
                .kvm
                .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
                .map_err(|e| Error::Vcpu(format!("KVM_GET_SUPPORTED_CPUID: {e}")))?;
            for entry in cpuid.as_mut_slice() {
                if entry.function == 1 {
                    // Leaf 1 ECX bit 31 = hypervisor present.
                    entry.ecx &= !(1u32 << 31);
                }
            }
            for (i, vcpu) in self.vcpus.iter().enumerate() {
                vcpu.set_cpuid2(&cpuid)
                    .map_err(|e| Error::Vcpu(format!("KVM_SET_CPUID2 vcpu {i}: {e}")))?;
            }
            Ok(())
        }

        /// Apply the [`CpuidStealthTable`]'s **topology** view to every vCPU,
        /// so the guest sees *its own* topology rather than the host's.
        ///
        /// KVM's `KVM_GET_SUPPORTED_CPUID` mirrors the host CPU, so its
        /// extended-topology leaf `0xB` and the leaf-`1` `EBX[23:16]`
        /// "max addressable logical-processor IDs" field encode the **host's**
        /// logical-processor count. A guest with fewer vCPUs that reads them
        /// finds a package far larger than the cores it actually has — a VM tell
        /// (and a correctness problem for a guest scheduler counting CPUs from
        /// CPUID). This overrides exactly those topology fields with the table's
        /// guest-derived values (built for the guest's `vcpu_count` /
        /// `threads_per_core`) — plus, for AMD, the leaf-`0x8000_0008` `ECX`
        /// core-count (`NC`) / APIC-ID-width field the kernel cross-checks
        /// against them, the per-cache sharing counts in leaf `0x8000_001D`
        /// `EAX[25:14]` (so the guest sees its L3 shared by its own vCPUs, not
        /// the host's), and the SMT width in leaf `0x8000_001E` `EBX[15:8]`
        /// (which KVM defaults to 0/no-SMT) — leaving every other supported leaf
        /// (and those leaves' host-backed sizes / per-vCPU APIC-ID fields)
        /// untouched. It also clears the leaf-`1`
        /// `ECX[31]` hypervisor bit, so it subsumes
        /// [`clear_cpuid_hypervisor_bit`](Self::clear_cpuid_hypervisor_bit)
        /// when a table is available, and folds in the architectural-PMU leaf
        /// `0xA` (see [`apply_pmu_stealth`](Self::apply_pmu_stealth)) in the same
        /// rebuild — so this single call is the **full CPUID-stealth** path
        /// (topology + hypervisor bit + PMU). The PMU fold is a no-op for an
        /// AMD-vendor table and effective for an Intel-presented one.
        ///
        /// The per-vCPU APIC identity (leaf `1` `EBX[31:24]` initial APIC ID,
        /// leaf `0xB`/`0x1F` `EDX` x2APIC ID, and on AMD leaf `0x8000_001E`
        /// `EAX` extended APIC ID + `EBX[7:0]` core ID) is stamped from each
        /// vCPU's own creation id ([`create_vcpu`](Self::create_vcpu)), **not**
        /// left to KVM: KVM only fills those leaves per vCPU when an in-kernel
        /// LAPIC is present, so without this every vCPU under
        /// `new_without_irqchip` (and on the bare-metal backend) would report
        /// the same APIC ID — an SMP tell.
        ///
        /// Call after creating vCPUs and before running them.
        ///
        /// # Errors
        /// Returns [`Error::Vcpu`] if querying the supported CPUID or setting it
        /// on a vCPU fails.
        ///
        /// [`CpuidStealthTable`]: enlil_devices::stealth::cpuid::CpuidStealthTable
        pub fn apply_topology_stealth(
            &self,
            table: &enlil_devices::stealth::cpuid::CpuidStealthTable,
        ) -> Result<()> {
            use kvm_bindings::{
                kvm_cpuid_entry2, CpuId, KVM_CPUID_FLAG_SIGNIFCANT_INDEX, KVM_MAX_CPUID_ENTRIES,
            };
            // EBX[23:16]: max addressable logical-processor IDs in the package.
            const MAX_IDS_MASK: u32 = 0x00FF_0000;
            // build_topology_leaves always emits subleaves 0 (SMT), 1 (core),
            // 2 (terminator) — the full leaf-0xB enumeration.
            const TOPOLOGY_SUBLEAVES: u32 = 3;

            let supported = self
                .kvm
                .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
                .map_err(|e| Error::Vcpu(format!("KVM_GET_SUPPORTED_CPUID: {e}")))?;

            // Copy the supported set out so we can both patch existing entries
            // and *add* the topology leaves KVM may not enumerate at all — AMD
            // hosts often omit the Intel-style extended-topology leaf 0xB from
            // the supported set, so an in-place patch would have nothing to
            // rewrite and the guest would read leaf 0xB as all-zero.
            let mut entries: Vec<kvm_cpuid_entry2> = supported.as_slice().to_vec();

            for entry in &mut entries {
                if entry.function == 1 {
                    // Clear the hypervisor-present tell and align the package
                    // width with the guest topology (leaf 0xB below).
                    entry.ecx &= !(1u32 << 31);
                    let table_ebx = table.lookup(1, 0).ebx;
                    entry.ebx = (entry.ebx & !MAX_IDS_MASK) | (table_ebx & MAX_IDS_MASK);
                }
            }

            // Overwrite or insert each leaf-0xB subleaf with the guest topology.
            // EDX (the per-vCPU x2APIC ID) is stamped per vCPU in the set loop
            // below; the shared template carries a placeholder 0 here.
            for sub in 0..TOPOLOGY_SUBLEAVES {
                let r = table.lookup(0xB, sub);
                if let Some(entry) = entries
                    .iter_mut()
                    .find(|e| e.function == 0xB && e.index == sub)
                {
                    entry.eax = r.eax;
                    entry.ebx = r.ebx;
                    entry.ecx = r.ecx;
                    entry.flags |= KVM_CPUID_FLAG_SIGNIFCANT_INDEX;
                } else {
                    entries.push(kvm_cpuid_entry2 {
                        function: 0xB,
                        index: sub,
                        flags: KVM_CPUID_FLAG_SIGNIFCANT_INDEX,
                        eax: r.eax,
                        ebx: r.ebx,
                        ecx: r.ecx,
                        edx: 0,
                        padding: [0; 3],
                    });
                }
            }

            // AMD encodes the package's core count in leaf 0x8000_0008 ECX[7:0]
            // (NC = cores-1) and the APIC-ID width in ECX[15:12]; the kernel
            // cross-checks these against leaf-1 EBX[23:16] and leaf 0xB. KVM
            // mirrors the host there, so without this an AMD guest's
            // 0x8000_0008.ECX would still report the host's core count and
            // *contradict* the guest topology just installed in leaf 1 / 0xB — a
            // one-instruction cross-check tell. Patch ECX from the table (the
            // host-backed EAX/EBX/EDX — address sizes and feature bits — are left
            // intact); reserved-zero for an Intel table, so a no-op there. Only
            // patch when KVM enumerates the leaf (it always does on x86_64); never
            // synthesise it, since EAX carries the real physical-address width.
            let ext8_ecx = table.lookup(0x8000_0008, 0).ecx;
            if let Some(entry) = entries.iter_mut().find(|e| e.function == 0x8000_0008) {
                entry.ecx = ext8_ecx;
            }

            // AMD leaf 0x8000_001D encodes, per cache, how many logical
            // processors share it (EAX[25:14] = NumSharingCache-1). KVM mirrors
            // the host, so a guest with fewer vCPUs reads e.g. the host's
            // package-wide L3 as shared by every host thread — a cache-topology
            // tell that also contradicts the core count just fixed in
            // 0x8000_0008 / leaf 0xB. Rewrite *only* the sharing sub-field of
            // each cache from the table (which derives it from the guest
            // topology: L1/L2 per core, L3 package-wide), matching KVM's subleaf
            // by index and cache type+level so the host-real cache sizes
            // (EBX/ECX) stay intact. No-op when KVM does not enumerate the leaf
            // (older hosts / no TOPOEXT pass-through) and for an Intel table
            // (which carries no 0x8000_001D; Intel uses leaf 4).
            const CACHE_SHARING_MASK: u32 = 0x03FF_C000; // EAX[25:14]
            const CACHE_TYPE_LEVEL_MASK: u32 = 0x0000_00FF; // EAX[7:0]: type + level
            for sub in 0..16u32 {
                let t = table.lookup(0x8000_001D, sub);
                if t.eax & 0x1F == 0 {
                    break; // null cache type terminates the table's enumeration
                }
                if let Some(entry) = entries.iter_mut().find(|e| {
                    e.function == 0x8000_001D
                        && e.index == sub
                        && (e.eax & CACHE_TYPE_LEVEL_MASK) == (t.eax & CACHE_TYPE_LEVEL_MASK)
                }) {
                    entry.eax = (entry.eax & !CACHE_SHARING_MASK) | (t.eax & CACHE_SHARING_MASK);
                }
            }

            // AMD leaf 0x8000_001E EBX[15:8] is ThreadsPerComputeUnit-1 (SMT
            // width); KVM mirrors the host, so an SMT-1 guest on an SMT-2 host
            // would read 1 here and contradict the single-thread topology in leaf
            // 0xB / leaf-1 EBX. Patch that sub-field from the table here in the
            // shared template (ECX node id is left to KVM). The per-vCPU EAX
            // (extended APIC id) and EBX[7:0] (core/compute-unit id) are stamped
            // per vCPU in the set loop below — KVM only fills them per vCPU with
            // an in-kernel LAPIC, the same gap that bit leaf 0xB EDX.
            // Reserved for an Intel table (no 0x8000_001E), so a no-op there.
            const SMT_WIDTH_MASK: u32 = 0x0000_FF00; // EBX[15:8]
            let ext1e_ebx = table.lookup(0x8000_001E, 0).ebx;
            if let Some(entry) = entries.iter_mut().find(|e| e.function == 0x8000_001E) {
                entry.ebx = (entry.ebx & !SMT_WIDTH_MASK) | (ext1e_ebx & SMT_WIDTH_MASK);
            }

            // Fold in the architectural-PMU leaf (0xA) in the same single rebuild,
            // so this one call yields full CPUID stealth (topology + hypervisor
            // bit + PMU) and the guest never reads a leaf-0xA "PMU version 0" tell
            // that contradicts the RDPMC shadow. No-op for an AMD-vendor table
            // (leaf 0xA reserved-zero there), effective for an Intel-presented one.
            Self::upsert_pmu_leaf(&mut entries, table);

            // Fold in the Intel TSC/processor-frequency leaves (0x15/0x16) in the
            // same rebuild, pinned to the measured effective guest TSC rate when
            // KVM can report it, so an Intel-presented guest reads a
            // self-consistent frequency instead of an in-range zero / a stale
            // host-passthrough rate. No-op for an AMD-vendor table.
            Self::upsert_frequency_leaves(&mut entries, table, self.tsc_khz().ok());

            // Fold in the CPU **identity** leaves (leaf-0 vendor string, leaf-1
            // FMS, brand 0x8000_0002-4) from the table in the same rebuild, so the
            // identity the guest reads matches the topology/PMU/frequency already
            // installed from that table. KVM passes the *host's* identity through
            // its supported set, so without this a masqueraded table (a different
            // model presented uniformly across a pool, or the future bare-metal
            // backend which has no KVM passthrough to inherit from) would install a
            // guest topology while the vendor/brand still read the host's — a
            // stealth-surface disagreement. For the live `from_host` table this
            // reinstalls the true host identity (no observable change), but it
            // closes that consistency gap.
            Self::upsert_identity_leaves(&mut entries, table);

            // Per-vCPU APIC identity. Several CPUID fields are the *current*
            // logical CPU's ID — distinct per vCPU on real hardware — yet KVM
            // only fills them per vCPU when an in-kernel LAPIC exists; without
            // one (this run loop's `new_without_irqchip`, and the future
            // bare-metal backend) every vCPU reads the *same* placeholder, so a
            // detector comparing the APIC ID across two vCPUs finds them
            // identical — a blatant SMP tell. Stamp each vCPU's own
            // `apic_ids[i]` into its CPUID before setting it, so the identity is
            // correct regardless of irqchip:
            //   - leaf 1 EBX[31:24]    initial (xAPIC) ID
            //   - leaf 0xB/0x1F EDX    x2APIC ID
            //   - leaf 0x8000_001E EAX extended APIC ID (AMD)
            //     and EBX[7:0]         core / compute-unit ID (AMD)
            // The AMD core ID is the APIC ID with the SMT (thread) bits shifted
            // out; leaf 0xB subleaf 0 EAX carries that shift width.
            let smt_shift = table.lookup(0xB, 0).eax & 0x1F;
            for (i, vcpu) in self.vcpus.iter().enumerate() {
                let apic_id = self.apic_ids.get(i).copied().unwrap_or(i as u64) as u32;
                let mut per_vcpu = entries.clone();
                Self::stamp_apic_identity(&mut per_vcpu, apic_id, smt_shift);
                let cpuid = CpuId::from_entries(&per_vcpu)
                    .map_err(|e| Error::Vcpu(format!("rebuild CpuId: {e:?}")))?;
                vcpu.set_cpuid2(&cpuid)
                    .map_err(|e| Error::Vcpu(format!("KVM_SET_CPUID2 vcpu {i}: {e}")))?;
            }
            Ok(())
        }

        /// Stamp a single vCPU's APIC identity into its CPUID `entries` in place:
        /// leaf 1 `EBX[31:24]` initial (xAPIC) ID, leaf `0xB`/`0x1F` `EDX` x2APIC
        /// ID, and AMD leaf `0x8000_001E` `EAX` extended APIC ID + `EBX[7:0]`
        /// core/compute-unit ID (the APIC ID with the SMT thread bits shifted
        /// out by `smt_shift`). Pure over `entries` so it is unit-testable with a
        /// synthetic supported-CPUID set (covering leaves an AMD host does not
        /// expose, e.g. `0x1F`); see [`apply_topology_stealth`] for why KVM
        /// cannot be trusted to fill these per vCPU.
        ///
        /// [`apply_topology_stealth`]: Self::apply_topology_stealth
        pub(crate) fn stamp_apic_identity(
            entries: &mut [kvm_bindings::kvm_cpuid_entry2],
            apic_id: u32,
            smt_shift: u32,
        ) {
            const INITIAL_APIC_ID_MASK: u32 = 0xFF00_0000; // leaf 1 EBX[31:24]
            const CORE_ID_MASK: u32 = 0x0000_00FF; // leaf 0x8000_001E EBX[7:0]
            let core_id = apic_id >> smt_shift;
            for entry in entries {
                match entry.function {
                    1 => {
                        entry.ebx = (entry.ebx & !INITIAL_APIC_ID_MASK) | (apic_id << 24);
                    }
                    0xB | 0x1F => entry.edx = apic_id,
                    0x8000_001E => {
                        entry.eax = apic_id;
                        entry.ebx = (entry.ebx & !CORE_ID_MASK) | (core_id & CORE_ID_MASK);
                    }
                    _ => {}
                }
            }
        }

        /// Apply the [`CpuidStealthTable`]'s **architectural-PMU** view (leaf
        /// `0xA`) to every vCPU, so the guest enumerates a performance-monitoring
        /// unit consistent with the RDPMC shadow it is being served.
        ///
        /// KVM does not synthesise leaf `0xA` from a virtual PMU model: on this
        /// AMD host `KVM_GET_SUPPORTED_CPUID` omits the Intel-style leaf entirely,
        /// so a guest reads it back as all-zero — **PMU version 0, "no
        /// architectural PMU."** That is itself a cloud/VM tell (only vPMU-less
        /// VMs report it) and it contradicts the `stealth::pmc` shadow that
        /// services the guest's RDPMC: a guest that finds counters via RDPMC but
        /// is told "version 0" by CPUID has caught the hypervisor. This installs
        /// exactly the table's leaf-`0xA` value (PMU version + GP/fixed counter
        /// counts and widths matching the shadow) so the two surfaces agree.
        ///
        /// For an AMD-vendor table the leaf is reserved-zero (correct for AMD,
        /// where the PMU is enumerated via leaf `0x8000_0022` + MSRs, not `0xA`),
        /// so this is a no-op there; it has effect for an Intel-presented guest.
        /// Like [`apply_topology_stealth`](Self::apply_topology_stealth) it
        /// *inserts* the leaf when KVM's supported set lacks it, rebuilding the
        /// CPUID array via `CpuId::from_entries`, and also clears the leaf-`1`
        /// `ECX[31]` hypervisor-present bit — the universal CPUID-stealth baseline
        /// — so a standalone PMU install never leaves the single biggest tell set
        /// (a guest that advertises a real PMU but still flags itself a
        /// hypervisor is self-contradicting). It does **not** rewrite topology.
        /// Because each CPUID-stealth installer re-derives from KVM's supported
        /// baseline before `KVM_SET_CPUID2`, this is an **alternative** full
        /// install, not a layer to chain after `apply_topology_stealth` —
        /// chaining would have whichever runs last drop the other's edits. Use
        /// this when only PMU stealth is wanted; for topology *and* PMU together
        /// call [`apply_topology_stealth`](Self::apply_topology_stealth), which
        /// folds this leaf into its single rebuild.
        ///
        /// Call after creating vCPUs and before running them.
        ///
        /// # Errors
        /// Returns [`Error::Vcpu`] if querying the supported CPUID or setting it
        /// on a vCPU fails.
        ///
        /// [`CpuidStealthTable`]: enlil_devices::stealth::cpuid::CpuidStealthTable
        pub fn apply_pmu_stealth(
            &self,
            table: &enlil_devices::stealth::cpuid::CpuidStealthTable,
        ) -> Result<()> {
            use kvm_bindings::{kvm_cpuid_entry2, CpuId, KVM_MAX_CPUID_ENTRIES};

            let supported = self
                .kvm
                .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
                .map_err(|e| Error::Vcpu(format!("KVM_GET_SUPPORTED_CPUID: {e}")))?;
            let mut entries: Vec<kvm_cpuid_entry2> = supported.as_slice().to_vec();

            // Clear the leaf-1 ECX[31] hypervisor-present bit so a standalone PMU
            // install is still hv-bit-safe (the universal baseline the other
            // CPUID installers also apply).
            for entry in &mut entries {
                if entry.function == 1 {
                    entry.ecx &= !(1u32 << 31);
                }
            }
            Self::upsert_pmu_leaf(&mut entries, table);

            let cpuid = CpuId::from_entries(&entries)
                .map_err(|e| Error::Vcpu(format!("rebuild CpuId: {e:?}")))?;
            for (i, vcpu) in self.vcpus.iter().enumerate() {
                vcpu.set_cpuid2(&cpuid)
                    .map_err(|e| Error::Vcpu(format!("KVM_SET_CPUID2 vcpu {i}: {e}")))?;
            }
            Ok(())
        }

        /// Overwrite (or insert) leaf `0xA` in `entries` with the table's
        /// architectural-PMU view. Leaf `0xA` is non-indexed (single subleaf 0).
        /// Shared by [`apply_pmu_stealth`](Self::apply_pmu_stealth) and
        /// [`apply_topology_stealth`](Self::apply_topology_stealth) so both reach
        /// the guest through one consistent leaf-`0xA` rewrite.
        fn upsert_pmu_leaf(
            entries: &mut Vec<kvm_bindings::kvm_cpuid_entry2>,
            table: &enlil_devices::stealth::cpuid::CpuidStealthTable,
        ) {
            let r = table.lookup(0xA, 0);
            if let Some(entry) = entries.iter_mut().find(|e| e.function == 0xA) {
                entry.index = 0;
                entry.eax = r.eax;
                entry.ebx = r.ebx;
                entry.ecx = r.ecx;
                entry.edx = r.edx;
            } else {
                entries.push(kvm_bindings::kvm_cpuid_entry2 {
                    function: 0xA,
                    index: 0,
                    flags: 0,
                    eax: r.eax,
                    ebx: r.ebx,
                    ecx: r.ecx,
                    edx: r.edx,
                    padding: [0; 3],
                });
            }
        }

        /// Upsert the Intel TSC/processor-frequency leaves (`0x15`, `0x16`) into
        /// `entries` from the stealth `table`.
        ///
        /// [`apply_topology_stealth`](Self::apply_topology_stealth) rebuilds CPUID
        /// from KVM's supported set, which on a non-Intel host omits these leaves
        /// entirely (and even on an Intel host passes through the *host's* base
        /// frequency rather than the rate the guest's TSC actually runs at).
        /// Without this an Intel-presented guest reads leaf `0x15` as "no TSC rate
        /// enumerated" and falls back to noisy PIT/HPET calibration whose result
        /// then has to agree with our virtual timers — a calibration tell.
        ///
        /// When `measured_tsc_khz` is `Some` (from `KVM_GET_TSC_KHZ`) the
        /// enumerated base frequency is pinned to the *effective guest* TSC rate so
        /// the frequency a guest derives from leaf `0x15` (`crystal × EBX/EAX`)
        /// equals the rate its RDTSC observes. enlil offsets the guest TSC (its
        /// start value) but does not scale its rate, so this measured kHz is stable
        /// across the run; the table keeps a 24 MHz crystal in ECX and `EAX = 24`
        /// so `EBX = base_mhz` makes the derived TSC exactly `base_mhz × 1e6`.
        ///
        /// No-op for an AMD-vendor table: AMD does not define `0x15`/`0x16`, so the
        /// table leaves them out-of-range reserved-zero (leaf `0x16` EAX `== 0`),
        /// matching bare metal — synthesising them would itself be an Intel tell on
        /// an AMD guest. Like the other CPUID upserts, single-subleaf (index 0).
        /// `pub(crate)` so it is unit-testable with a synthetic entry set, like
        /// [`stamp_apic_identity`](Self::stamp_apic_identity).
        pub(crate) fn upsert_frequency_leaves(
            entries: &mut Vec<kvm_bindings::kvm_cpuid_entry2>,
            table: &enlil_devices::stealth::cpuid::CpuidStealthTable,
            measured_tsc_khz: Option<u32>,
        ) {
            let mut r15 = table.lookup(0x15, 0);
            let mut r16 = table.lookup(0x16, 0);
            // AMD-vendor table: 0x15/0x16 are reserved-zero / out of range. Leave
            // them so an AMD guest stays consistent with bare metal.
            if r16.eax == 0 {
                return;
            }
            if let Some(khz) = measured_tsc_khz {
                let base_mhz = (khz + 500) / 1000; // kHz → MHz, round to nearest
                if base_mhz != 0 {
                    // Keep the table's 24 MHz crystal (ECX) and EAX denominator so
                    // TSC = crystal × EBX/EAX = base_mhz × 1e6 with EBX = base_mhz.
                    r15.ebx = base_mhz;
                    r16.eax = base_mhz;
                    // Max-turbo (leaf 0x16 EBX) must never read below base.
                    if r16.ebx < base_mhz {
                        r16.ebx = base_mhz;
                    }
                }
            }
            for (function, r) in [(0x15u32, r15), (0x16u32, r16)] {
                if let Some(entry) = entries.iter_mut().find(|e| e.function == function) {
                    entry.index = 0;
                    entry.eax = r.eax;
                    entry.ebx = r.ebx;
                    entry.ecx = r.ecx;
                    entry.edx = r.edx;
                } else {
                    entries.push(kvm_bindings::kvm_cpuid_entry2 {
                        function,
                        index: 0,
                        flags: 0,
                        eax: r.eax,
                        ebx: r.ebx,
                        ecx: r.ecx,
                        edx: r.edx,
                        padding: [0; 3],
                    });
                }
            }

            // The 0x15/0x16 leaves are only reachable if leaf 0's max-basic-leaf
            // (EAX) advertises them: a guest reads leaf 0 first and queries only
            // leaves with function <= that EAX. KVM's host leaf-0 EAX can be below
            // 0x16 (e.g. an AMD host whose basic range ends at 0x10/0x0D), so an
            // Intel-presented guest there would install 0x16 yet never read it.
            // Raise leaf 0 EAX to cover the highest leaf we just inserted; never
            // lower it (legitimately-higher leaves like 0x1F must survive).
            const HIGHEST_FREQ_LEAF: u32 = 0x16;
            if let Some(leaf0) = entries.iter_mut().find(|e| e.function == 0) {
                if leaf0.eax < HIGHEST_FREQ_LEAF {
                    leaf0.eax = HIGHEST_FREQ_LEAF;
                }
            }
        }

        /// Install the table's CPU **identity** leaves into `entries`: the
        /// leaf-`0` vendor string (`EBX`/`ECX`/`EDX`), the leaf-`1` `EAX`
        /// family/model/stepping, and the brand string in leaves
        /// `0x8000_0002`..=`0x8000_0004`.
        ///
        /// KVM's `KVM_GET_SUPPORTED_CPUID` mirrors the **host's** identity, so a
        /// [`CpuidStealthTable`] presenting a different identity (a masqueraded
        /// model unified across a heterogeneous pool, or the future bare-metal
        /// backend that has no supported-set to inherit) would otherwise install a
        /// guest topology/PMU/frequency view while the vendor and brand still read
        /// the host's — a self-inconsistent stealth surface. This makes the
        /// identity the guest reads agree with the rest of the table.
        ///
        /// Only the **identity** fields are touched: leaf-`0` `EAX`
        /// (max-basic-leaf, managed by [`upsert_frequency_leaves`] and by KVM's
        /// real leaf enumeration) and leaf-`1` `EBX`/`ECX`/`EDX` (APIC/max-IDs and
        /// the host-real feature bits, which must not be widened past what the
        /// physical CPU supports) are left exactly as-is. The brand leaves are
        /// upserted (KVM enumerates them on x86_64, but synthesise them if absent
        /// so the bare-metal path is covered too). `pub(crate)` so it is
        /// unit-testable with a synthetic entry set, like
        /// [`upsert_frequency_leaves`](Self::upsert_frequency_leaves).
        ///
        /// [`upsert_frequency_leaves`]: Self::upsert_frequency_leaves
        /// [`CpuidStealthTable`]: enlil_devices::stealth::cpuid::CpuidStealthTable
        pub(crate) fn upsert_identity_leaves(
            entries: &mut Vec<kvm_bindings::kvm_cpuid_entry2>,
            table: &enlil_devices::stealth::cpuid::CpuidStealthTable,
        ) {
            // Leaf 0: vendor string in EBX/EDX/ECX. Leave EAX (max-basic-leaf)
            // untouched — lowering it would hide leaves KVM actually enumerates.
            let l0 = table.lookup(0, 0);
            if let Some(entry) = entries.iter_mut().find(|e| e.function == 0) {
                entry.ebx = l0.ebx;
                entry.ecx = l0.ecx;
                entry.edx = l0.edx;
            }

            // Leaf 1: family/model/stepping in EAX only. EBX carries the APIC ID /
            // max-addressable-IDs field patched elsewhere, and ECX/EDX are the
            // host-real feature bits — none of which this rewrite may disturb.
            let l1 = table.lookup(1, 0);
            if let Some(entry) = entries.iter_mut().find(|e| e.function == 1) {
                entry.eax = l1.eax;
            }

            // Brand string: leaves 0x8000_0002..=0x8000_0004, four registers each.
            for i in 0..3u32 {
                let function = 0x8000_0002 + i;
                let r = table.lookup(function, 0);
                if let Some(entry) = entries.iter_mut().find(|e| e.function == function) {
                    entry.index = 0;
                    entry.eax = r.eax;
                    entry.ebx = r.ebx;
                    entry.ecx = r.ecx;
                    entry.edx = r.edx;
                } else {
                    entries.push(kvm_bindings::kvm_cpuid_entry2 {
                        function,
                        index: 0,
                        flags: 0,
                        eax: r.eax,
                        ebx: r.ebx,
                        ecx: r.ecx,
                        edx: r.edx,
                        padding: [0; 3],
                    });
                }
            }
        }

        /// Map a host buffer into the guest's physical address space.
        ///
        /// `host_addr` must point to at least `size` bytes of memory that
        /// outlives this VM (typically an mmap'd region owned by the caller).
        ///
        /// # Safety
        /// The caller guarantees `[host_addr, host_addr + size)` is a valid,
        /// stable host virtual mapping for the lifetime of the VM. KVM reads
        /// and writes through it directly.
        ///
        /// # Errors
        /// Returns [`Error::HypervisorError`] if `KVM_SET_USER_MEMORY_REGION`
        /// fails.
        pub unsafe fn map_memory(
            &mut self,
            guest_phys_addr: u64,
            host_addr: u64,
            size: u64,
        ) -> Result<MemSlot> {
            validate_region(guest_phys_addr, host_addr, size)?;
            let slot = self.next_slot;
            let region = kvm_userspace_memory_region {
                slot,
                flags: 0,
                guest_phys_addr,
                memory_size: size,
                userspace_addr: host_addr,
            };
            // SAFETY: forwarded from this function's safety contract.
            unsafe {
                self.vm.set_user_memory_region(region).map_err(|e| {
                    Error::HypervisorError(format!("KVM_SET_USER_MEMORY_REGION: {e}"))
                })?;
            }
            self.next_slot += 1;
            let entry = MemSlot {
                slot,
                guest_phys_addr,
                size,
                host_addr,
            };
            self.slots.push(entry);
            Ok(entry)
        }

        /// Create a vCPU with the given APIC id and return its index.
        ///
        /// # Errors
        /// Returns [`Error::Vcpu`] if `KVM_CREATE_VCPU` fails.
        pub fn create_vcpu(&mut self, id: u64) -> Result<usize> {
            let vcpu = self
                .vm
                .create_vcpu(id)
                .map_err(|e| Error::Vcpu(format!("KVM_CREATE_VCPU {id}: {e}")))?;
            self.vcpus.push(vcpu);
            self.apic_ids.push(id);
            Ok(self.vcpus.len() - 1)
        }

        /// Number of vCPUs created.
        #[must_use]
        pub fn vcpu_count(&self) -> usize {
            self.vcpus.len()
        }

        /// The host TSC frequency in kHz that KVM reports for vCPU 0
        /// (`KVM_GET_TSC_KHZ`) — the reference rate the guest's TSC runs at.
        ///
        /// This is the missing primitive for two things the platform currently
        /// hard-codes or omits: advancing the platform timers in lockstep with
        /// *guest execution time* (converting the per-entry guest reference-cycle
        /// delta from [`run_vcpu_timed`](Self::run_vcpu_timed) to nanoseconds),
        /// and advertising the core-crystal / TSC frequency through CPUID leaves
        /// `0x15`/`0x16` so a guest reads a self-consistent rate. Reads vCPU 0,
        /// so at least one vCPU must already exist.
        ///
        /// # Errors
        /// Returns [`Error::Vcpu`] if no vCPU has been created or
        /// `KVM_GET_TSC_KHZ` is unavailable on the host.
        pub fn tsc_khz(&self) -> Result<u32> {
            let vcpu = self
                .vcpus
                .first()
                .ok_or_else(|| Error::Vcpu("no vcpu created; cannot read TSC frequency".into()))?;
            vcpu.get_tsc_khz()
                .map_err(|e| Error::Vcpu(format!("KVM_GET_TSC_KHZ: {e}")))
        }

        /// The guest's current Time-Stamp Counter (`IA32_TSC`, MSR `0x10`) on
        /// vCPU `index`, read via `KVM_GET_MSRS`.
        ///
        /// This is the guest-visible TSC the LAPIC TSC-deadline timer is armed
        /// against: a guest arms a one-shot interrupt by writing an *absolute*
        /// TSC value to `IA32_TSC_DEADLINE`, so to fire it the run loop must
        /// compare against the guest TSC — not the host TSC or an ns delta. Read
        /// it after a guest entry and hand it to
        /// [`StandardPc::check_lapic_tsc_deadlines`](crate::device_bus::StandardPc::check_lapic_tsc_deadlines).
        ///
        /// # Errors
        /// Returns [`Error::Vcpu`] if `index` names no vCPU, `KVM_GET_MSRS`
        /// fails, or it does not return the single requested entry.
        pub fn read_guest_tsc(&self, index: usize) -> Result<u64> {
            use kvm_bindings::{kvm_msr_entry, Msrs};
            let vcpu = self
                .vcpus
                .get(index)
                .ok_or_else(|| Error::Vcpu(format!("no vcpu at index {index}")))?;
            // IA32_TSC is MSR 0x10.
            let mut msrs = Msrs::from_entries(&[kvm_msr_entry {
                index: 0x10,
                ..Default::default()
            }])
            .map_err(|e| Error::Vcpu(format!("Msrs alloc: {e:?}")))?;
            let n = vcpu
                .get_msrs(&mut msrs)
                .map_err(|e| Error::Vcpu(format!("KVM_GET_MSRS(IA32_TSC): {e}")))?;
            if n != 1 {
                return Err(Error::Vcpu(format!(
                    "KVM_GET_MSRS(IA32_TSC) returned {n} entries, expected 1"
                )));
            }
            Ok(msrs.as_slice()[0].data)
        }

        /// Capture vCPU `index`'s full architectural state into a
        /// [`KvmVcpuState`] — general registers, special/segment/control
        /// registers, the XSAVE extended state, and the context-relevant MSRs
        /// ([`CONTEXT_MSRS`]) — via `KVM_GET_{REGS,SREGS,XSAVE,MSRS}`.
        ///
        /// This is the state-save primitive the time-slice scheduler needs to
        /// pause a vCPU on quantum expiry (item 2.3) and the ACPI S3 path needs
        /// to snapshot a guest across suspend (item 5.7); pair it with
        /// [`restore_vcpu_state`](Self::restore_vcpu_state).
        ///
        /// The vCPU's CPUID must already be configured (as the run loop does via
        /// the CPUID-stealth install before entry): the long-mode MSRs in
        /// [`CONTEXT_MSRS`] (FS/GS base, the SYSCALL MSRs) are gated on the guest
        /// advertising long mode, so `KVM_GET_MSRS` on a bare, CPUID-less vCPU
        /// would report them missing.
        ///
        /// # Errors
        /// Returns [`Error::Vcpu`] if `index` names no vCPU or the
        /// `KVM_GET_{REGS,SREGS,XSAVE}` ioctls fail. Individual MSRs the host
        /// KVM does not serve are skipped, not treated as errors.
        pub fn save_vcpu_state(&self, index: usize) -> Result<KvmVcpuState> {
            use kvm_bindings::{kvm_msr_entry, Msrs};
            let vcpu = self
                .vcpus
                .get(index)
                .ok_or_else(|| Error::Vcpu(format!("no vcpu at index {index}")))?;
            let regs = vcpu
                .get_regs()
                .map_err(|e| Error::Vcpu(format!("KVM_GET_REGS: {e}")))?;
            let sregs = vcpu
                .get_sregs()
                .map_err(|e| Error::Vcpu(format!("KVM_GET_SREGS: {e}")))?;
            let xsave = vcpu
                .get_xsave()
                .map_err(|e| Error::Vcpu(format!("KVM_GET_XSAVE: {e}")))?;
            // Read each context MSR individually and keep the subset this host's
            // KVM actually exposes. KVM_GET_MSRS processes a batch in order and
            // stops at the first MSR it does not serve (returning only the count
            // read before it), so a single host-unsupported MSR would truncate a
            // batched read; per-MSR reads let us skip the unsupported ones. An MSR
            // KVM cannot read is not part of the guest's architectural state on
            // this host, so excluding it loses nothing to restore.
            let mut saved_msrs = Vec::with_capacity(CONTEXT_MSRS.len());
            for &index in &CONTEXT_MSRS {
                let mut one = Msrs::from_entries(&[kvm_msr_entry {
                    index,
                    ..Default::default()
                }])
                .map_err(|e| Error::Vcpu(format!("Msrs alloc: {e:?}")))?;
                if let Ok(1) = vcpu.get_msrs(&mut one) {
                    saved_msrs.push((index, one.as_slice()[0].data));
                }
            }
            Ok(KvmVcpuState {
                regs,
                sregs,
                xsave,
                msrs: saved_msrs,
            })
        }

        /// Re-apply a [`KvmVcpuState`] captured by
        /// [`save_vcpu_state`](Self::save_vcpu_state) onto vCPU `index`, restoring
        /// it to the exact point it was paused (item 2.3 resume / item 5.7 S3
        /// resume). Sets the special registers before the general registers (the
        /// order the `prepare_*_vcpu` helpers use so segment/control state is in
        /// place before `RIP`/`RFLAGS`), then the XSAVE area and the MSRs.
        ///
        /// # Errors
        /// Returns [`Error::Vcpu`] if `index` names no vCPU, any of the
        /// `KVM_SET_*` ioctls fail, or `KVM_SET_MSRS` does not accept every
        /// snapshot MSR.
        pub fn restore_vcpu_state(&self, index: usize, state: &KvmVcpuState) -> Result<()> {
            use kvm_bindings::{kvm_msr_entry, Msrs};
            let vcpu = self
                .vcpus
                .get(index)
                .ok_or_else(|| Error::Vcpu(format!("no vcpu at index {index}")))?;
            vcpu.set_sregs(&state.sregs)
                .map_err(|e| Error::Vcpu(format!("KVM_SET_SREGS: {e}")))?;
            vcpu.set_regs(&state.regs)
                .map_err(|e| Error::Vcpu(format!("KVM_SET_REGS: {e}")))?;
            vcpu.set_xsave(&state.xsave)
                .map_err(|e| Error::Vcpu(format!("KVM_SET_XSAVE: {e}")))?;
            let entries: Vec<kvm_msr_entry> = state
                .msrs
                .iter()
                .map(|&(index, data)| kvm_msr_entry {
                    index,
                    data,
                    ..Default::default()
                })
                .collect();
            let msrs = Msrs::from_entries(&entries)
                .map_err(|e| Error::Vcpu(format!("Msrs alloc: {e:?}")))?;
            let n = vcpu
                .set_msrs(&msrs)
                .map_err(|e| Error::Vcpu(format!("KVM_SET_MSRS: {e}")))?;
            if n != state.msrs.len() {
                return Err(Error::Vcpu(format!(
                    "KVM_SET_MSRS accepted {n} of {} entries",
                    state.msrs.len()
                )));
            }
            Ok(())
        }

        /// Point vCPU `index` at a flat 16-bit real-mode entry: every segment
        /// gets base 0 (so `rip` is a direct guest-physical offset), `rip` is
        /// set to `entry`, and `rflags` to the reserved-bit-only `0x2`.
        ///
        /// This is the minimal setup needed to execute a small real-mode code
        /// blob (the shape the Phase 0.2 serial smoke test uses); a full Linux
        /// boot will instead enter protected/long mode with a GDT.
        ///
        /// # Errors
        /// Returns [`Error::Vcpu`] if `index` is out of range or any of the
        /// `KVM_{GET,SET}_{SREGS,REGS}` ioctls fail.
        pub fn prepare_real_mode_vcpu(&self, index: usize, entry: u64) -> Result<()> {
            let vcpu = self
                .vcpus
                .get(index)
                .ok_or_else(|| Error::Vcpu(format!("no vcpu at index {index}")))?;

            let mut sregs = vcpu
                .get_sregs()
                .map_err(|e| Error::Vcpu(format!("KVM_GET_SREGS: {e}")))?;
            for seg in [
                &mut sregs.cs,
                &mut sregs.ds,
                &mut sregs.es,
                &mut sregs.fs,
                &mut sregs.gs,
                &mut sregs.ss,
            ] {
                seg.base = 0;
                seg.selector = 0;
            }
            vcpu.set_sregs(&sregs)
                .map_err(|e| Error::Vcpu(format!("KVM_SET_SREGS: {e}")))?;

            let mut regs = vcpu
                .get_regs()
                .map_err(|e| Error::Vcpu(format!("KVM_GET_REGS: {e}")))?;
            regs.rip = entry;
            regs.rflags = 0x2;
            vcpu.set_regs(&regs)
                .map_err(|e| Error::Vcpu(format!("KVM_SET_REGS: {e}")))?;
            Ok(())
        }

        /// Prepare vCPU `index` to start executing a 32-bit **protected-mode**
        /// code blob at `entry`, with paging off and flat segments.
        ///
        /// Real mode can only address the low 1 MiB, so a real-mode blob cannot
        /// reach the platform's high MMIO apertures (the LAPIC page at
        /// `0xFEE0_0000`, the I/O APIC at `0xFEC0_0000`, the HPET at
        /// `0xFED0_0000`). This sets `CR0.PE` and loads flat 4 GiB code/data
        /// segments straight into the cached descriptors via `KVM_SET_SREGS` —
        /// the same descriptor-cache trick kvmtool/Firecracker use to enter
        /// protected mode without a GDT in guest memory — so a blob can issue a
        /// 32-bit `mov` to an absolute high address and take a real MMIO exit.
        /// Paging stays off (`CR0.PG = 0`), so linear == physical.
        ///
        /// `CS` is a flat execute/read segment (selector `0x08`), the data
        /// segments a flat read/write segment (selector `0x10`); both are 32-bit
        /// (`db = 1`), page-granular (`g = 1`), present, ring 0.
        ///
        /// # Errors
        /// Returns [`Error::Vcpu`] if `index` is out of range or any of the
        /// `KVM_{GET,SET}_{SREGS,REGS}` ioctls fail.
        pub fn prepare_protected_mode_vcpu(&self, index: usize, entry: u64) -> Result<()> {
            let vcpu = self
                .vcpus
                .get(index)
                .ok_or_else(|| Error::Vcpu(format!("no vcpu at index {index}")))?;

            let mut sregs = vcpu
                .get_sregs()
                .map_err(|e| Error::Vcpu(format!("KVM_GET_SREGS: {e}")))?;

            // Flat 32-bit code segment (selector 0x08): execute/read, accessed.
            sregs.cs.base = 0;
            sregs.cs.limit = 0xFFFF_FFFF;
            sregs.cs.selector = 0x08;
            sregs.cs.type_ = 0b1011; // code, execute/read, accessed
            sregs.cs.s = 1; // code/data (not system)
            sregs.cs.dpl = 0;
            sregs.cs.present = 1;
            sregs.cs.db = 1; // 32-bit default operand/address size
            sregs.cs.l = 0; // not 64-bit
            sregs.cs.g = 1; // 4 KiB granularity -> limit is in pages

            // Flat 32-bit data segments (selector 0x10): read/write, accessed.
            for seg in [
                &mut sregs.ds,
                &mut sregs.es,
                &mut sregs.fs,
                &mut sregs.gs,
                &mut sregs.ss,
            ] {
                seg.base = 0;
                seg.limit = 0xFFFF_FFFF;
                seg.selector = 0x10;
                seg.type_ = 0b0011; // data, read/write, accessed
                seg.s = 1;
                seg.dpl = 0;
                seg.present = 1;
                seg.db = 1;
                seg.l = 0;
                seg.g = 1;
            }

            // Enter protected mode (CR0.PE), paging off (CR0.PG clear).
            sregs.cr0 = (sregs.cr0 | 0x1) & !(1 << 31);

            vcpu.set_sregs(&sregs)
                .map_err(|e| Error::Vcpu(format!("KVM_SET_SREGS: {e}")))?;

            let mut regs = vcpu
                .get_regs()
                .map_err(|e| Error::Vcpu(format!("KVM_GET_REGS: {e}")))?;
            regs.rip = entry;
            regs.rflags = 0x2;
            vcpu.set_regs(&regs)
                .map_err(|e| Error::Vcpu(format!("KVM_SET_REGS: {e}")))?;
            Ok(())
        }

        /// Prepare vCPU `index` to start executing 64-bit **long-mode** code at
        /// `entry`, with paging enabled through the page tables the caller has
        /// already written to guest RAM, rooted at `pml4_gpa` (the PML4's
        /// guest-physical address). This is the mode a real x86-64 kernel — and a
        /// 64-bit ACPI S3 resume trampoline — runs in.
        ///
        /// Sets `CR4.PAE`, `EFER.LME|LMA`, and `CR0.PE|PG`, points `CR3` at
        /// `pml4_gpa`, and loads a flat 64-bit code segment (`CS.L = 1`, selector
        /// `0x08`) plus flat data segments (selector `0x10`) straight into the
        /// cached descriptors — the descriptor-cache trick, so no GDT is needed in
        /// guest memory. The **caller owns the page tables**: they must at least
        /// identity-map the code at `entry` (and be reachable at `pml4_gpa`) or the
        /// first instruction fetch page-faults.
        ///
        /// # Errors
        /// Returns [`Error::Vcpu`] if `index` is out of range or any of the
        /// `KVM_{GET,SET}_{SREGS,REGS}` ioctls fail.
        pub fn prepare_long_mode_vcpu(
            &self,
            index: usize,
            entry: u64,
            pml4_gpa: u64,
        ) -> Result<()> {
            let vcpu = self
                .vcpus
                .get(index)
                .ok_or_else(|| Error::Vcpu(format!("no vcpu at index {index}")))?;

            let mut sregs = vcpu
                .get_sregs()
                .map_err(|e| Error::Vcpu(format!("KVM_GET_SREGS: {e}")))?;

            // Flat 64-bit code segment (selector 0x08): execute/read, accessed,
            // L = 1 (64-bit), db = 0 (required when L = 1).
            sregs.cs.base = 0;
            sregs.cs.limit = 0xFFFF_FFFF;
            sregs.cs.selector = 0x08;
            sregs.cs.type_ = 0b1011;
            sregs.cs.s = 1;
            sregs.cs.dpl = 0;
            sregs.cs.present = 1;
            sregs.cs.l = 1;
            sregs.cs.db = 0;
            sregs.cs.g = 1;

            // Flat data segments (selector 0x10): read/write, accessed.
            for seg in [
                &mut sregs.ds,
                &mut sregs.es,
                &mut sregs.fs,
                &mut sregs.gs,
                &mut sregs.ss,
            ] {
                seg.base = 0;
                seg.limit = 0xFFFF_FFFF;
                seg.selector = 0x10;
                seg.type_ = 0b0011;
                seg.s = 1;
                seg.dpl = 0;
                seg.present = 1;
                seg.db = 1;
                seg.l = 0;
                seg.g = 1;
            }

            // Enable long mode: PAE, EFER.LME|LMA, CR0.PE|PG, CR3 → the caller's
            // page tables.
            sregs.cr3 = pml4_gpa;
            sregs.cr4 |= 1 << 5; // CR4.PAE
            sregs.efer |= (1 << 8) | (1 << 10); // EFER.LME | EFER.LMA
            sregs.cr0 |= (1 << 0) | (1 << 31); // CR0.PE | CR0.PG

            vcpu.set_sregs(&sregs)
                .map_err(|e| Error::Vcpu(format!("KVM_SET_SREGS: {e}")))?;

            let mut regs = vcpu
                .get_regs()
                .map_err(|e| Error::Vcpu(format!("KVM_GET_REGS: {e}")))?;
            regs.rip = entry;
            regs.rflags = 0x2;
            vcpu.set_regs(&regs)
                .map_err(|e| Error::Vcpu(format!("KVM_SET_REGS: {e}")))?;
            Ok(())
        }

        /// Prepare vCPU `index` to enter a Linux protected-mode kernel at `entry`
        /// per the 32-bit boot protocol: flat 32-bit protected mode (as
        /// [`prepare_protected_mode_vcpu`](Self::prepare_protected_mode_vcpu)) with
        /// `RSI` pointing at the `boot_params` zero page — the one register the
        /// kernel's 32-bit entry reads to find its configuration. The caller must
        /// have placed the kernel and `boot_params` in guest RAM first.
        ///
        /// # Errors
        /// Returns [`Error::Vcpu`] if `index` is out of range or any of the
        /// `KVM_{GET,SET}_{SREGS,REGS}` ioctls fail.
        pub fn prepare_linux_boot_vcpu(
            &self,
            index: usize,
            entry: u64,
            boot_params: u64,
        ) -> Result<()> {
            self.prepare_protected_mode_vcpu(index, entry)?;
            let vcpu = self
                .vcpus
                .get(index)
                .ok_or_else(|| Error::Vcpu(format!("no vcpu at index {index}")))?;
            let mut regs = vcpu
                .get_regs()
                .map_err(|e| Error::Vcpu(format!("KVM_GET_REGS: {e}")))?;
            regs.rsi = boot_params;
            vcpu.set_regs(&regs)
                .map_err(|e| Error::Vcpu(format!("KVM_SET_REGS: {e}")))?;
            Ok(())
        }

        /// Prepare vCPU `index` to enter a Linux kernel via its **64-bit** entry
        /// point at `entry` — long mode through the page tables at `pml4_gpa` (as
        /// [`prepare_long_mode_vcpu`](Self::prepare_long_mode_vcpu)) with `RSI`
        /// pointing at `boot_params`. This is the 64-bit boot protocol modern
        /// kernels prefer; the caller places the kernel, `boot_params`, and page
        /// tables in guest RAM first.
        ///
        /// # Errors
        /// Returns [`Error::Vcpu`] if `index` is out of range or any of the
        /// `KVM_{GET,SET}_{SREGS,REGS}` ioctls fail.
        pub fn prepare_linux_boot_vcpu_64(
            &self,
            index: usize,
            entry: u64,
            boot_params: u64,
            pml4_gpa: u64,
        ) -> Result<()> {
            self.prepare_long_mode_vcpu(index, entry, pml4_gpa)?;
            let vcpu = self
                .vcpus
                .get(index)
                .ok_or_else(|| Error::Vcpu(format!("no vcpu at index {index}")))?;
            let mut regs = vcpu
                .get_regs()
                .map_err(|e| Error::Vcpu(format!("KVM_GET_REGS: {e}")))?;
            regs.rsi = boot_params;
            vcpu.set_regs(&regs)
                .map_err(|e| Error::Vcpu(format!("KVM_SET_REGS: {e}")))?;
            Ok(())
        }

        /// Registered guest memory slots.
        #[must_use]
        pub fn mem_slots(&self) -> &[MemSlot] {
            &self.slots
        }

        /// A [`DmaMemory`] view over this VM's registered guest RAM, for
        /// emulated devices that DMA into/out of guest memory (xHCI rings,
        /// virtio queues, …). Borrows the backend, so it cannot outlive the VM.
        #[must_use]
        pub fn guest_memory(&self) -> GuestMemory<'_> {
            GuestMemory::new(&self.slots)
        }

        /// Access the underlying [`Kvm`] handle (capability queries, etc.).
        #[must_use]
        pub fn kvm(&self) -> &Kvm {
            &self.kvm
        }

        /// Run vCPU `index` until its next exit, dispatching I/O and MMIO to
        /// `handler`, and return a [`GuestExit`] describing why it stopped.
        ///
        /// # Errors
        /// Returns [`Error::Vcpu`] if `index` is out of range or `KVM_RUN`
        /// fails for a reason other than `EINTR` (which is surfaced as
        /// [`GuestExit::Interrupted`]).
        pub fn run_vcpu(
            &mut self,
            index: usize,
            handler: &mut dyn VmExitHandler,
        ) -> Result<GuestExit> {
            let vcpu = self
                .vcpus
                .get_mut(index)
                .ok_or_else(|| Error::Vcpu(format!("no vcpu at index {index}")))?;

            match vcpu.run() {
                Ok(exit) => Ok(Self::dispatch(exit, handler)),
                Err(e) if e.errno() == libc::EINTR => Ok(GuestExit::Interrupted),
                Err(e) => Err(Error::Vcpu(format!("KVM_RUN: {e}"))),
            }
        }

        /// Run vCPU `index` once like [`run_vcpu`](Self::run_vcpu), but drive
        /// the per-vCPU timing shadows around the entry so the guest-visible
        /// APERF/MPERF hide the time spent handling the *previous* exit.
        ///
        /// Per call: read the host TSC just before re-entry and
        /// [`on_vmresume`](crate::timing_stealth::VcpuTimingState::on_vmresume)
        /// to subtract the userspace gap since the last exit (proportionally,
        /// so the APERF/MPERF ratio is preserved); run; read the TSC again,
        /// [`advance`](crate::timing_stealth::VcpuTimingState::advance) the
        /// shadows by the in-guest delta at the model's core/ref rate, and
        /// record the exit TSC for the next gap. The net effect: the shadows
        /// count guest execution at the model frequency and never advance
        /// across a VMEXIT — an IET divergence detector reading APERF/MPERF sees
        /// continuous guest time with no hypervisor overhead.
        ///
        /// `model` must be the same [`PmcRateModel`] used to drive the RDPMC
        /// fixed counters, or the two stealth surfaces would disagree.
        ///
        /// Returns the exit **and** the in-guest reference-cycle delta this
        /// entry advanced the timing shadows by. The PMC counters live in the
        /// handler (not shared like the timing `Arc`), so they cannot be
        /// advanced inside this call without aliasing `handler`; instead the
        /// run loop feeds the returned delta to `PmcState::advance_counters`
        /// (e.g. via `bus.stealth_msr_mut()`) *after* this returns, keeping the
        /// RDPMC surface in lockstep with APERF/MPERF at the same model rate.
        ///
        /// [`PmcRateModel`]: enlil_devices::stealth::pmc::PmcRateModel
        ///
        /// # Errors
        /// As [`run_vcpu`](Self::run_vcpu).
        pub fn run_vcpu_timed(
            &mut self,
            index: usize,
            handler: &mut dyn VmExitHandler,
            timing: &crate::timing_stealth::VcpuTimingState,
            model: &enlil_devices::stealth::pmc::PmcRateModel,
        ) -> Result<(GuestExit, u64)> {
            // The RIP we are about to resume at — recorded for LBR sanitization
            // (on_vmresume stashes it); 0 if regs are unreadable.
            let guest_rip = self
                .vcpus
                .get(index)
                .and_then(|v| v.get_regs().ok())
                .map_or(0, |r| r.rip);

            // SAFETY: `_rdtsc` is a baseline x86-64 instruction with no
            // preconditions; the KVM backend only compiles for x86-64 Linux.
            let entry_tsc = unsafe { core::arch::x86_64::_rdtsc() };
            timing.on_vmresume(entry_tsc, guest_rip);

            let exit = self.run_vcpu(index, handler)?;

            // SAFETY: as above.
            let exit_tsc = unsafe { core::arch::x86_64::_rdtsc() };
            let guest_ref_cycles = exit_tsc.saturating_sub(entry_tsc);
            timing.advance(guest_ref_cycles, model);
            timing.on_vmexit(exit_tsc);

            Ok((exit, guest_ref_cycles))
        }

        /// Arm (or disarm) a vCPU's KVM immediate-exit flag.
        ///
        /// With it armed, the next [`run_vcpu`](Self::run_vcpu) returns
        /// [`GuestExit::Interrupted`] without entering the guest — and an
        /// arm-from-another-thread *while* `KVM_RUN` is in flight kicks the
        /// vCPU straight back out. This is the bound the run loop needs over an
        /// otherwise-unbounded `KVM_RUN`: with the in-kernel IRQ chip a guest
        /// that idles in `HLT` (or spins without exiting) never returns on its
        /// own, so a watchdog arms this to reclaim the thread. The caller
        /// disarms it (pass `false`) after the run returns before re-entering.
        ///
        /// # Errors
        /// Returns [`Error::Vcpu`] if `index` is out of range.
        pub fn set_immediate_exit(&mut self, index: usize, armed: bool) -> Result<()> {
            let vcpu = self
                .vcpus
                .get_mut(index)
                .ok_or_else(|| Error::Vcpu(format!("no vcpu at index {index}")))?;
            vcpu.set_kvm_immediate_exit(u8::from(armed));
            Ok(())
        }

        /// Translate a single `kvm-ioctls` exit into a [`GuestExit`], invoking
        /// the handler for data transfer. Split out so the mapping is easy to
        /// reason about.
        fn dispatch(exit: VcpuExit<'_>, handler: &mut dyn VmExitHandler) -> GuestExit {
            match exit {
                VcpuExit::IoIn(port, data) => {
                    let size = data.len();
                    handler.io_in(port, data);
                    GuestExit::IoIn { port, size }
                }
                VcpuExit::IoOut(port, data) => {
                    let size = data.len();
                    handler.io_out(port, data);
                    GuestExit::IoOut { port, size }
                }
                VcpuExit::MmioRead(addr, data) => {
                    let size = data.len();
                    handler.mmio_read(addr, data);
                    GuestExit::MmioRead { addr, size }
                }
                VcpuExit::MmioWrite(addr, data) => {
                    let size = data.len();
                    handler.mmio_write(addr, data);
                    GuestExit::MmioWrite { addr, size }
                }
                VcpuExit::X86Rdmsr(exit) => {
                    let msr = exit.index;
                    match handler.rdmsr(msr) {
                        Some(value) => {
                            *exit.data = value;
                            *exit.error = 0;
                        }
                        // Refused: inject #GP into the guest on re-entry.
                        None => *exit.error = 1,
                    }
                    GuestExit::MsrRead { msr }
                }
                VcpuExit::X86Wrmsr(exit) => {
                    let msr = exit.index;
                    let value = exit.data;
                    // Refused: inject #GP into the guest on re-entry.
                    *exit.error = u8::from(!handler.wrmsr(msr, value));
                    GuestExit::MsrWrite { msr, value }
                }
                VcpuExit::Hlt => GuestExit::Halted,
                VcpuExit::Shutdown | VcpuExit::SystemEvent(..) => GuestExit::Shutdown,
                VcpuExit::Debug(_) => GuestExit::Debug,
                VcpuExit::Intr | VcpuExit::IrqWindowOpen => GuestExit::Interrupted,
                VcpuExit::InternalError => GuestExit::InternalError,
                VcpuExit::FailEntry(reason, _cpu) => GuestExit::FailedEntry(reason),
                VcpuExit::Unsupported(reason) => GuestExit::Unsupported(reason),
                // Everything else (Hypercall, NMI, …) is not yet modelled;
                // surface a sentinel so the caller can log it.
                _ => GuestExit::Unsupported(u32::MAX),
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::{
    is_kvm_available, GuestMemory, GuestRam, KvmBackend, KvmVcpuState, MemSlot, HOST_PAGE_SIZE,
};

#[cfg(test)]
mod tests {
    use super::*;

    // Deterministic coverage of the per-vCPU APIC-identity stamping (the fix in
    // apply_topology_stealth). Unlike the live-KVM tests it can include an
    // Intel-style leaf 0x1F that this AMD host never exposes, exercising every
    // stamped leaf without hardware.
    #[cfg(target_os = "linux")]
    #[test]
    fn stamp_apic_identity_writes_each_per_vcpu_leaf() {
        use kvm_bindings::kvm_cpuid_entry2;
        let mut entries = vec![
            kvm_cpuid_entry2 {
                function: 1,
                ebx: 0x0000_AB00, // low bytes (CLFLUSH size, etc.) must survive
                ..Default::default()
            },
            kvm_cpuid_entry2 {
                function: 0xB,
                edx: 0,
                ..Default::default()
            },
            kvm_cpuid_entry2 {
                function: 0x1F, // Intel v2 topology — absent on this AMD host
                edx: 0,
                ..Default::default()
            },
            kvm_cpuid_entry2 {
                function: 0x8000_001E,
                eax: 0,
                ebx: 0x0000_0100, // SMT-width field (EBX[15:8]) must survive
                ..Default::default()
            },
            kvm_cpuid_entry2 {
                function: 0x8000_0008, // unrelated leaf — must be untouched
                eax: 0x3030,
                ..Default::default()
            },
        ];

        // APIC id 5, SMT shift 1 → core id = 5 >> 1 = 2.
        KvmBackend::stamp_apic_identity(&mut entries, 5, 1);

        // leaf 1: EBX[31:24] = initial APIC id; low 24 bits preserved.
        assert_eq!(entries[0].ebx >> 24, 5);
        assert_eq!(entries[0].ebx & 0x00FF_FFFF, 0x0000_AB00);
        // leaf 0xB and 0x1F: EDX = x2APIC id.
        assert_eq!(entries[1].edx, 5);
        assert_eq!(entries[2].edx, 5);
        // leaf 0x8000_001E: EAX = extended APIC id; EBX[7:0] = core id; EBX high
        // (SMT width) preserved.
        assert_eq!(entries[3].eax, 5);
        assert_eq!(entries[3].ebx & 0xFF, 2);
        assert_eq!(entries[3].ebx & 0xFF00, 0x0100);
        // Unrelated leaf is left exactly as-is.
        assert_eq!(entries[4].eax, 0x3030);
    }

    // upsert_frequency_leaves folds the Intel TSC/processor-frequency leaves
    // (0x15/0x16) into a CPUID entry set, pinning the rate to the measured
    // effective guest TSC kHz, and is a no-op for an AMD-vendor table. Pure over
    // `entries`, so it is unit-testable without /dev/kvm.
    #[cfg(target_os = "linux")]
    #[test]
    fn upsert_frequency_leaves_pins_intel_tsc_rate_and_skips_amd() {
        use enlil_devices::stealth::cpuid::{CpuVendor, CpuidStealthConfig, CpuidStealthTable};
        use kvm_bindings::kvm_cpuid_entry2;

        // Intel-vendor table populates leaf 0x16 (EAX != 0).
        let mut intel_cfg = CpuidStealthConfig::from_host(1, 1);
        intel_cfg.vendor = CpuVendor::Intel;
        let intel = CpuidStealthTable::build(&intel_cfg);
        assert_ne!(
            intel.lookup(0x16, 0).eax,
            0,
            "Intel table must enumerate 0x16"
        );

        // Start from a set lacking 0x15/0x16 but with an unrelated leaf that must
        // survive untouched.
        let mut entries = vec![kvm_cpuid_entry2 {
            function: 1,
            eax: 0xDEAD,
            ..Default::default()
        }];

        // Pin to a measured 3_000_001 kHz -> 3000 MHz (round to nearest).
        KvmBackend::upsert_frequency_leaves(&mut entries, &intel, Some(3_000_001));

        let l15 = entries
            .iter()
            .find(|e| e.function == 0x15)
            .expect("0x15 inserted");
        let l16 = entries
            .iter()
            .find(|e| e.function == 0x16)
            .expect("0x16 inserted");
        // leaf 0x15: TSC = crystal(ECX) * EBX / EAX must equal 3000 MHz exactly.
        assert_eq!(l15.eax, 24, "EAX denominator = table's 24");
        assert_eq!(l15.ebx, 3000, "EBX = base MHz pinned to measured rate");
        assert_eq!(l15.ecx, 24_000_000, "ECX = 24 MHz crystal");
        assert_eq!(
            u64::from(l15.ecx) * u64::from(l15.ebx) / u64::from(l15.eax),
            3_000_000_000,
            "derived TSC rate must equal 3000 MHz"
        );
        // leaf 0x16: EAX = base MHz; EBX (max turbo) must never read below base.
        assert_eq!(l16.eax, 3000, "0x16 EAX = base MHz");
        assert!(l16.ebx >= 3000, "max turbo must not be below base");
        assert_eq!(l16.index, 0, "single-subleaf leaf");
        // Unrelated leaf is untouched.
        assert_eq!(
            entries.iter().find(|e| e.function == 1).unwrap().eax,
            0xDEAD
        );

        // Without a measured rate, the table's static base (non-zero) is used.
        let mut entries2 = Vec::new();
        KvmBackend::upsert_frequency_leaves(&mut entries2, &intel, None);
        let l16b = entries2.iter().find(|e| e.function == 0x16).unwrap();
        assert_eq!(
            l16b.eax,
            intel.lookup(0x16, 0).eax,
            "no measured rate -> table's static base frequency"
        );

        // An existing 0x15 entry is overwritten in place (upsert, not duplicate).
        let mut entries3 = vec![kvm_cpuid_entry2 {
            function: 0x15,
            ebx: 9999,
            ..Default::default()
        }];
        KvmBackend::upsert_frequency_leaves(&mut entries3, &intel, Some(2_500_000));
        assert_eq!(
            entries3.iter().filter(|e| e.function == 0x15).count(),
            1,
            "no duplicate 0x15 entry"
        );
        assert_eq!(
            entries3.iter().find(|e| e.function == 0x15).unwrap().ebx,
            2500
        );

        // AMD-vendor table: 0x15/0x16 stay out of range, so this is a no-op.
        let amd_cfg = CpuidStealthConfig::from_host(1, 1);
        let amd = {
            let mut c = amd_cfg;
            c.vendor = CpuVendor::Amd;
            CpuidStealthTable::build(&c)
        };
        let mut amd_entries: Vec<kvm_cpuid_entry2> = Vec::new();
        KvmBackend::upsert_frequency_leaves(&mut amd_entries, &amd, Some(3_000_000));
        assert!(
            amd_entries.is_empty(),
            "AMD table must not synthesise 0x15/0x16"
        );
    }

    // The Intel frequency leaves are only reachable if leaf 0's max-basic-leaf
    // (EAX) advertises them, so upsert_frequency_leaves must raise a too-low
    // leaf-0 EAX to 0x16 (and never lower a higher one), and never touch leaf 0
    // for an AMD table (which installs nothing).
    #[cfg(target_os = "linux")]
    #[test]
    fn upsert_frequency_leaves_advertises_0x16_in_the_max_basic_leaf() {
        use enlil_devices::stealth::cpuid::{CpuVendor, CpuidStealthConfig, CpuidStealthTable};
        use kvm_bindings::kvm_cpuid_entry2;

        let intel = {
            let mut c = CpuidStealthConfig::from_host(1, 1);
            c.vendor = CpuVendor::Intel;
            CpuidStealthTable::build(&c)
        };

        // An AMD-host-style leaf 0 whose basic range ends at 0x10 (< 0x16): it
        // must be raised so the guest reaches the installed 0x16.
        let mut entries = vec![kvm_cpuid_entry2 {
            function: 0,
            eax: 0x10,
            ..Default::default()
        }];
        KvmBackend::upsert_frequency_leaves(&mut entries, &intel, Some(3_000_000));
        assert_eq!(
            entries.iter().find(|e| e.function == 0).unwrap().eax,
            0x16,
            "max-basic-leaf must be raised to cover 0x16"
        );

        // A leaf 0 already advertising a higher max (e.g. 0x1F) must NOT be
        // lowered.
        let mut high = vec![kvm_cpuid_entry2 {
            function: 0,
            eax: 0x1F,
            ..Default::default()
        }];
        KvmBackend::upsert_frequency_leaves(&mut high, &intel, Some(3_000_000));
        assert_eq!(
            high.iter().find(|e| e.function == 0).unwrap().eax,
            0x1F,
            "a higher max-basic-leaf must survive"
        );

        // AMD table installs nothing, so leaf 0 is left exactly as-is.
        let amd = {
            let mut c = CpuidStealthConfig::from_host(1, 1);
            c.vendor = CpuVendor::Amd;
            CpuidStealthTable::build(&c)
        };
        let mut amd_entries = vec![kvm_cpuid_entry2 {
            function: 0,
            eax: 0x10,
            ..Default::default()
        }];
        KvmBackend::upsert_frequency_leaves(&mut amd_entries, &amd, Some(3_000_000));
        assert_eq!(
            amd_entries.iter().find(|e| e.function == 0).unwrap().eax,
            0x10,
            "AMD table must not touch the max-basic-leaf"
        );
    }

    // upsert_identity_leaves stamps the table's vendor string (leaf 0), FMS
    // (leaf 1 EAX), and brand string (0x8000_0002-4) into a CPUID entry set while
    // leaving leaf-0 EAX (max-basic-leaf) and leaf-1 EBX/ECX/EDX (APIC + host
    // feature bits) untouched. Pure over `entries`, so unit-testable without
    // /dev/kvm; uses a masqueraded (Intel) table distinct from this AMD host so
    // every assertion is non-vacuous.
    #[cfg(target_os = "linux")]
    #[test]
    fn upsert_identity_leaves_installs_vendor_fms_and_brand() {
        use enlil_devices::stealth::cpuid::{CpuVendor, CpuidStealthConfig, CpuidStealthTable};
        use kvm_bindings::kvm_cpuid_entry2;

        // A masqueraded identity: Intel vendor, a distinctive FMS, and a sentinel
        // brand string — all different from the AMD host this test runs on.
        let mut cfg = CpuidStealthConfig::from_host(1, 1);
        cfg.vendor = CpuVendor::Intel;
        cfg.family_model_stepping = 0x000B_06F2;
        let mut brand = [0u8; 48];
        brand[..b"Enlil Masquerade CPU @ 3.00GHz".len()]
            .copy_from_slice(b"Enlil Masquerade CPU @ 3.00GHz");
        cfg.brand_string = brand;
        let table = CpuidStealthTable::build(&cfg);

        // Host-mirrored starting set: leaf 0 with AMD-style vendor regs and a low
        // max-basic-leaf, leaf 1 with a host FMS + sentinel EBX/ECX/EDX, and one
        // existing brand leaf (to prove overwrite) — 0x8000_0003/4 are absent so
        // the insert path is also exercised.
        let (amd_ebx, amd_edx, amd_ecx) = CpuVendor::Amd.vendor_regs();
        let mut entries = vec![
            kvm_cpuid_entry2 {
                function: 0,
                eax: 0x10,
                ebx: amd_ebx,
                ecx: amd_ecx,
                edx: amd_edx,
                ..Default::default()
            },
            kvm_cpuid_entry2 {
                function: 1,
                eax: 0x00A0_0F11, // host FMS
                ebx: 0x0102_0304, // APIC/max-IDs + CLFLUSH — must survive
                ecx: 0x1234_5678, // feature bits — must survive
                edx: 0x8765_4321,
                ..Default::default()
            },
            kvm_cpuid_entry2 {
                function: 0x8000_0002,
                eax: 0xDEAD_BEEF, // stale host brand chunk — must be overwritten
                ..Default::default()
            },
        ];

        KvmBackend::upsert_identity_leaves(&mut entries, &table);

        // Leaf 0: vendor regs replaced with the table's (Intel); EAX untouched.
        let l0 = entries.iter().find(|e| e.function == 0).unwrap();
        let t0 = table.lookup(0, 0);
        assert_eq!(l0.eax, 0x10, "leaf-0 max-basic-leaf must not be lowered");
        assert_eq!((l0.ebx, l0.ecx, l0.edx), (t0.ebx, t0.ecx, t0.edx));
        assert_ne!(
            (l0.ebx, l0.ecx, l0.edx),
            (amd_ebx, amd_ecx, amd_edx),
            "vendor regs must no longer be the host AMD string"
        );

        // Leaf 1: EAX = table FMS; EBX/ECX/EDX preserved.
        let l1 = entries.iter().find(|e| e.function == 1).unwrap();
        assert_eq!(l1.eax, table.lookup(1, 0).eax, "leaf-1 EAX = table FMS");
        assert_eq!(l1.ebx, 0x0102_0304, "leaf-1 EBX preserved");
        assert_eq!(l1.ecx, 0x1234_5678, "leaf-1 ECX preserved");
        assert_eq!(l1.edx, 0x8765_4321, "leaf-1 EDX preserved");

        // Brand leaves: existing overwritten, missing inserted, all from table.
        for i in 0..3u32 {
            let function = 0x8000_0002 + i;
            let e = entries
                .iter()
                .find(|e| e.function == function)
                .unwrap_or_else(|| panic!("brand leaf {function:#x} present"));
            let r = table.lookup(function, 0);
            assert_eq!(
                (e.eax, e.ebx, e.ecx, e.edx),
                (r.eax, r.ebx, r.ecx, r.edx),
                "brand leaf {function:#x} matches table"
            );
        }
        assert_eq!(
            entries.iter().filter(|e| e.function == 0x8000_0002).count(),
            1,
            "no duplicate brand leaf"
        );
    }

    // Real-KVM: apply_topology_stealth must make the guest read the *table's*
    // brand string, not the host's KVM-passthrough brand. Builds a host-vendor
    // (AMD) table with only the brand string masqueraded — safe on real hardware
    // since the brand is pure data — and has a real-mode guest read leaf
    // 0x8000_0002 and echo its four bytes over COM1. Self-skips without /dev/kvm.
    #[cfg(target_os = "linux")]
    #[test]
    fn topology_stealth_installs_the_table_brand_string() {
        use enlil_devices::stealth::cpuid::{CpuidStealthConfig, CpuidStealthTable};

        if !is_kvm_available() {
            eprintln!("skipping topology_stealth_installs_the_table_brand_string: no /dev/kvm");
            return;
        }

        // 16-bit real-mode blob; reads leaf 0x8000_0002 (first 4 brand bytes in
        // EAX) and echoes them low-byte-first over COM1:
        //   66 B8 02 00 00 80   mov eax, 0x80000002
        //   0F A2               cpuid
        //   BA F8 03            mov dx, 0x3F8
        //   EE                  out dx, al        ; brand[0]
        //   66 C1 E8 08         shr eax, 8
        //   EE                  out dx, al        ; brand[1]
        //   66 C1 E8 08         shr eax, 8
        //   EE                  out dx, al        ; brand[2]
        //   66 C1 E8 08         shr eax, 8
        //   EE                  out dx, al        ; brand[3]
        //   F4                  hlt
        #[rustfmt::skip]
        let code: [u8; 28] = [
            0x66, 0xB8, 0x02, 0x00, 0x00, 0x80,
            0x0F, 0xA2,
            0xBA, 0xF8, 0x03,
            0xEE,
            0x66, 0xC1, 0xE8, 0x08,
            0xEE,
            0x66, 0xC1, 0xE8, 0x08,
            0xEE,
            0x66, 0xC1, 0xE8, 0x08,
            0xEE,
            0xF4,
        ];

        const ENTRY: u64 = 0x1000;
        const SIZE: usize = 0x1000;
        let mut ram = GuestRam::new(SIZE);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu 0");

        // Host-vendor table with only the brand string masqueraded to a sentinel
        // that differs from the real host brand, so the assertion is non-vacuous.
        let mut cfg = CpuidStealthConfig::from_host(1, 1);
        let mut brand = [0u8; 48];
        brand[..b"ENLIL-STEALTH-IDENTITY".len()].copy_from_slice(b"ENLIL-STEALTH-IDENTITY");
        cfg.brand_string = brand;
        let table = CpuidStealthTable::build(&cfg);
        let want = table.lookup(0x8000_0002, 0).eax.to_le_bytes().to_vec();

        backend
            .apply_topology_stealth(&table)
            .expect("apply topology stealth");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        struct EchoOut(Vec<u8>);
        impl VmExitHandler for EchoOut {
            fn io_out(&mut self, _port: u16, data: &[u8]) {
                self.0.extend_from_slice(data);
            }
        }
        let mut echo = EchoOut(Vec::new());

        let mut halted = false;
        for _ in 0..100 {
            if backend.run_vcpu(0, &mut echo).expect("run vcpu") == GuestExit::Halted {
                halted = true;
                break;
            }
        }
        assert!(halted, "guest never reached HLT");
        assert_eq!(
            echo.0, want,
            "guest must read the table's masqueraded brand, not the host's"
        );
    }

    #[test]
    fn terminal_exits_stop_the_loop() {
        assert_eq!(GuestExit::Halted.outcome(), RunOutcome::Stopped);
        assert_eq!(GuestExit::Shutdown.outcome(), RunOutcome::Stopped);
        assert_eq!(GuestExit::InternalError.outcome(), RunOutcome::Stopped);
        assert_eq!(GuestExit::FailedEntry(7).outcome(), RunOutcome::Stopped);
    }

    #[test]
    fn resumable_exits_continue_the_loop() {
        assert_eq!(
            GuestExit::IoIn {
                port: 0x3f8,
                size: 1
            }
            .outcome(),
            RunOutcome::Continue
        );
        assert_eq!(
            GuestExit::MmioWrite {
                addr: 0xfee0_0000,
                size: 4
            }
            .outcome(),
            RunOutcome::Continue
        );
        assert_eq!(GuestExit::Interrupted.outcome(), RunOutcome::Continue);
        assert_eq!(GuestExit::Unsupported(42).outcome(), RunOutcome::Continue);
        assert_eq!(
            GuestExit::MsrRead { msr: 0xE8 }.outcome(),
            RunOutcome::Continue
        );
        assert_eq!(
            GuestExit::MsrWrite {
                msr: 0x1D9,
                value: 1
            }
            .outcome(),
            RunOutcome::Continue
        );
    }

    #[test]
    fn recording_handler_captures_writes_and_read_sizes() {
        let mut h = RecordingHandler::default();
        h.io_out(0x3f8, b"OK");
        let mut buf = [0u8; 4];
        h.io_in(0x60, &mut buf);
        h.mmio_write(0xfed4_0000, &[1, 2, 3]);
        let mut mbuf = [0u8; 8];
        h.mmio_read(0xfee0_0000, &mut mbuf);

        assert_eq!(h.io_out, vec![(0x3f8, b"OK".to_vec())]);
        assert_eq!(h.io_in, vec![(0x60, 4)]);
        assert_eq!(h.mmio_write, vec![(0xfed4_0000, vec![1, 2, 3])]);
        assert_eq!(h.mmio_read, vec![(0xfee0_0000, 8)]);

        assert_eq!(h.rdmsr(0xE8), Some(0));
        assert!(h.wrmsr(0x1D9, 0x42));
        assert_eq!(h.msr_read, vec![0xE8]);
        assert_eq!(h.msr_write, vec![(0x1D9, 0x42)]);
    }

    #[test]
    fn default_handler_is_a_noop() {
        struct Bare;
        impl VmExitHandler for Bare {}
        let mut h = Bare;
        // Default impls must not panic and must leave read buffers untouched.
        let mut buf = [0xaau8; 2];
        h.io_in(0x3f8, &mut buf);
        h.mmio_read(0x1000, &mut buf);
        h.io_out(0x3f8, &[0]);
        h.mmio_write(0x1000, &[0]);
        assert_eq!(buf, [0xaa, 0xaa]);
        // An unmodelled MSR is refused by default (→ #GP in the guest), not
        // silently spoofed.
        assert_eq!(h.rdmsr(0xE8), None);
        assert!(!h.wrmsr(0x1D9, 0x1));
    }

    // Integration test that touches real KVM. It is honest about the runner:
    // when `/dev/kvm` is unavailable (no nested virt) it returns early and is
    // reported as skipped rather than fabricating a pass.
    #[cfg(target_os = "linux")]
    #[test]
    fn kvm_create_vm_and_map_memory() {
        if !is_kvm_available() {
            eprintln!("skipping: /dev/kvm not available (no nested virt)");
            return;
        }

        let mut backend = KvmBackend::new().expect("create KVM VM");

        // Back the guest with a page-aligned host buffer (a plain `Vec<u8>`
        // is only byte-aligned and KVM would reject it with EINVAL).
        const SIZE: usize = 0x1000;
        let ram = GuestRam::new(SIZE);
        let host_addr = ram.host_addr();
        // SAFETY: `ram` outlives `backend` within this test scope.
        let slot = unsafe { backend.map_memory(0x1000, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        assert_eq!(slot.slot, 0);
        assert_eq!(backend.mem_slots().len(), 1);

        let idx = backend.create_vcpu(0).expect("create vcpu");
        assert_eq!(idx, 0);
        assert_eq!(backend.vcpu_count(), 1);
    }

    #[test]
    fn tsc_khz_reports_a_plausible_host_frequency() {
        if !is_kvm_available() {
            eprintln!("skipping: /dev/kvm not available (no nested virt)");
            return;
        }

        let mut backend = KvmBackend::new().expect("create KVM VM");
        // No vCPU yet → the frequency cannot be read.
        assert!(
            backend.tsc_khz().is_err(),
            "tsc_khz must require a vCPU to query"
        );

        backend.create_vcpu(0).expect("create vcpu");
        let khz = backend.tsc_khz().expect("KVM_GET_TSC_KHZ");
        // Any real x86-64 host TSC runs well above 100 MHz; sanity-bound it
        // rather than pin an exact value (it is host-specific).
        assert!(khz > 100_000, "implausible host TSC frequency: {khz} kHz");
    }

    #[test]
    fn read_guest_tsc_reports_a_running_counter() {
        if !is_kvm_available() {
            eprintln!("skipping: /dev/kvm not available (no nested virt)");
            return;
        }

        let mut backend = KvmBackend::new().expect("create KVM VM");
        // No vCPU yet → there is no guest TSC to read.
        assert!(
            backend.read_guest_tsc(0).is_err(),
            "read_guest_tsc must require a vCPU"
        );

        backend.create_vcpu(0).expect("create vcpu");
        let first = backend.read_guest_tsc(0).expect("KVM_GET_MSRS(IA32_TSC)");
        // The TSC is monotonic and free-running: a second read is never earlier
        // than the first (wall time only moves forward between the two ioctls).
        let second = backend.read_guest_tsc(0).expect("KVM_GET_MSRS(IA32_TSC)");
        assert!(
            second >= first,
            "guest TSC went backwards: {first} -> {second}"
        );
    }

    // Item 2.3 / 5.7: a full vCPU state snapshot must round-trip through
    // save_vcpu_state → restore_vcpu_state exactly. Real KVM (skips without
    // /dev/kvm). The proof is non-vacuous: between save and restore the live
    // vCPU is driven to a divergent state (protected mode), and one MSR in the
    // snapshot is set to a sentinel so the restore provably writes it.
    #[cfg(target_os = "linux")]
    #[test]
    fn vcpu_state_round_trips_through_save_and_restore() {
        if !is_kvm_available() {
            eprintln!("skipping: /dev/kvm not available (no nested virt)");
            return;
        }

        const ENTRY: u64 = 0x1000;
        const SIZE: usize = 0x1000;
        let ram = GuestRam::new(SIZE);
        let host_addr = ram.host_addr();
        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu 0");
        // Configure the guest CPUID (as the run loop does before entry) so KVM
        // exposes the long-mode MSRs (FS/GS base, the SYSCALL MSRs) to
        // KVM_GET_MSRS — they are gated on the guest advertising long mode.
        backend
            .clear_cpuid_hypervisor_bit()
            .expect("install supported CPUID");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("prepare real-mode vcpu");

        // Baseline snapshot, with LSTAR overwritten to a sentinel so restoring it
        // is provably faithful rather than a no-op.
        const LSTAR: u32 = 0xC000_0082;
        const SENTINEL: u64 = 0xFFFF_8000_DEAD_BEEF; // canonical MSR_LSTAR value
        let mut snap = backend.save_vcpu_state(0).expect("save baseline state");
        assert_eq!(snap.regs.rip, ENTRY, "baseline rip is the real-mode entry");
        // LSTAR is served by every long-mode KVM; guard anyway so the MSR
        // assertion is skipped rather than spuriously failing on a host that
        // does not expose it, while the regs/sregs/xsave proof always runs.
        let has_lstar = snap.msrs.iter().any(|m| m.0 == LSTAR);
        for m in &mut snap.msrs {
            if m.0 == LSTAR {
                m.1 = SENTINEL;
            }
        }

        // Drive the live vCPU away from the snapshot: entering protected mode
        // flips CR0.PE and CS and moves rip, so the restore has real work to undo.
        backend
            .prepare_protected_mode_vcpu(0, 0x2000)
            .expect("enter protected mode");
        let diverged = backend.save_vcpu_state(0).expect("save diverged state");
        assert_ne!(diverged.regs.rip, snap.regs.rip, "rip diverged");
        assert_ne!(diverged.sregs.cr0, snap.sregs.cr0, "cr0.PE diverged");

        // Restore the baseline and confirm every class of state came back.
        backend
            .restore_vcpu_state(0, &snap)
            .expect("restore baseline state");
        let restored = backend.save_vcpu_state(0).expect("re-save after restore");
        assert_eq!(restored.regs.rip, ENTRY, "rip restored");
        assert_eq!(restored.regs.rflags, snap.regs.rflags, "rflags restored");
        assert_eq!(restored.sregs.cr0, snap.sregs.cr0, "cr0 restored");
        assert_eq!(
            restored.sregs.cs.selector, snap.sregs.cs.selector,
            "cs restored"
        );
        if has_lstar {
            let lstar = restored
                .msrs
                .iter()
                .find(|m| m.0 == LSTAR)
                .expect("lstar present after restore")
                .1;
            assert_eq!(lstar, SENTINEL, "LSTAR restored from the snapshot");
        }
        assert_eq!(
            restored.xsave.region, snap.xsave.region,
            "XSAVE extended-state area restored losslessly"
        );
    }

    #[test]
    fn protected_mode_guest_writes_the_high_lapic_mmio_page() {
        if !is_kvm_available() {
            eprintln!("skipping: /dev/kvm not available (no nested virt)");
            return;
        }

        // A 32-bit protected-mode blob that real mode could not run: store a
        // 32-bit value to the absolute LAPIC EOI register at 0xFEE000B0 (above
        // 1 MiB, unreachable from real mode), then HLT.
        //   B8 78 56 34 12   mov eax, 0x12345678
        //   A3 B0 00 E0 FE   mov [0xFEE000B0], eax   (mov moffs32, eax)
        //   F4               hlt
        #[rustfmt::skip]
        let code: [u8; 11] = [
            0xB8, 0x78, 0x56, 0x34, 0x12,
            0xA3, 0xB0, 0x00, 0xE0, 0xFE,
            0xF4,
        ];
        const ENTRY: u64 = 0x1000;
        const LAPIC_EOI_ADDR: u64 = 0xFEE0_00B0;

        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");

        let mut ram = GuestRam::new(0x1000);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu");
        backend
            .prepare_protected_mode_vcpu(0, ENTRY)
            .expect("set protected-mode entry");

        let mut handler = RecordingHandler::default();
        let mut halted = false;
        for _ in 0..16 {
            match backend.run_vcpu(0, &mut handler).expect("run vcpu") {
                GuestExit::Halted => {
                    halted = true;
                    break;
                }
                _ => continue,
            }
        }

        assert!(halted, "protected-mode guest never reached HLT");
        assert_eq!(
            handler.mmio_write,
            vec![(LAPIC_EOI_ADDR, 0x1234_5678u32.to_le_bytes().to_vec())],
            "the high LAPIC MMIO store must reach the handler as a single 4-byte write"
        );
    }

    // -- page-aligned guest RAM (no KVM required) ---------------------------

    #[cfg(target_os = "linux")]
    #[test]
    fn guest_ram_is_page_aligned_zeroed_and_writable() {
        let mut ram = GuestRam::new(100);
        // Rounded up to a whole page.
        assert_eq!(ram.len(), HOST_PAGE_SIZE);
        assert!(!ram.is_empty());
        // Page-aligned host base — the property KVM requires.
        assert_eq!(ram.host_addr() % HOST_PAGE_SIZE as u64, 0);
        // Zero-initialised.
        assert!(ram.as_slice().iter().all(|&b| b == 0));
        // Writable through the mutable slice.
        ram.as_mut_slice()[0] = 0xAB;
        ram.as_mut_slice()[HOST_PAGE_SIZE - 1] = 0xCD;
        assert_eq!(ram.as_slice()[0], 0xAB);
        assert_eq!(ram.as_slice()[HOST_PAGE_SIZE - 1], 0xCD);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn guest_ram_rounds_multi_page_size_up() {
        let ram = GuestRam::new(HOST_PAGE_SIZE + 1);
        assert_eq!(ram.len(), 2 * HOST_PAGE_SIZE);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn validate_region_rejects_misaligned_and_zero() {
        // Misaligned guest physical address.
        assert!(super::linux::validate_region(0x1001, 0x1000, 0x1000).is_err());
        // Misaligned host address.
        assert!(super::linux::validate_region(0x1000, 0x1001, 0x1000).is_err());
        // Misaligned (non-page-multiple) size.
        assert!(super::linux::validate_region(0x1000, 0x1000, 0x800).is_err());
        // Zero size.
        assert!(super::linux::validate_region(0x1000, 0x1000, 0).is_err());
        // A fully page-aligned region is accepted.
        assert!(super::linux::validate_region(0x1000, 0x2000, 0x1000).is_ok());
    }

    // -- GuestMemory DMA view (no KVM required) -----------------------------

    #[cfg(target_os = "linux")]
    #[test]
    fn guest_memory_dma_round_trips_real_guest_ram() {
        use enlil_devices::usb::xhci::transfer::DmaMemory;

        const GPA: u64 = 0x4000;
        let ram = GuestRam::new(HOST_PAGE_SIZE);
        let slots = [MemSlot {
            slot: 0,
            guest_phys_addr: GPA,
            size: ram.len() as u64,
            host_addr: ram.host_addr(),
        }];

        let mut mem = GuestMemory::new(&slots);
        assert!(mem.write(GPA + 0x10, &[0xDE, 0xAD, 0xBE, 0xEF]));
        let mut buf = [0u8; 4];
        assert!(mem.read(GPA + 0x10, &mut buf));
        assert_eq!(buf, [0xDE, 0xAD, 0xBE, 0xEF]);

        // The write landed in the backing RAM the guest actually sees.
        assert_eq!(&ram.as_slice()[0x10..0x14], &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn guest_memory_rejects_out_of_range_access() {
        use enlil_devices::usb::xhci::transfer::DmaMemory;

        const GPA: u64 = 0x4000;
        let ram = GuestRam::new(HOST_PAGE_SIZE);
        let size = ram.len() as u64;
        let slots = [MemSlot {
            slot: 0,
            guest_phys_addr: GPA,
            size,
            host_addr: ram.host_addr(),
        }];

        let mut mem = GuestMemory::new(&slots);
        let mut buf = [0u8; 8];
        // Starts below the slot.
        assert!(!mem.read(GPA - 1, &mut buf));
        // Straddles the end of the slot.
        assert!(!mem.read(GPA + size - 4, &mut buf));
        // Entirely above the slot.
        assert!(!mem.write(GPA + size, &[1, 2, 3, 4]));
        // The last 8 bytes are in range.
        assert!(mem.read(GPA + size - 8, &mut buf));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn guest_memory_routes_across_multiple_slots() {
        use enlil_devices::usb::xhci::transfer::DmaMemory;

        let ram_lo = GuestRam::new(HOST_PAGE_SIZE);
        let ram_hi = GuestRam::new(HOST_PAGE_SIZE);
        let slots = [
            MemSlot {
                slot: 0,
                guest_phys_addr: 0x1000,
                size: ram_lo.len() as u64,
                host_addr: ram_lo.host_addr(),
            },
            MemSlot {
                slot: 1,
                guest_phys_addr: 0x9000,
                size: ram_hi.len() as u64,
                host_addr: ram_hi.host_addr(),
            },
        ];

        let mut mem = GuestMemory::new(&slots);
        assert!(mem.write(0x1000, &[0xAA]));
        assert!(mem.write(0x9000, &[0xBB]));
        // The gap between the two slots is unbacked.
        assert!(!mem.write(0x5000, &[0xCC]));

        assert_eq!(ram_lo.as_slice()[0], 0xAA);
        assert_eq!(ram_hi.as_slice()[0], 0xBB);
    }

    // End-to-end DMA coherence on real KVM: a guest writes a byte into its own
    // RAM, and the host then reads that byte back through the device-facing
    // GuestMemory view built from the backend's real slot table — proving the
    // DMA view sees exactly what the guest wrote (what xHCI ring/context
    // residency relies on). Self-skips without /dev/kvm rather than faking it.
    #[cfg(target_os = "linux")]
    #[test]
    fn guest_memory_reads_what_the_guest_wrote() {
        use enlil_devices::usb::xhci::transfer::DmaMemory;

        if !is_kvm_available() {
            eprintln!("skipping guest_memory_reads_what_the_guest_wrote: no /dev/kvm");
            return;
        }

        // 16-bit real-mode blob (DS base 0): store 0x42 into guest RAM at GPA
        // 0x1800 (a normal memory write — no vmexit), then halt.
        //   B0 42      mov al, 0x42
        //   A2 00 18   mov [0x1800], al
        //   F4         hlt
        #[rustfmt::skip]
        let code: [u8; 6] = [0xB0, 0x42, 0xA2, 0x00, 0x18, 0xF4];

        const ENTRY: u64 = 0x1000;
        const SIZE: usize = 0x1000;
        const DATA_GPA: u64 = 0x1800;
        let mut ram = GuestRam::new(SIZE);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        let mut halted = false;
        for _ in 0..100 {
            if backend.run_vcpu(0, &mut NoopHandler).expect("run vcpu") == GuestExit::Halted {
                halted = true;
                break;
            }
        }
        assert!(halted, "guest never reached HLT");

        // Read the byte the guest stored, through the real backend slot table.
        let mem = backend.guest_memory();
        let mut buf = [0u8; 1];
        assert!(mem.read(DATA_GPA, &mut buf));
        assert_eq!(buf, [0x42]);
    }

    // The immediate-exit flag bounds an otherwise-unbounded run: a guest that
    // spins forever with no exit (`jmp $`) would block KVM_RUN indefinitely,
    // but with immediate-exit armed run_vcpu returns Interrupted at once. This
    // is the watchdog primitive for the in-kernel-IRQ-chip HLT-idle case.
    // Self-skips without /dev/kvm rather than faking it.
    #[cfg(target_os = "linux")]
    #[test]
    fn immediate_exit_bounds_an_unending_run() {
        if !is_kvm_available() {
            eprintln!("skipping immediate_exit_bounds_an_unending_run: no /dev/kvm");
            return;
        }

        // 16-bit real-mode `jmp $` — an infinite loop that produces no exit, so
        // without the immediate-exit bound KVM_RUN would never return.
        #[rustfmt::skip]
        let code: [u8; 2] = [0xEB, 0xFE];

        const ENTRY: u64 = 0x1000;
        const SIZE: usize = 0x1000;
        let mut ram = GuestRam::new(SIZE);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        // Arm the bound, then run: the spinning guest is kicked straight back
        // out instead of blocking the thread forever.
        backend
            .set_immediate_exit(0, true)
            .expect("arm immediate exit");
        assert_eq!(
            backend.run_vcpu(0, &mut NoopHandler).expect("run vcpu"),
            GuestExit::Interrupted
        );

        // Disarmed, the same run would re-enter the spin — so just confirm the
        // flag clears without error (the run loop disarms after a kick).
        backend
            .set_immediate_exit(0, false)
            .expect("disarm immediate exit");
    }

    // Proves the MSR exit path end-to-end on real KVM: with userspace MSR
    // forwarding on, a guest `rdmsr` of an MSR KVM does not emulate traps to
    // userspace, the handler supplies the 64-bit value, and it round-trips into
    // the guest (EDX:EAX) — the seam the Phase 5 timing/PMC stealth shadows
    // serve their spoofed APERF/MPERF/PMC values through. Self-skips when
    // /dev/kvm or the userspace-MSR cap is unavailable rather than faking it.
    #[cfg(target_os = "linux")]
    #[test]
    fn rdmsr_is_forwarded_and_value_round_trips_to_guest() {
        if !is_kvm_available() {
            eprintln!("skipping rdmsr_is_forwarded_...: no /dev/kvm");
            return;
        }

        // A handler that supplies a known MSR value and captures the byte the
        // guest echoes back out — proving the supplied value reached EAX.
        struct MsrProbe {
            supplied: u64,
            seen: Option<u32>,
            echoed: Vec<u8>,
        }
        impl VmExitHandler for MsrProbe {
            fn rdmsr(&mut self, msr: u32) -> Option<u64> {
                self.seen = Some(msr);
                Some(self.supplied)
            }
            fn io_out(&mut self, _port: u16, data: &[u8]) {
                self.echoed.extend_from_slice(data);
            }
        }

        // 16-bit real-mode blob:
        //   66 B9 78 56 34 12   mov ecx, 0x12345678  ; an MSR KVM doesn't know
        //   0F 32               rdmsr                ; edx:eax = supplied value
        //   BA F8 03            mov dx, 0x3F8        ; COM1 transmit register
        //   EE                  out dx, al           ; echo low byte of eax
        //   F4                  hlt
        #[rustfmt::skip]
        let code: [u8; 13] = [
            0x66, 0xB9, 0x78, 0x56, 0x34, 0x12,
            0x0F, 0x32,
            0xBA, 0xF8, 0x03,
            0xEE,
            0xF4,
        ];

        const ENTRY: u64 = 0x1000;
        const SIZE: usize = 0x1000;
        const MSR: u32 = 0x1234_5678;
        let mut ram = GuestRam::new(SIZE);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        // Forwarding the cap must precede vCPU creation. If the host kernel
        // lacks it, skip honestly rather than failing.
        if let Err(e) = backend.enable_userspace_msr_exits() {
            eprintln!("skipping rdmsr_is_forwarded_...: {e}");
            return;
        }
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        let mut probe = MsrProbe {
            supplied: 0x0000_0000_0000_00AB,
            seen: None,
            echoed: Vec::new(),
        };

        let mut halted = false;
        let mut saw_msr_exit = false;
        for _ in 0..100 {
            match backend.run_vcpu(0, &mut probe).expect("run vcpu") {
                GuestExit::Halted => {
                    halted = true;
                    break;
                }
                GuestExit::MsrRead { msr } => {
                    assert_eq!(msr, MSR, "the forwarded MSR index must reach the handler");
                    saw_msr_exit = true;
                }
                _ => {}
            }
        }
        assert!(halted, "guest never reached HLT");
        assert!(saw_msr_exit, "rdmsr never trapped to userspace");
        assert_eq!(probe.seen, Some(MSR));
        // The low byte of the supplied MSR value reached EAX and was echoed.
        assert_eq!(probe.echoed, vec![0xAB]);
    }

    // End-to-end proof that the AMD PMC MSR surface added to the stealth router
    // is actually forwarded and served on the live guest: forwarding *exactly*
    // `StealthMsrRouter::filter_ranges()` for an AMD platform, a guest `rdmsr` of
    // the AMD PerfMonV2 core PerfCtr0 (0xC0010201 — which KVM otherwise emulates
    // in-kernel) traps to userspace and the router serves its model-driven shadow
    // value (not KVM's overhead-revealing one). Self-skips without /dev/kvm or
    // the userspace-MSR cap rather than faking it.
    #[cfg(target_os = "linux")]
    #[test]
    fn amd_perfmon_v2_counter_is_forwarded_and_serves_the_router_shadow() {
        use crate::stealth_msr::StealthMsrRouter;
        use crate::timing_stealth::VcpuTimingState;
        use enlil_devices::stealth::lbr::{LbrPlatform, LbrState};
        use enlil_devices::stealth::pmc::msr as pmc_msr;

        if !is_kvm_available() {
            eprintln!("skipping amd_perfmon_v2_counter_...: no /dev/kvm");
            return;
        }

        // An AMD-platform router with a known shadow value in PerfCtr0 (index 0).
        let mut router =
            StealthMsrRouter::new(VcpuTimingState::new(), LbrState::new(LbrPlatform::AmdSvm));
        router.pmc.gp_counters[0] = 0x0000_0000_0000_00C7; // low byte 0xC7

        // The handler routes the guest's rdmsr through the router and echoes the
        // low byte the router supplied.
        struct RouterHandler {
            router: StealthMsrRouter,
            echoed: Vec<u8>,
        }
        impl VmExitHandler for RouterHandler {
            fn rdmsr(&mut self, msr: u32) -> Option<u64> {
                self.router.read_msr(msr)
            }
            fn io_out(&mut self, _port: u16, data: &[u8]) {
                self.echoed.extend_from_slice(data);
            }
        }

        // 16-bit real-mode blob:
        //   66 B9 01 02 01 C0   mov ecx, 0xC0010201  ; AMD PerfMonV2 PerfCtr0
        //   0F 32               rdmsr                ; edx:eax = router shadow
        //   BA F8 03            mov dx, 0x3F8        ; COM1 transmit register
        //   EE                  out dx, al           ; echo low byte of eax
        //   F4                  hlt
        #[rustfmt::skip]
        let code: [u8; 13] = [
            0x66, 0xB9, 0x01, 0x02, 0x01, 0xC0,
            0x0F, 0x32,
            0xBA, 0xF8, 0x03,
            0xEE,
            0xF4,
        ];

        const ENTRY: u64 = 0x1000;
        const SIZE: usize = 0x1000;
        let mut ram = GuestRam::new(SIZE);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        if let Err(e) = backend.enable_userspace_msr_exits() {
            eprintln!("skipping amd_perfmon_v2_counter_...: {e}");
            return;
        }
        // Forward exactly what the AMD router serves — this is what proves the
        // AMD PerfCtr address really is one of the ranges the router emits.
        let ranges = router.filter_ranges();
        assert!(
            ranges.iter().any(
                |&(b, c)| pmc_msr::AMD_CORE_PERFCTR0 >= b && pmc_msr::AMD_CORE_PERFCTR0 < b + c
            ),
            "AMD PerfCtr0 must be in the AMD router's filter ranges"
        );
        if let Err(e) = backend.forward_msrs_to_userspace(&ranges) {
            eprintln!("skipping amd_perfmon_v2_counter_...: {e}");
            return;
        }
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        let mut handler = RouterHandler {
            router,
            echoed: Vec::new(),
        };

        let mut halted = false;
        let mut saw_msr = false;
        for _ in 0..100 {
            match backend.run_vcpu(0, &mut handler).expect("run vcpu") {
                GuestExit::Halted => {
                    halted = true;
                    break;
                }
                GuestExit::MsrRead { msr } => {
                    assert_eq!(
                        msr,
                        pmc_msr::AMD_CORE_PERFCTR0,
                        "the forwarded AMD PerfCtr MSR must reach the handler"
                    );
                    saw_msr = true;
                }
                _ => {}
            }
        }
        assert!(halted, "guest never reached HLT");
        assert!(saw_msr, "AMD PerfCtr0 rdmsr never trapped to userspace");
        // The router's shadow value (low byte 0xC7) reached EAX and was echoed —
        // KVM's in-kernel PMU emulation was overridden by our forwarding.
        assert_eq!(handler.echoed, vec![0xC7]);
    }

    // The guest WRMSR path: every other forwarding test reads MSRs, but a write
    // must reach the router too (GuestExit::MsrWrite -> VmExitHandler::wrmsr). A
    // guest that WRMSRs IA32_DEBUGCTL then RDMSRs it back gets its own written
    // value (proving the write landed in the router's LbrState), and the router
    // observed the LBR-enable bit. Self-skips without /dev/kvm or the
    // userspace-MSR cap rather than faking it.
    #[cfg(target_os = "linux")]
    #[test]
    fn wrmsr_is_forwarded_and_the_written_value_round_trips() {
        use crate::stealth_msr::StealthMsrRouter;
        use crate::timing_stealth::VcpuTimingState;
        use enlil_devices::stealth::lbr::{intel_msr, LbrPlatform, LbrState};

        if !is_kvm_available() {
            eprintln!("skipping wrmsr_is_forwarded_...: no /dev/kvm");
            return;
        }

        struct RouterHandler {
            router: StealthMsrRouter,
            echoed: Vec<u8>,
        }
        impl VmExitHandler for RouterHandler {
            fn rdmsr(&mut self, msr: u32) -> Option<u64> {
                self.router.read_msr(msr)
            }
            fn wrmsr(&mut self, msr: u32, value: u64) -> bool {
                self.router.write_msr(msr, value)
            }
            fn io_out(&mut self, _port: u16, data: &[u8]) {
                self.echoed.extend_from_slice(data);
            }
        }

        // 16-bit real-mode blob:
        //   66 B9 D9 01 00 00   mov ecx, 0x1D9   ; IA32_DEBUGCTL
        //   66 B8 09 00 00 00   mov eax, 0x09    ; LBR-enable (bit 0) + bit 3
        //   66 BA 00 00 00 00   mov edx, 0
        //   0F 30               wrmsr            ; DEBUGCTL = 0x09
        //   0F 32               rdmsr            ; read it back -> edx:eax
        //   BA F8 03            mov dx, 0x3F8    ; COM1
        //   EE                  out dx, al       ; echo low byte (0x09)
        //   F4                  hlt
        #[rustfmt::skip]
        let code: [u8; 27] = [
            0x66, 0xB9, 0xD9, 0x01, 0x00, 0x00,
            0x66, 0xB8, 0x09, 0x00, 0x00, 0x00,
            0x66, 0xBA, 0x00, 0x00, 0x00, 0x00,
            0x0F, 0x30,
            0x0F, 0x32,
            0xBA, 0xF8, 0x03,
            0xEE,
            0xF4,
        ];
        const ENTRY: u64 = 0x1000;

        let router =
            StealthMsrRouter::new(VcpuTimingState::new(), LbrState::new(LbrPlatform::IntelVmx));

        let mut ram = GuestRam::new(0x1000);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        if let Err(e) = backend.enable_userspace_msr_exits() {
            eprintln!("skipping wrmsr_is_forwarded_...: {e}");
            return;
        }
        let ranges = router.filter_ranges();
        if let Err(e) = backend.forward_msrs_to_userspace(&ranges) {
            eprintln!("skipping wrmsr_is_forwarded_...: {e}");
            return;
        }
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        let mut handler = RouterHandler {
            router,
            echoed: Vec::new(),
        };

        let mut halted = false;
        let mut saw_write = false;
        for _ in 0..100 {
            match backend.run_vcpu(0, &mut handler).expect("run vcpu") {
                GuestExit::Halted => {
                    halted = true;
                    break;
                }
                GuestExit::MsrWrite { msr, value } => {
                    assert_eq!(msr, intel_msr::IA32_DEBUGCTL);
                    assert_eq!(value, 0x09);
                    saw_write = true;
                }
                _ => {}
            }
        }
        assert!(halted, "guest never reached HLT");
        assert!(saw_write, "wrmsr never trapped to userspace");
        // The written DEBUGCTL value round-tripped back into the guest...
        assert_eq!(handler.echoed, vec![0x09]);
        // ...and the router's LbrState recorded the write (LBR-enable observed).
        assert_eq!(handler.router.lbr.read_debug_ctl(), 0x09);
        assert!(handler.router.lbr.lbr_enabled);
    }

    // The AMD last-branch MSRs are forwarded and served on the live guest: an
    // anti-cheat reads LastBranchFromIP (0x1DB) after forcing a VMEXIT to spot a
    // branch into the hypervisor. Forwarding the AMD router's filter ranges, a
    // guest rdmsr of 0x1DB (which KVM otherwise emulates) traps to userspace and
    // reads the router's sanitized LbrState shadow, not a real branch record.
    // Self-skips without /dev/kvm or the userspace-MSR cap rather than faking it.
    #[cfg(target_os = "linux")]
    #[test]
    fn amd_last_branch_msr_is_forwarded_and_serves_the_router_shadow() {
        use crate::stealth_msr::StealthMsrRouter;
        use crate::timing_stealth::VcpuTimingState;
        use enlil_devices::stealth::lbr::{amd_msr, LbrPlatform, LbrState};

        if !is_kvm_available() {
            eprintln!("skipping amd_last_branch_msr_...: no /dev/kvm");
            return;
        }

        struct RouterHandler {
            router: StealthMsrRouter,
            echoed: Vec<u8>,
        }
        impl VmExitHandler for RouterHandler {
            fn rdmsr(&mut self, msr: u32) -> Option<u64> {
                self.router.read_msr(msr)
            }
            fn io_out(&mut self, _port: u16, data: &[u8]) {
                self.echoed.extend_from_slice(data);
            }
        }

        // An AMD-platform router whose LastBranchFromIP shadow has low byte 0x3E.
        let mut router =
            StealthMsrRouter::new(VcpuTimingState::new(), LbrState::new(LbrPlatform::AmdSvm));
        assert!(router
            .lbr
            .write_amd_lbr(amd_msr::LAST_BRANCH_FROM_IP, 0x0000_0000_0000_003E));

        // 16-bit real-mode blob:
        //   66 B9 DB 01 00 00   mov ecx, 0x1DB   ; AMD LastBranchFromIP
        //   0F 32               rdmsr
        //   BA F8 03            mov dx, 0x3F8
        //   EE                  out dx, al
        //   F4                  hlt
        #[rustfmt::skip]
        let code: [u8; 13] = [
            0x66, 0xB9, 0xDB, 0x01, 0x00, 0x00,
            0x0F, 0x32,
            0xBA, 0xF8, 0x03,
            0xEE,
            0xF4,
        ];
        const ENTRY: u64 = 0x1000;

        let mut ram = GuestRam::new(0x1000);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        if let Err(e) = backend.enable_userspace_msr_exits() {
            eprintln!("skipping amd_last_branch_msr_...: {e}");
            return;
        }
        let ranges = router.filter_ranges();
        assert!(
            ranges
                .iter()
                .any(|&(b, c)| amd_msr::LAST_BRANCH_FROM_IP >= b
                    && amd_msr::LAST_BRANCH_FROM_IP < b + c),
            "the AMD last-branch pair must be in the AMD router's filter ranges"
        );
        if let Err(e) = backend.forward_msrs_to_userspace(&ranges) {
            eprintln!("skipping amd_last_branch_msr_...: {e}");
            return;
        }
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        let mut handler = RouterHandler {
            router,
            echoed: Vec::new(),
        };

        let mut halted = false;
        let mut saw_msr = false;
        for _ in 0..100 {
            match backend.run_vcpu(0, &mut handler).expect("run vcpu") {
                GuestExit::Halted => {
                    halted = true;
                    break;
                }
                GuestExit::MsrRead { msr } => {
                    assert_eq!(msr, amd_msr::LAST_BRANCH_FROM_IP);
                    saw_msr = true;
                }
                _ => {}
            }
        }
        assert!(halted, "guest never reached HLT");
        assert!(
            saw_msr,
            "AMD LastBranchFromIP rdmsr never trapped to userspace"
        );
        // The router's shadow (low byte 0x3E) reached the guest.
        assert_eq!(handler.echoed, vec![0x3E]);
    }

    // Intel-side symmetry for the live forwarding coverage: a guest rdmsr of
    // IA32_PMC0 (0xC1) on an IntelVmx router traps to userspace through the
    // Intel PMC filter range and reads the router's shadow counter. Self-skips
    // without /dev/kvm or the userspace-MSR cap.
    #[cfg(target_os = "linux")]
    #[test]
    fn intel_pmc_msr_is_forwarded_and_serves_the_router_shadow() {
        use crate::stealth_msr::StealthMsrRouter;
        use crate::timing_stealth::VcpuTimingState;
        use enlil_devices::stealth::lbr::{LbrPlatform, LbrState};
        use enlil_devices::stealth::pmc::msr as pmc_msr;

        if !is_kvm_available() {
            eprintln!("skipping intel_pmc_msr_...: no /dev/kvm");
            return;
        }

        struct RouterHandler {
            router: StealthMsrRouter,
            echoed: Vec<u8>,
        }
        impl VmExitHandler for RouterHandler {
            fn rdmsr(&mut self, msr: u32) -> Option<u64> {
                self.router.read_msr(msr)
            }
            fn io_out(&mut self, _port: u16, data: &[u8]) {
                self.echoed.extend_from_slice(data);
            }
        }

        let mut router =
            StealthMsrRouter::new(VcpuTimingState::new(), LbrState::new(LbrPlatform::IntelVmx));
        router.pmc.gp_counters[0] = 0x0000_0000_0000_0071; // low byte 0x71

        // mov ecx, 0xC1 (IA32_PMC0); rdmsr; out 0x3F8, al; hlt.
        #[rustfmt::skip]
        let code: [u8; 13] = [
            0x66, 0xB9, 0xC1, 0x00, 0x00, 0x00,
            0x0F, 0x32,
            0xBA, 0xF8, 0x03,
            0xEE,
            0xF4,
        ];
        const ENTRY: u64 = 0x1000;

        let mut ram = GuestRam::new(0x1000);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        if let Err(e) = backend.enable_userspace_msr_exits() {
            eprintln!("skipping intel_pmc_msr_...: {e}");
            return;
        }
        let ranges = router.filter_ranges();
        assert!(
            ranges
                .iter()
                .any(|&(b, c)| pmc_msr::IA32_PMC0 >= b && pmc_msr::IA32_PMC0 < b + c),
            "IA32_PMC0 must be in the Intel router's filter ranges"
        );
        if let Err(e) = backend.forward_msrs_to_userspace(&ranges) {
            eprintln!("skipping intel_pmc_msr_...: {e}");
            return;
        }
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        let mut handler = RouterHandler {
            router,
            echoed: Vec::new(),
        };

        let mut halted = false;
        let mut saw_msr = false;
        for _ in 0..100 {
            match backend.run_vcpu(0, &mut handler).expect("run vcpu") {
                GuestExit::Halted => {
                    halted = true;
                    break;
                }
                GuestExit::MsrRead { msr } => {
                    assert_eq!(msr, pmc_msr::IA32_PMC0);
                    saw_msr = true;
                }
                _ => {}
            }
        }
        assert!(halted, "guest never reached HLT");
        assert!(saw_msr, "IA32_PMC0 rdmsr never trapped to userspace");
        assert_eq!(handler.echoed, vec![0x71]);
    }

    // Intel LBR stack symmetry: a guest rdmsr of LBR_FROM_BASE (0x680, the first
    // MSR_LASTBRANCH_*_FROM_IP) on an IntelVmx router traps through the Intel LBR
    // filter block and reads the router's sanitized stack shadow. Self-skips
    // without /dev/kvm or the userspace-MSR cap.
    #[cfg(target_os = "linux")]
    #[test]
    fn intel_lbr_stack_msr_is_forwarded_and_serves_the_router_shadow() {
        use crate::stealth_msr::StealthMsrRouter;
        use crate::timing_stealth::VcpuTimingState;
        use enlil_devices::stealth::lbr::{intel_msr, LbrPlatform, LbrState};

        if !is_kvm_available() {
            eprintln!("skipping intel_lbr_stack_msr_...: no /dev/kvm");
            return;
        }

        struct RouterHandler {
            router: StealthMsrRouter,
            echoed: Vec<u8>,
        }
        impl VmExitHandler for RouterHandler {
            fn rdmsr(&mut self, msr: u32) -> Option<u64> {
                self.router.read_msr(msr)
            }
            fn io_out(&mut self, _port: u16, data: &[u8]) {
                self.echoed.extend_from_slice(data);
            }
        }

        let mut router =
            StealthMsrRouter::new(VcpuTimingState::new(), LbrState::new(LbrPlatform::IntelVmx));
        router.lbr.from_addresses[0] = 0x0000_0000_0000_005C; // low byte 0x5C

        // mov ecx, 0x680 (LBR_FROM_BASE); rdmsr; out 0x3F8, al; hlt.
        #[rustfmt::skip]
        let code: [u8; 13] = [
            0x66, 0xB9, 0x80, 0x06, 0x00, 0x00,
            0x0F, 0x32,
            0xBA, 0xF8, 0x03,
            0xEE,
            0xF4,
        ];
        const ENTRY: u64 = 0x1000;

        let mut ram = GuestRam::new(0x1000);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        if let Err(e) = backend.enable_userspace_msr_exits() {
            eprintln!("skipping intel_lbr_stack_msr_...: {e}");
            return;
        }
        let ranges = router.filter_ranges();
        assert!(
            ranges
                .iter()
                .any(|&(b, c)| intel_msr::LBR_FROM_BASE >= b && intel_msr::LBR_FROM_BASE < b + c),
            "LBR_FROM_BASE must be in the Intel router's filter ranges"
        );
        if let Err(e) = backend.forward_msrs_to_userspace(&ranges) {
            eprintln!("skipping intel_lbr_stack_msr_...: {e}");
            return;
        }
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        let mut handler = RouterHandler {
            router,
            echoed: Vec::new(),
        };

        let mut halted = false;
        let mut saw_msr = false;
        for _ in 0..100 {
            match backend.run_vcpu(0, &mut handler).expect("run vcpu") {
                GuestExit::Halted => {
                    halted = true;
                    break;
                }
                GuestExit::MsrRead { msr } => {
                    assert_eq!(msr, intel_msr::LBR_FROM_BASE);
                    saw_msr = true;
                }
                _ => {}
            }
        }
        assert!(halted, "guest never reached HLT");
        assert!(saw_msr, "LBR_FROM_BASE rdmsr never trapped to userspace");
        assert_eq!(handler.echoed, vec![0x5C]);
    }

    // Proves clear_cpuid_hypervisor_bit() installs the host's *real* feature
    // set on the guest with the hypervisor-present tell cleared: after applying
    // it, a guest running CPUID leaf 1 reads ECX bit 31 (hypervisor present) as
    // 0 *and* EDX bit 4 (TSC) as 1 — i.e. it sees genuine CPU features, not an
    // empty CPUID, and no VM tell. (In this minimal VM KVM does not set the
    // hypervisor bit by default, so the value of this call is installing the
    // supported feature set with the bit guaranteed clear, not flipping a 1.)
    // Self-skips without /dev/kvm rather than faking it.
    #[cfg(target_os = "linux")]
    #[test]
    fn cpuid_stealth_installs_real_features_without_the_hypervisor_tell() {
        if !is_kvm_available() {
            eprintln!("skipping cpuid_stealth_installs_real_features_...: no /dev/kvm");
            return;
        }

        // 16-bit real-mode blob; echoes two status bytes to COM1:
        //   66 B8 01 00 00 00   mov eax, 1      ; CPUID leaf 1
        //   0F A2               cpuid
        //   66 C1 E9 1F         shr ecx, 31     ; ecx = hypervisor-present bit
        //   88 C8               mov al, cl
        //   BA F8 03            mov dx, 0x3F8   ; COM1
        //   EE                  out dx, al      ; byte 0: hypervisor bit
        //   66 C1 EA 04         shr edx, 4      ; edx bit0 = TSC feature (EDX[4])
        //   80 E2 01            and dl, 1
        //   88 D0               mov al, dl
        //   EE                  out dx, al      ; byte 1: TSC bit
        //   F4                  hlt
        #[rustfmt::skip]
        let code: [u8; 29] = [
            0x66, 0xB8, 0x01, 0x00, 0x00, 0x00,
            0x0F, 0xA2,
            0x66, 0xC1, 0xE9, 0x1F,
            0x88, 0xC8,
            0xBA, 0xF8, 0x03,
            0xEE,
            0x66, 0xC1, 0xEA, 0x04,
            0x80, 0xE2, 0x01,
            0x88, 0xD0,
            0xEE,
            0xF4,
        ];

        const ENTRY: u64 = 0x1000;
        const SIZE: usize = 0x1000;
        let mut ram = GuestRam::new(SIZE);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu");
        backend
            .clear_cpuid_hypervisor_bit()
            .expect("apply cpuid stealth");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        struct EchoOut(Vec<u8>);
        impl VmExitHandler for EchoOut {
            fn io_out(&mut self, _port: u16, data: &[u8]) {
                self.0.extend_from_slice(data);
            }
        }
        let mut echo = EchoOut(Vec::new());

        let mut halted = false;
        for _ in 0..100 {
            if backend.run_vcpu(0, &mut echo).expect("run vcpu") == GuestExit::Halted {
                halted = true;
                break;
            }
        }
        assert!(halted, "guest never reached HLT");
        // byte 0: hypervisor-present bit clear; byte 1: TSC feature present.
        assert_eq!(echo.0, vec![0, 1]);
    }

    #[test]
    fn cpuid_stealth_preserves_invariant_tsc() {
        // Transparency (LOCKED PRINCIPLE 1): a modern CPU always advertises
        // Invariant TSC (CPUID 0x8000_0007:EDX[8]); a guest that finds it CLEAR
        // has a detection tell. apply_topology_stealth patches KVM's supported
        // CPUID rather than rebuilding from the (0x8000_0007-less) stealth
        // table, so the host's Invariant-TSC bit must survive to the guest.
        if !is_kvm_available() {
            eprintln!("skipping cpuid_stealth_preserves_invariant_tsc: no /dev/kvm");
            return;
        }
        // __cpuid is safe on baseline x86_64 (CPUID is always available).
        let host_invariant_tsc = core::arch::x86_64::__cpuid(0x8000_0007).edx & (1 << 8);
        if host_invariant_tsc == 0 {
            eprintln!(
                "skipping cpuid_stealth_preserves_invariant_tsc: host does not expose Invariant TSC"
            );
            return;
        }

        // 16-bit real-mode blob: read CPUID 0x8000_0007 and emit EDX[8] on COM1.
        //   66 B8 07 00 00 80   mov eax, 0x80000007
        //   0F A2               cpuid
        //   66 C1 EA 08         shr edx, 8       ; edx bit0 = Invariant TSC (EDX[8])
        //   80 E2 01            and dl, 1
        //   88 D0               mov al, dl
        //   BA F8 03            mov dx, 0x3F8    ; COM1
        //   EE                  out dx, al
        //   F4                  hlt
        #[rustfmt::skip]
        let code: [u8; 22] = [
            0x66, 0xB8, 0x07, 0x00, 0x00, 0x80,
            0x0F, 0xA2,
            0x66, 0xC1, 0xEA, 0x08,
            0x80, 0xE2, 0x01,
            0x88, 0xD0,
            0xBA, 0xF8, 0x03,
            0xEE,
            0xF4,
        ];

        const ENTRY: u64 = 0x1000;
        const SIZE: usize = 0x1000;
        let mut ram = GuestRam::new(SIZE);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu");
        // The exact stealth the orchestrator applies before a guest runs.
        let table = enlil_devices::stealth::cpuid::CpuidStealthTable::build(
            &enlil_devices::stealth::cpuid::CpuidStealthConfig::from_host(1, 1),
        );
        backend
            .apply_topology_stealth(&table)
            .expect("apply cpuid stealth");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        struct EchoOut(Vec<u8>);
        impl VmExitHandler for EchoOut {
            fn io_out(&mut self, _port: u16, data: &[u8]) {
                self.0.extend_from_slice(data);
            }
        }
        let mut echo = EchoOut(Vec::new());

        let mut halted = false;
        for _ in 0..100 {
            if backend.run_vcpu(0, &mut echo).expect("run vcpu") == GuestExit::Halted {
                halted = true;
                break;
            }
        }
        assert!(halted, "guest never reached HLT");
        assert_eq!(
            echo.0,
            vec![1],
            "guest must see Invariant TSC (CPUID 0x8000_0007:EDX[8]) preserved through CPUID stealth"
        );
    }

    #[test]
    fn guest_cpuid_hypervisor_leaf_exposes_no_vendor_signature() {
        // Transparency (LOCKED PRINCIPLE 1): the CPUID 0x40000000 hypervisor
        // vendor leaf must NOT leak a signature like "KVMKVMKVM" to the guest.
        // apply_topology_stealth builds from KVM_GET_SUPPORTED_CPUID (which has
        // no 0x40000000 entry), so the guest should read the leaf clean. Verify
        // by reading it inside a real guest and decoding with the same routine
        // an in-guest detector (5.8) would use.
        if !is_kvm_available() {
            eprintln!("skipping guest_cpuid_hypervisor_leaf_...: no /dev/kvm");
            return;
        }

        // Real-mode blob: CPUID(0x40000000), then emit EBX, ECX, EDX as 12
        // bytes (LSB first) on COM1, then HLT. EDX is copied into EBX before
        // emission because DX holds the port and shifting EDX would clobber it.
        #[rustfmt::skip]
        let code: [u8; 87] = [
            0x66, 0xB8, 0x00, 0x00, 0x00, 0x40, // mov eax, 0x40000000
            0x0F, 0xA2,                         // cpuid
            0xBA, 0xF8, 0x03,                   // mov dx, 0x3F8
            // emit EBX (4 bytes, LSB first)
            0x88, 0xD8, 0xEE,                   // mov al, bl; out dx, al
            0x66, 0xC1, 0xEB, 0x08, 0x88, 0xD8, 0xEE, // shr ebx,8; mov al,bl; out
            0x66, 0xC1, 0xEB, 0x08, 0x88, 0xD8, 0xEE,
            0x66, 0xC1, 0xEB, 0x08, 0x88, 0xD8, 0xEE,
            // emit ECX (4 bytes, LSB first)
            0x88, 0xC8, 0xEE,                   // mov al, cl; out
            0x66, 0xC1, 0xE9, 0x08, 0x88, 0xC8, 0xEE, // shr ecx,8; mov al,cl; out
            0x66, 0xC1, 0xE9, 0x08, 0x88, 0xC8, 0xEE,
            0x66, 0xC1, 0xE9, 0x08, 0x88, 0xC8, 0xEE,
            0x66, 0x89, 0xD3,                   // mov ebx, edx
            // emit EDX (now in EBX; 4 bytes, LSB first)
            0x88, 0xD8, 0xEE,
            0x66, 0xC1, 0xEB, 0x08, 0x88, 0xD8, 0xEE,
            0x66, 0xC1, 0xEB, 0x08, 0x88, 0xD8, 0xEE,
            0x66, 0xC1, 0xEB, 0x08, 0x88, 0xD8, 0xEE,
            0xF4,                               // hlt
        ];

        const ENTRY: u64 = 0x1000;
        const SIZE: usize = 0x1000;
        let mut ram = GuestRam::new(SIZE);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu");
        let table = enlil_devices::stealth::cpuid::CpuidStealthTable::build(
            &enlil_devices::stealth::cpuid::CpuidStealthConfig::from_host(1, 1),
        );
        backend
            .apply_topology_stealth(&table)
            .expect("apply cpuid stealth");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        struct EchoOut(Vec<u8>);
        impl VmExitHandler for EchoOut {
            fn io_out(&mut self, _port: u16, data: &[u8]) {
                self.0.extend_from_slice(data);
            }
        }
        let mut echo = EchoOut(Vec::new());

        let mut halted = false;
        for _ in 0..200 {
            if backend.run_vcpu(0, &mut echo).expect("run vcpu") == GuestExit::Halted {
                halted = true;
                break;
            }
        }
        assert!(halted, "guest never reached HLT");
        assert_eq!(echo.0.len(), 12, "guest must emit all 12 signature bytes");

        let ebx = u32::from_le_bytes(echo.0[0..4].try_into().unwrap());
        let ecx = u32::from_le_bytes(echo.0[4..8].try_into().unwrap());
        let edx = u32::from_le_bytes(echo.0[8..12].try_into().unwrap());
        assert_eq!(
            enlil_devices::stealth::detection::hypervisor_vendor_from_signature(ebx, ecx, edx),
            None,
            "CPUID 0x40000000 leaked a hypervisor vendor signature to the guest: {:?}",
            core::str::from_utf8(&echo.0)
        );
    }

    // apply_topology_stealth makes a guest see ITS OWN topology, not the host's.
    // KVM's supported leaf 0xB mirrors the host's logical-processor count; a
    // 2-vCPU guest reading leaf 0xB subleaf 1 EBX would otherwise find the host's
    // (much larger) count — a VM tell. After applying a table built for 2 vCPUs,
    // the guest's cpuid(0xB, 1).EBX reads exactly 2. Self-skips without /dev/kvm.
    #[cfg(target_os = "linux")]
    #[test]
    fn topology_stealth_makes_the_guest_see_its_own_cpu_count() {
        use enlil_devices::stealth::cpuid::{CpuidStealthConfig, CpuidStealthTable};

        if !is_kvm_available() {
            eprintln!(
                "skipping topology_stealth_makes_the_guest_see_its_own_cpu_count: no /dev/kvm"
            );
            return;
        }

        // 16-bit real-mode blob; reads leaf 0xB subleaf 1 and echoes EBX low byte:
        //   66 B8 0B 00 00 00   mov eax, 0xB    ; extended topology leaf
        //   66 B9 01 00 00 00   mov ecx, 1      ; subleaf 1 (core level)
        //   0F A2               cpuid
        //   88 D8               mov al, bl      ; al = EBX[7:0] = logical-proc count
        //   BA F8 03            mov dx, 0x3F8   ; COM1
        //   EE                  out dx, al
        //   F4                  hlt
        #[rustfmt::skip]
        let code: [u8; 21] = [
            0x66, 0xB8, 0x0B, 0x00, 0x00, 0x00,
            0x66, 0xB9, 0x01, 0x00, 0x00, 0x00,
            0x0F, 0xA2,
            0x88, 0xD8,
            0xBA, 0xF8, 0x03,
            0xEE,
            0xF4,
        ];

        const ENTRY: u64 = 0x1000;
        const SIZE: usize = 0x1000;
        const GUEST_VCPUS: u32 = 2;
        let mut ram = GuestRam::new(SIZE);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        // Two vCPUs, so the guest topology (2) differs from the host's count.
        backend.create_vcpu(0).expect("create vcpu 0");
        backend.create_vcpu(1).expect("create vcpu 1");
        // Build the table for the guest's topology and apply it.
        let table = CpuidStealthTable::build(&CpuidStealthConfig::from_host(GUEST_VCPUS, 1));
        backend
            .apply_topology_stealth(&table)
            .expect("apply topology stealth");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        struct EchoOut(Vec<u8>);
        impl VmExitHandler for EchoOut {
            fn io_out(&mut self, _port: u16, data: &[u8]) {
                self.0.extend_from_slice(data);
            }
        }
        let mut echo = EchoOut(Vec::new());

        let mut halted = false;
        for _ in 0..100 {
            if backend.run_vcpu(0, &mut echo).expect("run vcpu") == GuestExit::Halted {
                halted = true;
                break;
            }
        }
        assert!(halted, "guest never reached HLT");
        // The guest reports its own 2-vCPU topology, regardless of the host's
        // real logical-processor count.
        assert_eq!(
            echo.0,
            vec![GUEST_VCPUS as u8],
            "leaf 0xB subleaf 1 EBX should be the guest vCPU count, not the host's"
        );
    }

    // apply_pmu_stealth makes a guest enumerate an architectural PMU (leaf 0xA)
    // consistent with the RDPMC shadow it is served. On this AMD host KVM omits
    // leaf 0xA from KVM_GET_SUPPORTED_CPUID, so without the override a guest
    // reads it back as all-zero — "PMU version 0", the cloud/VM tell. After
    // applying an Intel-vendor table whose leaf 0xA advertises PMU version 5,
    // the guest's cpuid(0xA).EAX[7:0] reads exactly 5. The test is therefore
    // non-vacuous on AMD: the 5 can only come from our injected leaf, not the
    // host. Self-skips without /dev/kvm rather than faking it.
    #[cfg(target_os = "linux")]
    #[test]
    fn pmu_stealth_makes_the_guest_see_an_architectural_pmu() {
        use enlil_devices::stealth::cpuid::{CpuVendor, CpuidStealthConfig, CpuidStealthTable};

        if !is_kvm_available() {
            eprintln!("skipping pmu_stealth_makes_the_guest_see_an_architectural_pmu: no /dev/kvm");
            return;
        }

        // 16-bit real-mode blob; echoes the PMU version then the hypervisor bit:
        //   66 B8 0A 00 00 00   mov eax, 0xA    ; architectural-PMU leaf
        //   0F A2               cpuid           ; al = EAX[7:0] = PMU version
        //   BA F8 03            mov dx, 0x3F8   ; COM1
        //   EE                  out dx, al      ; echo PMU version
        //   66 B8 01 00 00 00   mov eax, 1      ; feature leaf
        //   0F A2               cpuid
        //   66 C1 E9 1F         shr ecx, 31     ; cl = ECX[31] = hypervisor bit
        //   88 C8               mov al, cl
        //   EE                  out dx, al      ; echo hypervisor bit
        //   F4                  hlt
        #[rustfmt::skip]
        let code: [u8; 28] = [
            0x66, 0xB8, 0x0A, 0x00, 0x00, 0x00,
            0x0F, 0xA2,
            0xBA, 0xF8, 0x03,
            0xEE,
            0x66, 0xB8, 0x01, 0x00, 0x00, 0x00,
            0x0F, 0xA2,
            0x66, 0xC1, 0xE9, 0x1F,
            0x88, 0xC8,
            0xEE,
            0xF4,
        ];

        const ENTRY: u64 = 0x1000;
        const SIZE: usize = 0x1000;
        let mut ram = GuestRam::new(SIZE);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu 0");

        // Present the guest as Intel so the table populates leaf 0xA (it is
        // reserved-zero for an AMD-vendor table). Only leaf 0xA is consulted by
        // apply_pmu_stealth, so the rest of the (host-derived) table is moot here.
        let mut config = CpuidStealthConfig::from_host(1, 1);
        config.vendor = CpuVendor::Intel;
        let table = CpuidStealthTable::build(&config);
        // Sanity: the table really does advertise a non-zero PMU version.
        assert_eq!(table.lookup(0xA, 0).eax & 0xFF, 5, "table PMU version");
        backend
            .apply_pmu_stealth(&table)
            .expect("apply pmu stealth");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        struct EchoOut(Vec<u8>);
        impl VmExitHandler for EchoOut {
            fn io_out(&mut self, _port: u16, data: &[u8]) {
                self.0.extend_from_slice(data);
            }
        }
        let mut echo = EchoOut(Vec::new());

        let mut halted = false;
        for _ in 0..100 {
            if backend.run_vcpu(0, &mut echo).expect("run vcpu") == GuestExit::Halted {
                halted = true;
                break;
            }
        }
        assert!(halted, "guest never reached HLT");
        // The guest reads PMU version 5 from leaf 0xA — which KVM would have
        // reported as 0 on this AMD host without the injected leaf — and the
        // hypervisor-present bit is cleared, so a standalone PMU install is not
        // self-contradicting (real PMU, yet "I'm a hypervisor").
        assert_eq!(
            echo.0,
            vec![5, 0],
            "leaf 0xA PMU version (5) then leaf-1 ECX[31] hypervisor bit (0)"
        );
    }

    // apply_topology_stealth is the full CPUID-stealth path: a single call must
    // give the guest BOTH its own topology (leaf 0xB) AND a consistent
    // architectural PMU (leaf 0xA) in one rebuild. With an Intel-vendor 2-vCPU
    // table, the guest reads leaf 0xB subleaf 1 EBX == 2 and leaf 0xA
    // EAX[7:0] == 5 from the same install. On this AMD host both values can only
    // come from our injected leaves (KVM reports the host's count for 0xB and
    // omits 0xA entirely), so the test is non-vacuous. Self-skips without /dev/kvm.
    #[cfg(target_os = "linux")]
    #[test]
    fn topology_stealth_also_applies_the_pmu_leaf() {
        use enlil_devices::stealth::cpuid::{CpuVendor, CpuidStealthConfig, CpuidStealthTable};

        if !is_kvm_available() {
            eprintln!("skipping topology_stealth_also_applies_the_pmu_leaf: no /dev/kvm");
            return;
        }

        // 16-bit real-mode blob; echoes the topology count then the PMU version:
        //   66 B8 0B 00 00 00   mov eax, 0xB    ; extended topology leaf
        //   66 B9 01 00 00 00   mov ecx, 1      ; subleaf 1 (core level)
        //   0F A2               cpuid
        //   88 D8               mov al, bl      ; al = logical-proc count
        //   BA F8 03            mov dx, 0x3F8   ; COM1
        //   EE                  out dx, al
        //   66 B8 0A 00 00 00   mov eax, 0xA    ; architectural-PMU leaf
        //   0F A2               cpuid           ; al = EAX[7:0] = PMU version
        //   EE                  out dx, al
        //   F4                  hlt
        #[rustfmt::skip]
        let code: [u8; 30] = [
            0x66, 0xB8, 0x0B, 0x00, 0x00, 0x00,
            0x66, 0xB9, 0x01, 0x00, 0x00, 0x00,
            0x0F, 0xA2,
            0x88, 0xD8,
            0xBA, 0xF8, 0x03,
            0xEE,
            0x66, 0xB8, 0x0A, 0x00, 0x00, 0x00,
            0x0F, 0xA2,
            0xEE,
            0xF4,
        ];

        const ENTRY: u64 = 0x1000;
        const SIZE: usize = 0x1000;
        const GUEST_VCPUS: u32 = 2;
        let mut ram = GuestRam::new(SIZE);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu 0");
        backend.create_vcpu(1).expect("create vcpu 1");

        // Intel-presented 2-vCPU table: leaf 0xB carries the topology, leaf 0xA
        // the PMU. A single apply_topology_stealth must install both.
        let mut config = CpuidStealthConfig::from_host(GUEST_VCPUS, 1);
        config.vendor = CpuVendor::Intel;
        let table = CpuidStealthTable::build(&config);
        backend
            .apply_topology_stealth(&table)
            .expect("apply topology + pmu stealth");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        struct EchoOut(Vec<u8>);
        impl VmExitHandler for EchoOut {
            fn io_out(&mut self, _port: u16, data: &[u8]) {
                self.0.extend_from_slice(data);
            }
        }
        let mut echo = EchoOut(Vec::new());

        let mut halted = false;
        for _ in 0..100 {
            if backend.run_vcpu(0, &mut echo).expect("run vcpu") == GuestExit::Halted {
                halted = true;
                break;
            }
        }
        assert!(halted, "guest never reached HLT");
        // One install → both surfaces: 2-vCPU topology and PMU version 5.
        assert_eq!(
            echo.0,
            vec![GUEST_VCPUS as u8, 5],
            "one apply_topology_stealth should yield topology (2) AND PMU version (5)"
        );
    }

    // apply_topology_stealth must also make an AMD guest's leaf 0x8000_0008
    // ECX[7:0] (NC = cores-1) report the guest's core count, not the host's.
    // KVM mirrors the host there, so a guest with fewer vCPUs would otherwise
    // read the host's NC — and contradict the topology installed in leaf 1 /
    // leaf 0xB (the kernel cross-checks the two). After applying a table built
    // for this (AMD) host's vendor, the guest reads exactly the table's NC. The
    // expected value is taken from the table so the test is vendor-correct; on
    // this AMD host it is 1 (2 vCPUs), which differs from the host's real core
    // count, making it non-vacuous. Self-skips without /dev/kvm.
    #[cfg(target_os = "linux")]
    #[test]
    fn topology_stealth_fixes_the_amd_core_count_leaf() {
        use enlil_devices::stealth::cpuid::{CpuidStealthConfig, CpuidStealthTable};

        if !is_kvm_available() {
            eprintln!("skipping topology_stealth_fixes_the_amd_core_count_leaf: no /dev/kvm");
            return;
        }

        // 16-bit real-mode blob; reads leaf 0x8000_0008 and echoes ECX low byte:
        //   66 B8 08 00 00 80   mov eax, 0x80000008  ; extended address/topology
        //   0F A2               cpuid
        //   88 C8               mov al, cl           ; al = ECX[7:0] = NC
        //   BA F8 03            mov dx, 0x3F8        ; COM1
        //   EE                  out dx, al
        //   F4                  hlt
        #[rustfmt::skip]
        let code: [u8; 15] = [
            0x66, 0xB8, 0x08, 0x00, 0x00, 0x80,
            0x0F, 0xA2,
            0x88, 0xC8,
            0xBA, 0xF8, 0x03,
            0xEE,
            0xF4,
        ];

        const ENTRY: u64 = 0x1000;
        const SIZE: usize = 0x1000;
        const GUEST_VCPUS: u32 = 2;
        let mut ram = GuestRam::new(SIZE);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu 0");
        backend.create_vcpu(1).expect("create vcpu 1");

        // Build for the host's own vendor (AMD here) so the 0x8000_0008 ECX
        // override is the one the guest would actually receive in production.
        let table = CpuidStealthTable::build(&CpuidStealthConfig::from_host(GUEST_VCPUS, 1));
        let want_nc = (table.lookup(0x8000_0008, 0).ecx & 0xFF) as u8;
        backend
            .apply_topology_stealth(&table)
            .expect("apply topology stealth");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        struct EchoOut(Vec<u8>);
        impl VmExitHandler for EchoOut {
            fn io_out(&mut self, _port: u16, data: &[u8]) {
                self.0.extend_from_slice(data);
            }
        }
        let mut echo = EchoOut(Vec::new());

        let mut halted = false;
        for _ in 0..100 {
            if backend.run_vcpu(0, &mut echo).expect("run vcpu") == GuestExit::Halted {
                halted = true;
                break;
            }
        }
        assert!(halted, "guest never reached HLT");
        // The guest reads its own NC from leaf 0x8000_0008, consistent with the
        // leaf-1 / leaf-0xB topology — not the host's core count.
        assert_eq!(
            echo.0,
            vec![want_nc],
            "leaf 0x8000_0008 ECX[7:0] should be the guest's NC, not the host's"
        );
    }

    // apply_topology_stealth must also fix the AMD per-cache sharing count in
    // leaf 0x8000_001D: a guest with fewer vCPUs should read its package-wide L3
    // as shared by its own vCPUs, not the host's logical-processor count. The
    // L3 subleaf's EAX[25:14] (NumSharingCache-1) must reflect the guest. With a
    // 2-vCPU AMD table the guest computes 2 sharers for L3, where the host
    // reports many more. Expected derived from the table; self-skips without
    // /dev/kvm. (Fails loudly if KVM here does not enumerate 0x8000_001D, which
    // would mean the no-op path was taken — a real signal, not a silent pass.)
    #[cfg(target_os = "linux")]
    #[test]
    fn topology_stealth_fixes_the_amd_l3_sharing_leaf() {
        use enlil_devices::stealth::cpuid::{CpuidStealthConfig, CpuidStealthTable};

        if !is_kvm_available() {
            eprintln!("skipping topology_stealth_fixes_the_amd_l3_sharing_leaf: no /dev/kvm");
            return;
        }

        // 16-bit real-mode blob; reads leaf 0x8000_001D subleaf 3 (L3) and
        // echoes the sharing count = EAX[25:14] + 1:
        //   66 B8 1D 00 00 80   mov eax, 0x8000001D
        //   66 B9 03 00 00 00   mov ecx, 3           ; subleaf 3 = L3
        //   0F A2               cpuid
        //   66 C1 E8 0E         shr eax, 14          ; drop the type/level bits
        //   66 25 FF 0F 00 00   and eax, 0xFFF       ; isolate NumSharingCache-1
        //   FE C0               inc al               ; -> sharing count
        //   BA F8 03            mov dx, 0x3F8        ; COM1
        //   EE                  out dx, al
        //   F4                  hlt
        #[rustfmt::skip]
        let code: [u8; 31] = [
            0x66, 0xB8, 0x1D, 0x00, 0x00, 0x80,
            0x66, 0xB9, 0x03, 0x00, 0x00, 0x00,
            0x0F, 0xA2,
            0x66, 0xC1, 0xE8, 0x0E,
            0x66, 0x25, 0xFF, 0x0F, 0x00, 0x00,
            0xFE, 0xC0,
            0xBA, 0xF8, 0x03,
            0xEE,
            0xF4,
        ];

        const ENTRY: u64 = 0x1000;
        const SIZE: usize = 0x1000;
        const GUEST_VCPUS: u32 = 2;
        let mut ram = GuestRam::new(SIZE);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu 0");
        backend.create_vcpu(1).expect("create vcpu 1");

        let table = CpuidStealthTable::build(&CpuidStealthConfig::from_host(GUEST_VCPUS, 1));
        let l3 = table.lookup(0x8000_001D, 3);
        assert_eq!(
            (l3.eax >> 5) & 0x7,
            3,
            "table subleaf 3 should be the L3 cache"
        );
        let want_shared = (((l3.eax >> 14) & 0xFFF) + 1) as u8;
        assert_eq!(
            want_shared, GUEST_VCPUS as u8,
            "L3 should be shared by exactly the guest's vCPUs"
        );
        backend
            .apply_topology_stealth(&table)
            .expect("apply topology stealth");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        struct EchoOut(Vec<u8>);
        impl VmExitHandler for EchoOut {
            fn io_out(&mut self, _port: u16, data: &[u8]) {
                self.0.extend_from_slice(data);
            }
        }
        let mut echo = EchoOut(Vec::new());

        let mut halted = false;
        for _ in 0..100 {
            if backend.run_vcpu(0, &mut echo).expect("run vcpu") == GuestExit::Halted {
                halted = true;
                break;
            }
        }
        assert!(halted, "guest never reached HLT");
        // The guest sees its L3 shared by its own 2 vCPUs, not the host's count.
        assert_eq!(
            echo.0,
            vec![want_shared],
            "leaf 0x8000_001D L3 EAX[25:14]+1 should be the guest's sharing count"
        );
    }

    // apply_topology_stealth must also make an AMD SMT guest's leaf 0x8000_001E
    // EBX[15:8] (ThreadsPerComputeUnit-1) report its SMT width. KVM defaults this
    // field to 0 (no SMT) regardless of how many vCPUs exist — measured on this
    // host — so a guest presented as 2 threads per core (the table sets
    // EBX[15:8] = 1) would otherwise read 0 and contradict its own leaf-0xB SMT
    // level and leaf-1 HTT bit. After the override the guest reads 1. The
    // expected value comes from the table; non-vacuous because KVM's native
    // value here is 0. Self-skips without /dev/kvm.
    #[cfg(target_os = "linux")]
    #[test]
    fn topology_stealth_fixes_the_amd_smt_width_leaf() {
        use enlil_devices::stealth::cpuid::{CpuidStealthConfig, CpuidStealthTable};

        if !is_kvm_available() {
            eprintln!("skipping topology_stealth_fixes_the_amd_smt_width_leaf: no /dev/kvm");
            return;
        }

        // 16-bit real-mode blob; reads leaf 0x8000_001E and echoes EBX[15:8]:
        //   66 B8 1E 00 00 80   mov eax, 0x8000001E
        //   0F A2               cpuid
        //   66 C1 EB 08         shr ebx, 8        ; bl = EBX[15:8]
        //   88 D8               mov al, bl
        //   BA F8 03            mov dx, 0x3F8    ; COM1
        //   EE                  out dx, al
        //   F4                  hlt
        #[rustfmt::skip]
        let code: [u8; 19] = [
            0x66, 0xB8, 0x1E, 0x00, 0x00, 0x80,
            0x0F, 0xA2,
            0x66, 0xC1, 0xEB, 0x08,
            0x88, 0xD8,
            0xBA, 0xF8, 0x03,
            0xEE,
            0xF4,
        ];

        const ENTRY: u64 = 0x1000;
        const SIZE: usize = 0x1000;
        // 2 vCPUs, 2 threads/core => a single SMT-2 core. The table's
        // 0x8000_001E SMT field is then 1 (threads-1), which KVM never reports.
        const GUEST_VCPUS: u32 = 2;
        const THREADS_PER_CORE: u32 = 2;
        let mut ram = GuestRam::new(SIZE);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu 0");
        backend.create_vcpu(1).expect("create vcpu 1");

        let table = CpuidStealthTable::build(&CpuidStealthConfig::from_host(
            GUEST_VCPUS,
            THREADS_PER_CORE,
        ));
        let want_smt = ((table.lookup(0x8000_001E, 0).ebx >> 8) & 0xFF) as u8;
        assert_eq!(
            want_smt, 1,
            "an SMT-2 core encodes ThreadsPerComputeUnit-1 = 1"
        );
        backend
            .apply_topology_stealth(&table)
            .expect("apply topology stealth");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        struct EchoOut(Vec<u8>);
        impl VmExitHandler for EchoOut {
            fn io_out(&mut self, _port: u16, data: &[u8]) {
                self.0.extend_from_slice(data);
            }
        }
        let mut echo = EchoOut(Vec::new());

        let mut halted = false;
        for _ in 0..100 {
            if backend.run_vcpu(0, &mut echo).expect("run vcpu") == GuestExit::Halted {
                halted = true;
                break;
            }
        }
        assert!(halted, "guest never reached HLT");
        // The guest reads its SMT width (1), not KVM's native 0.
        assert_eq!(
            echo.0,
            vec![want_smt],
            "leaf 0x8000_001E EBX[15:8] should be the guest's ThreadsPerComputeUnit-1"
        );
    }

    // Proves the run loop drives the timing shadows on real KVM: after running
    // a guest that takes several exits via run_vcpu_timed, the APERF/MPERF
    // shadows have advanced (guest executed cycles) and their ratio is the
    // model's core/ref ratio — i.e. the shadows count guest time at the spoofed
    // frequency and the exit overhead was hidden proportionally (no 1.0-ratio
    // tell). Self-skips without /dev/kvm rather than faking it.
    #[cfg(target_os = "linux")]
    #[test]
    fn run_vcpu_timed_advances_shadows_at_the_model_ratio() {
        use crate::timing_stealth::VcpuTimingState;
        use enlil_devices::stealth::pmc::PmcRateModel;

        if !is_kvm_available() {
            eprintln!("skipping run_vcpu_timed_advances_shadows_...: no /dev/kvm");
            return;
        }

        // 16-bit real-mode blob: three COM1 writes (three IO exits) then HLT,
        // so the guest executes real cycles across several entries.
        //   B0 41            mov al, 'A'
        //   BA F8 03         mov dx, 0x3F8
        //   EE               out dx, al
        //   EE               out dx, al
        //   EE               out dx, al
        //   F4               hlt
        #[rustfmt::skip]
        let code: [u8; 9] = [0xB0, 0x41, 0xBA, 0xF8, 0x03, 0xEE, 0xEE, 0xEE, 0xF4];

        const ENTRY: u64 = 0x1000;
        const SIZE: usize = 0x1000;
        let mut ram = GuestRam::new(SIZE);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        let timing = VcpuTimingState::new();
        let model = PmcRateModel::DEFAULT;
        // Seed so the ratio is the model's from the first read (as the platform
        // install does), not the 1.0 identity.
        timing.advance(1_000_000, &model);
        // A PMC the run loop drives with the returned delta, in lockstep.
        let mut pmc = enlil_devices::stealth::pmc::PmcState::new();
        pmc.fixed_ctr_ctrl = 0x330; // core + ref fixed counters
        pmc.rate_model = model;

        let mut sink = RecordingHandler::default();
        let mut halted = false;
        let mut total_guest_cycles = 0u64;
        for _ in 0..100 {
            let (exit, guest_cycles) = backend
                .run_vcpu_timed(0, &mut sink, &timing, &model)
                .expect("run vcpu timed");
            // The run loop feeds the same delta to the PMC surface.
            pmc.advance_counters(guest_cycles);
            total_guest_cycles += guest_cycles;
            if exit == GuestExit::Halted {
                halted = true;
                break;
            }
        }
        assert!(halted, "guest never reached HLT");
        assert_eq!(sink.io_out.len(), 3, "guest issued its three OUTs");
        assert!(total_guest_cycles > 0, "run loop measured guest cycles");

        let aperf = timing.read_aperf();
        let mperf = timing.read_mperf();
        assert!(
            aperf > 0 && mperf > 0,
            "shadows advanced for guest execution"
        );
        // The PMC core counter, driven by the same per-entry deltas, tracks the
        // APERF shadow — the two surfaces stay in lockstep through the run loop.
        let pmc_core = pmc.read_pmc(0x4000_0001);
        let pmc_ref = pmc.read_pmc(0x4000_0002);
        assert!(pmc_core > 0 && pmc_ref > 0, "PMC advanced with the guest");
        assert!(
            (pmc_core as f64 / pmc_ref as f64 - model.core_per_kilo_ref as f64 / 1000.0).abs()
                < 0.02,
            "RDPMC core/ref tracks the model ratio like APERF/MPERF"
        );
        // The APERF/MPERF ratio is the model core/ref ratio (1.15), preserved
        // across the run and the hidden exits — not the 1.0 VM tell. Tolerance
        // covers per-step integer/float rounding in advance/on_vmresume.
        let ratio = aperf as f64 / mperf as f64;
        let model_ratio = model.core_per_kilo_ref as f64 / 1000.0;
        assert!(
            (ratio - model_ratio).abs() < 0.02,
            "APERF/MPERF ratio {ratio} should track the model {model_ratio}"
        );
    }

    // Proves the MSR *filter* forwards a KVM-*known* MSR to userspace on real
    // KVM: APERF (0xE8) is normally emulated in-kernel, but after
    // forward_msrs_to_userspace covers it, a guest rdmsr of APERF traps to our
    // handler (which serves the stealth shadow) and the shadow value round-trips
    // into the guest. Without the filter the unknown-reason cap alone would not
    // forward APERF. Self-skips without /dev/kvm rather than faking it.
    #[cfg(target_os = "linux")]
    #[test]
    fn msr_filter_forwards_a_kvm_known_msr() {
        if !is_kvm_available() {
            eprintln!("skipping msr_filter_forwards_a_kvm_known_msr: no /dev/kvm");
            return;
        }

        struct MsrProbe {
            supplied: u64,
            seen: Option<u32>,
            echoed: Vec<u8>,
        }
        impl VmExitHandler for MsrProbe {
            fn rdmsr(&mut self, msr: u32) -> Option<u64> {
                self.seen = Some(msr);
                Some(self.supplied)
            }
            fn io_out(&mut self, _port: u16, data: &[u8]) {
                self.echoed.extend_from_slice(data);
            }
        }

        // 16-bit real-mode blob: rdmsr(IA32_APERF=0xE8), echo AL, hlt.
        //   66 B9 E8 00 00 00   mov ecx, 0xE8
        //   0F 32               rdmsr
        //   BA F8 03            mov dx, 0x3F8
        //   EE                  out dx, al
        //   F4                  hlt
        #[rustfmt::skip]
        let code: [u8; 13] = [
            0x66, 0xB9, 0xE8, 0x00, 0x00, 0x00,
            0x0F, 0x32,
            0xBA, 0xF8, 0x03,
            0xEE,
            0xF4,
        ];

        const ENTRY: u64 = 0x1000;
        const SIZE: usize = 0x1000;
        const IA32_APERF: u32 = 0xE8;
        let mut ram = GuestRam::new(SIZE);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();

        let mut backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        if let Err(e) = backend.enable_userspace_msr_exits() {
            eprintln!("skipping msr_filter_forwards_a_kvm_known_msr: {e}");
            return;
        }
        // Deny (forward) MPERF + APERF (0xE7, 0xE8) — both KVM-known.
        backend
            .forward_msrs_to_userspace(&[(0xE7, 2)])
            .expect("install msr filter");
        // SAFETY: `ram` outlives `backend` within this test scope.
        unsafe { backend.map_memory(ENTRY, host_addr, ram.len() as u64) }
            .expect("map guest memory");
        backend.create_vcpu(0).expect("create vcpu");
        backend
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        let mut probe = MsrProbe {
            supplied: 0x0000_0000_0000_00CD,
            seen: None,
            echoed: Vec::new(),
        };
        let mut halted = false;
        let mut saw_aperf = false;
        for _ in 0..100 {
            match backend.run_vcpu(0, &mut probe).expect("run vcpu") {
                GuestExit::Halted => {
                    halted = true;
                    break;
                }
                GuestExit::MsrRead { msr } if msr == IA32_APERF => saw_aperf = true,
                _ => {}
            }
        }
        assert!(halted, "guest never reached HLT");
        assert!(saw_aperf, "APERF rdmsr was not forwarded by the filter");
        assert_eq!(probe.seen, Some(IA32_APERF));
        // The supplied shadow value's low byte reached the guest's AL.
        assert_eq!(probe.echoed, vec![0xCD]);
    }

    /// A do-nothing [`VmExitHandler`] for guests that only touch RAM.
    struct NoopHandler;
    impl VmExitHandler for NoopHandler {}
}
