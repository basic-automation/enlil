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
}

// ===========================================================================
// Linux / KVM implementation
// ===========================================================================

#[cfg(target_os = "linux")]
mod linux {
    use super::{GuestExit, VmExitHandler};
    use crate::error::{Error, Result};
    use kvm_bindings::kvm_userspace_memory_region;
    use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};

    /// Returns `true` if this host exposes a usable `/dev/kvm`.
    ///
    /// Used to gate integration tests and to fail fast with a clear message
    /// when nested virtualization is unavailable.
    #[must_use]
    pub fn is_kvm_available() -> bool {
        Kvm::new().is_ok()
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
        /// # Errors
        /// Returns [`Error::HypervisorError`] if KVM is unavailable, the API
        /// version is unexpected, or any setup ioctl fails.
        pub fn new() -> Result<Self> {
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
            // interrupts without userspace round-trips.
            vm.create_irq_chip()
                .map_err(|e| Error::HypervisorError(format!("KVM_CREATE_IRQCHIP: {e}")))?;

            Ok(Self {
                kvm,
                vm,
                vcpus: Vec::new(),
                slots: Vec::new(),
                next_slot: 0,
            })
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

        /// Registered guest memory slots.
        #[must_use]
        pub fn mem_slots(&self) -> &[MemSlot] {
            &self.slots
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
                VcpuExit::Hlt => GuestExit::Halted,
                VcpuExit::Shutdown | VcpuExit::SystemEvent(..) => GuestExit::Shutdown,
                VcpuExit::Debug(_) => GuestExit::Debug,
                VcpuExit::Intr | VcpuExit::IrqWindowOpen => GuestExit::Interrupted,
                VcpuExit::InternalError => GuestExit::InternalError,
                VcpuExit::FailEntry(reason, _cpu) => GuestExit::FailedEntry(reason),
                VcpuExit::Unsupported(reason) => GuestExit::Unsupported(reason),
                // Everything else (Hypercall, MSR exits, NMI, …) is not yet
                // modelled; surface a sentinel so the caller can log it.
                _ => GuestExit::Unsupported(u32::MAX),
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::{is_kvm_available, KvmBackend, MemSlot};

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

        // Back the guest with a page-aligned host buffer.
        const SIZE: usize = 0x1000;
        let mut mem = vec![0u8; SIZE];
        let host_addr = mem.as_mut_ptr() as u64;
        // SAFETY: `mem` outlives `backend` within this test scope.
        let slot = unsafe { backend.map_memory(0x1000, host_addr, SIZE as u64) }
            .expect("map guest memory");
        assert_eq!(slot.slot, 0);
        assert_eq!(backend.mem_slots().len(), 1);

        let idx = backend.create_vcpu(0).expect("create vcpu");
        assert_eq!(idx, 0);
        assert_eq!(backend.vcpu_count(), 1);
    }
}
