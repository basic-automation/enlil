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

    /// A live KVM virtual machine plus its vCPUs.
    pub struct KvmBackend {
        kvm: Kvm,
        vm: VmFd,
        vcpus: Vec<VcpuFd>,
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
            Ok(self.vcpus.len() - 1)
        }

        /// Number of vCPUs created.
        #[must_use]
        pub fn vcpu_count(&self) -> usize {
            self.vcpus.len()
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
        ) -> Result<GuestExit> {
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
            timing.advance(exit_tsc.saturating_sub(entry_tsc), model);
            timing.on_vmexit(exit_tsc);

            Ok(exit)
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
pub use linux::{is_kvm_available, GuestMemory, GuestRam, KvmBackend, MemSlot, HOST_PAGE_SIZE};

#[cfg(test)]
mod tests {
    use super::*;

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

        let mut sink = RecordingHandler::default();
        let mut halted = false;
        for _ in 0..100 {
            if backend
                .run_vcpu_timed(0, &mut sink, &timing, &model)
                .expect("run vcpu timed")
                == GuestExit::Halted
            {
                halted = true;
                break;
            }
        }
        assert!(halted, "guest never reached HLT");
        assert_eq!(sink.io_out.len(), 3, "guest issued its three OUTs");

        let aperf = timing.read_aperf();
        let mperf = timing.read_mperf();
        assert!(aperf > 0 && mperf > 0, "shadows advanced for guest execution");
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
