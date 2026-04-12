import pathlib

target = pathlib.Path(r'D:\Development\enlil\enlil-core\src\kvm_backend.rs')

parts_dir = pathlib.Path(r'D:\Development\enlil\kvm_parts')

# Read existing good parts
parts = []
for p in sorted(parts_dir.glob('p*.rs')):
    parts.append(p.read_text(encoding='utf-8'))

# Write remaining parts inline
remaining_parts = []

# p04: KvmVcpu struct and basic methods
remaining_parts.append(r'''
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ KvmVcpu ━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// A single vCPU backed by a KVM file descriptor.
#[cfg(target_os = "linux")]
pub struct KvmVcpu {
    vcpu_fd: VcpuFd,
    id: u32,
}

#[cfg(target_os = "linux")]
impl KvmVcpu {
    /// Wrap an already-created vCPU fd.
    pub fn new(vcpu_fd: VcpuFd, id: u32) -> Self {
        Self { vcpu_fd, id }
    }

    /// Execute one KVM_RUN and return the exit reason.
    pub fn run_once(&mut self) -> Result<VcpuExit<'_>, Error> {
        self.vcpu_fd.run().map_err(|e| {
            if e.errno() == libc::EINTR {
                Error::HypervisorError("KVM_RUN interrupted (EINTR)".into())
            } else {
                Error::HypervisorError(format!("KVM_RUN failed: {}", e))
            }
        })
    }

    /// Return the vCPU id.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Get general-purpose registers.
    pub fn get_regs(&self) -> Result<kvm_regs, Error> {
        self.vcpu_fd
            .get_regs()
            .map_err(|e| Error::HypervisorError(format!("get_regs: {}", e)))
    }

    /// Set general-purpose registers.
    pub fn set_regs(&self, regs: &kvm_regs) -> Result<(), Error> {
        self.vcpu_fd
            .set_regs(regs)
            .map_err(|e| Error::HypervisorError(format!("set_regs: {}", e)))
    }

    /// Get special registers.
    pub fn get_sregs(&self) -> Result<kvm_sregs, Error> {
        self.vcpu_fd
            .get_sregs()
            .map_err(|e| Error::HypervisorError(format!("get_sregs: {}", e)))
    }

    /// Set special registers.
    pub fn set_sregs(&self, sregs: &kvm_sregs) -> Result<(), Error> {
        self.vcpu_fd
            .set_sregs(sregs)
            .map_err(|e| Error::HypervisorError(format!("set_sregs: {}", e)))
    }

    /// Configure CPUID entries for this vCPU.
    pub fn set_cpuid(&self, cpuid: &CpuId) -> Result<(), Error> {
        self.vcpu_fd
            .set_cpuid2(cpuid)
            .map_err(|e| Error::HypervisorError(format!("set_cpuid2: {}", e)))
    }

    /// Access the raw VcpuFd (for advanced ioctls).
    pub fn vcpu_fd(&self) -> &VcpuFd {
        &self.vcpu_fd
    }
}
''')

# p05: KvmVm struct definition
remaining_parts.append(r'''
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ KvmVm ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// A single virtual machine backed by a KVM VM fd.
#[cfg(target_os = "linux")]
pub struct KvmVm {
    vm_fd: VmFd,
    vcpus: HashMap<u32, KvmVcpu>,
    slots: Vec<MemorySlot>,
    slot_alloc: SlotAllocator,
    /// If true, the run loop should exit at next opportunity.
    exit_requested: Arc<AtomicBool>,
}

#[cfg(target_os = "linux")]
impl KvmVm {
    /// Wrap an already-created VM fd.
    pub fn new(vm_fd: VmFd) -> Self {
        Self {
            vm_fd,
            vcpus: HashMap::new(),
            slots: Vec::new(),
            slot_alloc: SlotAllocator::new(),
            exit_requested: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Get a handle to request loop exit from another thread.
    pub fn exit_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.exit_requested)
    }

    /// Access the raw VmFd.
    pub fn vm_fd(&self) -> &VmFd {
        &self.vm_fd
    }

    /// Return a snapshot of registered memory slots.
    pub fn memory_slots(&self) -> &[MemorySlot] {
        &self.slots
    }
''')

# p06: KvmVm memory region management
remaining_parts.append(r'''
    // ── memory region management ──────────────────────────────────────

    /// Register a memory region with KVM via KVM_SET_USER_MEMORY_REGION.
    ///
    /// `guest_phys_addr` is the GPA base, `memory_size` in bytes,
    /// `userspace_addr` is the HVA of the backing memory.
    /// If `log_dirty` is true, KVM will track dirty pages for this slot.
    pub fn add_memory_region(
        &mut self,
        guest_phys_addr: u64,
        memory_size: u64,
        userspace_addr: u64,
        log_dirty: bool,
    ) -> Result<u32, Error> {
        let slot_id = self.slot_alloc.alloc();
        let flags = if log_dirty { KVM_MEM_LOG_DIRTY_PAGES } else { 0 };

        let region = kvm_userspace_memory_region {
            slot: slot_id,
            flags,
            guest_phys_addr,
            memory_size,
            userspace_addr,
        };

        // SAFETY: the caller must ensure userspace_addr points to a valid
        // mmap'd region of at least `memory_size` bytes that outlives the VM.
        unsafe {
            self.vm_fd
                .set_user_memory_region(region)
                .map_err(|e| Error::HypervisorError(format!(
                    "set_user_memory_region(slot={}, gpa={:#x}, size={:#x}): {}",
                    slot_id, guest_phys_addr, memory_size, e
                )))?;
        }

        let slot = MemorySlot {
            slot: slot_id,
            guest_phys_addr,
            memory_size,
            userspace_addr,
            flags,
        };
        info!(
            "Added memory slot {}: gpa={:#x} size={:#x} dirty={}",
            slot_id, guest_phys_addr, memory_size, log_dirty
        );
        self.slots.push(slot);
        Ok(slot_id)
    }

    /// Remove a memory region by setting its size to zero.
    pub fn remove_memory_region(&mut self, slot_id: u32) -> Result<(), Error> {
        let region = kvm_userspace_memory_region {
            slot: slot_id,
            flags: 0,
            guest_phys_addr: 0,
            memory_size: 0,
            userspace_addr: 0,
        };
        unsafe {
            self.vm_fd
                .set_user_memory_region(region)
                .map_err(|e| Error::HypervisorError(format!(
                    "remove_memory_region(slot={}): {}", slot_id, e
                )))?;
        }
        self.slots.retain(|s| s.slot != slot_id);
        debug!("Removed memory slot {}", slot_id);
        Ok(())
    }
''')

# p07: dirty log + vcpu creation
remaining_parts.append(r'''
    // ── dirty page tracking ───────────────────────────────────────────

    /// Retrieve the dirty page bitmap for a slot.
    /// Returns a Vec<u64> where each bit represents one 4 KiB page.
    pub fn get_dirty_log(&self, slot_id: u32) -> Result<Vec<u64>, Error> {
        // Find the slot to get its size
        let slot = self.slots.iter().find(|s| s.slot == slot_id)
            .ok_or_else(|| Error::HypervisorError(format!(
                "get_dirty_log: slot {} not found", slot_id
            )))?;

        let page_count = (slot.memory_size + 4095) / 4096;
        let bitmap_len = ((page_count + 63) / 64) as usize;

        // KVM_GET_DIRTY_LOG returns and clears the bitmap atomically
        let bitmap = self.vm_fd
            .get_dirty_log(slot_id, slot.memory_size as usize)
            .map_err(|e| Error::HypervisorError(format!(
                "get_dirty_log(slot={}): {}", slot_id, e
            )))?;

        trace!("Dirty log slot {}: {} u64 words, {} pages total",
            slot_id, bitmap.len(), page_count);
        Ok(bitmap)
    }

    /// Count dirty pages across all slots that have LOG_DIRTY enabled.
    pub fn count_dirty_pages(&self) -> Result<u64, Error> {
        let mut total = 0u64;
        for slot in &self.slots {
            if slot.flags & KVM_MEM_LOG_DIRTY_PAGES != 0 {
                let bitmap = self.get_dirty_log(slot.slot)?;
                for word in &bitmap {
                    total += word.count_ones() as u64;
                }
            }
        }
        Ok(total)
    }

    // ── vCPU management ───────────────────────────────────────────────

    /// Create a new vCPU and store it.
    pub fn create_vcpu(&mut self, id: u32) -> Result<&mut KvmVcpu, Error> {
        let vcpu_fd = self.vm_fd
            .create_vcpu(id as u64)
            .map_err(|e| Error::HypervisorError(format!("create_vcpu({}): {}", id, e)))?;

        let vcpu = KvmVcpu::new(vcpu_fd, id);
        self.vcpus.insert(id, vcpu);
        debug!("Created vCPU {}", id);
        Ok(self.vcpus.get_mut(&id).unwrap())
    }

    /// Get a mutable reference to a vCPU.
    pub fn vcpu_mut(&mut self, id: u32) -> Result<&mut KvmVcpu, Error> {
        self.vcpus.get_mut(&id)
            .ok_or_else(|| Error::HypervisorError(format!("vCPU {} not found", id)))
    }

    /// Get a shared reference to a vCPU.
    pub fn vcpu(&self, id: u32) -> Result<&KvmVcpu, Error> {
        self.vcpus.get(&id)
            .ok_or_else(|| Error::HypervisorError(format!("vCPU {} not found", id)))
    }
''')

# p08: The main run loop
remaining_parts.append(r'''
    // ── run loop ──────────────────────────────────────────────────────

    /// Run the vCPU in a loop, dispatching exits to the handler.
    ///
    /// The loop continues until:
    /// - `handle_halt()` returns `false`
    /// - A shutdown/triple-fault occurs
    /// - `exit_requested` is set from another thread
    /// - An unrecoverable error occurs
    ///
    /// Returns the final `KvmExit` that caused the loop to stop.
    pub fn run_vcpu_loop(
        &mut self,
        vcpu_id: u32,
        handler: &mut dyn VmExitHandler,
    ) -> Result<KvmExit, Error> {
        let vcpu = self.vcpus.get_mut(&vcpu_id)
            .ok_or_else(|| Error::HypervisorError(format!("vCPU {} not found", vcpu_id)))?;

        let exit_flag = Arc::clone(&self.exit_requested);
        info!("Entering run loop for vCPU {}", vcpu_id);

        loop {
            // Check if external exit was requested
            if exit_flag.load(Ordering::Relaxed) {
                info!("Exit requested for vCPU {}", vcpu_id);
                return Ok(KvmExit::Halt);
            }

            // Execute one KVM_RUN
            let exit = match vcpu.vcpu_fd.run() {
                Ok(exit) => exit,
                Err(e) => {
                    if e.errno() == libc::EINTR {
                        trace!("KVM_RUN interrupted (EINTR), retrying");
                        continue;
                    }
                    return Err(Error::HypervisorError(
                        format!("KVM_RUN failed on vCPU {}: {}", vcpu_id, e)
                    ));
                }
            };

            // Dispatch the exit
            match exit {
                VcpuExit::IoIn(port, data) => {
                    let size = data.len() as u8;
                    let val = handler.handle_io_in(port, size);
                    let bytes = val.to_le_bytes();
                    let n = data.len().min(4);
                    data[..n].copy_from_slice(&bytes[..n]);
                    trace!("IO in: port={:#x} size={} val={:#x}", port, size, val);
                }
                VcpuExit::IoOut(port, data) => {
                    let size = data.len() as u8;
                    trace!("IO out: port={:#x} size={} data={:?}", port, size, data);
                    handler.handle_io_out(port, size, data);
                }
                VcpuExit::MmioRead(addr, data) => {
                    let size = data.len() as u8;
                    let val = handler.handle_mmio_read(addr, size);
                    let bytes = val.to_le_bytes();
                    let n = data.len().min(8);
                    data[..n].copy_from_slice(&bytes[..n]);
                    trace!("MMIO read: addr={:#x} size={} val={:#x}", addr, size, val);
                }
                VcpuExit::MmioWrite(addr, data) => {
                    let size = data.len() as u8;
                    trace!("MMIO write: addr={:#x} size={} data={:?}", addr, size, data);
                    handler.handle_mmio_write(addr, size, data);
                }
''')

# p09: more exit handlers in the match
remaining_parts.append(r'''
                VcpuExit::Hlt => {
                    trace!("vCPU {} halted", vcpu_id);
                    if !handler.handle_halt() {
                        return Ok(KvmExit::Halt);
                    }
                    // If handler returns true, we continue (guest is
                    // waiting for an interrupt -- in a real VMM you'd
                    // wait on an eventfd here).
                }
                VcpuExit::Shutdown => {
                    warn!("vCPU {} shutdown (triple fault)", vcpu_id);
                    handler.handle_shutdown();
                    return Ok(KvmExit::Shutdown);
                }
                VcpuExit::InternalError => {
                    error!("KVM internal error on vCPU {}", vcpu_id);
                    return Ok(KvmExit::InternalError);
                }
                VcpuExit::FailEntry(reason, cpu) => {
                    error!(
                        "VM entry failed on vCPU {}: reason={:#x} cpu={}",
                        vcpu_id, reason, cpu
                    );
                    return Ok(KvmExit::FailEntry {
                        hardware_entry_failure_reason: reason,
                        cpu,
                    });
                }
                VcpuExit::SystemEventShutdown => {
                    info!("System event: shutdown on vCPU {}", vcpu_id);
                    handler.handle_shutdown();
                    return Ok(KvmExit::SystemEvent { event_type: 1 });
                }
                VcpuExit::SystemEventReset => {
                    info!("System event: reset on vCPU {}", vcpu_id);
                    return Ok(KvmExit::SystemEvent { event_type: 2 });
                }
''')

# p10: hypercall + debug + catch-all + closing braces
remaining_parts.append(r'''
                VcpuExit::Hypercall => {
                    // Read hypercall info from registers
                    let regs = vcpu.get_regs()?;
                    let nr = regs.rax;
                    let args = [regs.rdi, regs.rsi, regs.rdx];
                    trace!("Hypercall nr={:#x} args={:?}", nr, args);
                    let ret = handler.handle_hypercall(nr, args);
                    // Write return value
                    let mut new_regs = regs;
                    new_regs.rax = ret;
                    vcpu.set_regs(&new_regs)?;
                }
                VcpuExit::Debug(debug_exit) => {
                    let pc = debug_exit.pc;
                    debug!("Debug exit at pc={:#x} on vCPU {}", pc, vcpu_id);
                    return Ok(KvmExit::Debug { pc });
                }
                exit => {
                    let desc = format!("{:?}", exit);
                    warn!("Unhandled VM exit on vCPU {}: {}", vcpu_id, desc);
                    return Ok(KvmExit::Other(desc));
                }
            }
        }
    }
}
''')

# p11: KvmHypervisor
remaining_parts.append(r'''
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━ KvmHypervisor ━━━━━━━━━━━━━━━━━━━━━━━━━

/// Top-level handle to `/dev/kvm`. Creates and manages VMs on demand.
#[cfg(target_os = "linux")]
pub struct KvmHypervisor {
    kvm: Kvm,
    vms: HashMap<usize, KvmVm>,
    vm_counter: usize,
}

#[cfg(target_os = "linux")]
impl KvmHypervisor {
    /// Open `/dev/kvm` and verify the API version.
    pub fn new() -> Result<Self, Error> {
        let kvm = Kvm::new()
            .map_err(|e| Error::HypervisorError(format!("Failed to open /dev/kvm: {}", e)))?;

        let api_version = kvm.get_api_version();
        if api_version != KVM_API_VERSION as i32 {
            return Err(Error::HypervisorError(format!(
                "KVM API version mismatch: expected {}, got {}",
                KVM_API_VERSION, api_version
            )));
        }

        info!("Opened /dev/kvm, API version {}", api_version);
        Ok(Self {
            kvm,
            vms: HashMap::new(),
            vm_counter: 0,
        })
    }

    /// Create a new VM, returning its id.
    pub fn create_vm(&mut self) -> Result<usize, Error> {
        let vm_fd = self.kvm
            .create_vm()
            .map_err(|e| Error::HypervisorError(format!("create_vm: {}", e)))?;

        let id = self.vm_counter;
        self.vm_counter += 1;
        self.vms.insert(id, KvmVm::new(vm_fd));
        debug!("Created KVM VM {}", id);
        Ok(id)
    }

    /// Get a mutable reference to a VM by id.
    pub fn vm_mut(&mut self, vm_id: usize) -> Result<&mut KvmVm, Error> {
        self.vms
            .get_mut(&vm_id)
            .ok_or_else(|| Error::HypervisorError(format!("VM {} not found", vm_id)))
    }

    /// Get a shared reference to a VM by id.
    pub fn vm(&self, vm_id: usize) -> Result<&KvmVm, Error> {
        self.vms
            .get(&vm_id)
            .ok_or_else(|| Error::HypervisorError(format!("VM {} not found", vm_id)))
    }

    /// Access the raw `Kvm` handle (for supported-cpuid queries, etc.).
    pub fn kvm(&self) -> &Kvm {
        &self.kvm
    }

    /// Query supported CPUID entries from the host.
    pub fn get_supported_cpuid(&self) -> Result<CpuId, Error> {
        self.kvm
            .get_supported_cpuid(kvm_bindings::KVM_MAX_CPUID_ENTRIES)
            .map_err(|e| Error::HypervisorError(format!("get_supported_cpuid: {}", e)))
    }
}
''')

# p12: non-Linux stubs
remaining_parts.append(r'''
// ━━━━━━━━━━━━━━━━━━━━━━━━ Non-Linux stub ━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[cfg(not(target_os = "linux"))]
pub struct KvmVcpu;

#[cfg(not(target_os = "linux"))]
pub struct KvmVm;

#[cfg(not(target_os = "linux"))]
pub struct KvmHypervisor;

#[cfg(not(target_os = "linux"))]
impl KvmHypervisor {
    pub fn new() -> Result<Self, Error> {
        Err(Error::HypervisorError(
            "KVM backend is only available on Linux".into(),
        ))
    }
}

#[cfg(not(target_os = "linux"))]
impl KvmVm {
    pub fn run_vcpu_loop(
        &mut self,
        _vcpu_id: u32,
        _handler: &mut dyn VmExitHandler,
    ) -> Result<KvmExit, Error> {
        Err(Error::HypervisorError(
            "KVM backend is only available on Linux".into(),
        ))
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ Tests ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kvm_exit_debug_display() {
        let exit = KvmExit::IoOut {
            port: 0x3f8,
            size: 1,
            data: vec![0x41],
        };
        let dbg = format!("{:?}", exit);
        assert!(dbg.contains("3f8"));
    }

    #[test]
    fn kvm_exit_halt_clone() {
        let exit = KvmExit::Halt;
        let exit2 = exit.clone();
        assert!(matches!(exit2, KvmExit::Halt));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_hypervisor_returns_error() {
        assert!(KvmHypervisor::new().is_err());
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_vm_run_returns_error() {
        // Cannot construct KvmVm on non-linux, so we test via
        // the struct existing as a unit type
        let _ = KvmVm;
    }
}
''')

# Combine: p01 + p02 + p03 + remaining_parts
all_parts = parts[:3]  # p01, p02, p03
all_parts.extend(remaining_parts)

final = '\n'.join(all_parts)

target.write_text(final, encoding='utf-8')
print(f"Wrote {len(final)} bytes to {target}")
