// ━━━━━━━━━━━━━━━━━━━━━━━━ VmExitHandler trait ━━━━━━━━━━━━━━━━━━━━━━

/// Trait that callers implement to handle VM exits dispatched by the
/// run-loop. Each method corresponds to a KvmExit variant.
pub trait VmExitHandler {
    /// Handle IN instruction. Return the value to inject (up to 4 bytes, LE).
    fn handle_io_in(&mut self, port: u16, size: u8) -> u32;
    /// Handle OUT instruction.
    fn handle_io_out(&mut self, port: u16, size: u8, data: &[u8]);
    /// Handle MMIO read. Return the value to inject (up to 8 bytes, LE).
    fn handle_mmio_read(&mut self, addr: u64, size: u8) -> u64;
    /// Handle MMIO write.
    fn handle_mmio_write(&mut self, addr: u64, size: u8, data: &[u8]);
    /// Handle CPUID exit. Return (eax, ebx, ecx, edx).
    fn handle_cpuid(&mut self, leaf: u32, subleaf: u32) -> (u32, u32, u32, u32);
    /// Handle RDMSR exit. Return the MSR value.
    fn handle_rdmsr(&mut self, msr: u32) -> u64;
    /// Handle WRMSR exit.
    fn handle_wrmsr(&mut self, msr: u32, value: u64);
    /// Handle HLT. Return `true` to continue (wait for interrupt), `false` to stop.
    fn handle_halt(&mut self) -> bool;
    /// Handle shutdown / triple fault.
    fn handle_shutdown(&mut self);
    /// Handle hypercall. Return the value to place in RAX.
    fn handle_hypercall(&mut self, nr: u64, args: [u64; 3]) -> u64;
}
