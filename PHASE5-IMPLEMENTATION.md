# Phase 5 — Windows Guest Support & Transparency Implementation Plan

## Milestone
**Windows 11 installs and runs as a guest, passes pafish + al-khaser + custom IET test**
**Goal:** Fully boot Windows transparently using USB live boot mode

## Critical Path (What Must Happen First)

### Phase 0.2 **MUST** Complete First
We cannot begin Phase 5 without a real hypervisor backend. The roadmap requires KVM or Windows Hyper-V (WHP) integration:
- **KVM path (Linux host):** Use `kvm-ioctls`, `vm-memory`, `linux-loader`
- **WHP path (Windows host):** Use WHP API via `windows-rs`

**Decision:** Start with KVM on Linux (Phase 6 will enable bare-metal via UEFI)

### Prerequisite: Phase 0.2 Implementation Steps
1. Add KVM dependencies to `enlil-core`
2. Implement `HypervisorBackend` trait for KVM:
   - `create_vm()` → KVM_CREATE_VM ioctl
   - `create_vcpu(vm, id)` → KVM_CREATE_VCPU ioctl
   - `run_vcpu(vcpu)` → KVM_RUN ioctl loop
   - `handle_exit(vm, exit_info)` → dispatch on exit_reason
   - `map_guest_memory(vm, gpa, hpa, size)` → set user memory region
   - `inject_interrupt(vcpu, vector)` → KVM_INTERRUPT ioctl
3. Test with minimal Linux guest boot to serial shell
4. **This unlocks Phase 5**

---

## Phase 5 Tasks (In Order)

### 5.1 ACPI Table Synthesis

**Goal:** Generate realistic ACPI tables that convince Windows 11 to boot

#### 5.1.1 Add Dependencies
```toml
[dependencies]
acpi = "0.3"         # ACPI table parsing + generation
aml = "0.17"         # AML bytecode execution
```

#### 5.1.2 Create `enlil-acpi` Crate
- Location: `crates/enlil-acpi/src/`
- Modules:
  - `rsdp.rs`: RSDP (Root System Descriptor Pointer) generation
  - `xsdt.rs`: XSDT (Extended System Descriptor Table)
  - `fadt.rs`: FADT (Fixed ACPI Description Table) — power management, FACS address
  - `madt.rs`: MADT (Multiple APIC Description Table) — CPU topology, LAPIC, IOAPIC
  - `dsdt.rs`: DSDT (Differentiated System Description Table) — AML bytecode for PCI bus, devices
  - `ssdt.rs`: SSDT (Secondary System Description Table) — additional device definitions
  - `mcfg.rs`: MCFG (Memory-mapped Configuration Space Base Address) — PCIe ECAM
  - `hpet.rs`: HPET (High Precision Event Timer)
  - `builder.rs`: `AcpiBuilder` — constructs full table set from guest config

#### 5.1.3 Key Implementation Details

**RSDP (36 bytes, then extended to 36 bytes for XSDT):**
```rust
pub struct Rsdp {
    signature: [u8; 8],      // "RSD PTR "
    checksum: u8,
    oem_id: [u8; 6],         // e.g., "ENLIL "
    revision: u8,            // 2 for XSDT
    rsdt_address: u32,
    length: u32,             // Extended RSDP length
    xsdt_address: u64,       // Points to XSDT
    extended_checksum: u8,
    reserved: [u8; 3],
}
```

**XSDT:** Points to all other tables (FADT, MADT, DSDT, etc.)

**MADT:** Critical for Windows. Must match vCPU count:
- One LAPIC entry per vCPU with unique APIC IDs (0, 1, 2, ...)
- One IOAPIC entry for virtual interrupt routing
- IO APIC Address: standard 0xFEC00000

**DSDT/SSDT:** AML bytecode defining:
- PCI Root Complex (`PCI0`)
- ISA Bus (`LPCB`)
- UART device (COM1)
- Power management (S5 state at minimum)

**MCFG:** PCIe config space base address (typically 0xF0000000)

#### 5.1.4 Validation
- Use `iasl` (Intel ACPI Source Language compiler) to validate generated AML bytecode
- Checksum calculation for all tables
- Test with `acpi_tables` crate's parser

#### 5.1.5 Integration with Guest Config
```toml
[guest.windows11]
name = "Windows 11"
vcpus = 4
memory_mb = 8192
acpi_oem = "ENLIL"        # Custom OEM string
```

---

### 5.2 SMBIOS Synthesis

**Goal:** Generate SMBIOS tables that pass Windows activation checks

#### 5.2.1 Create `enlil-smbios` Crate
- Modules:
  - `smbios_header.rs`: Entry point structure
  - `table_0_bios.rs`: BIOS information (vendor, version, date)
  - `table_1_system.rs`: System information (manufacturer, product name, serial, UUID)
  - `table_2_baseboard.rs`: Baseboard (optional but realistic)
  - `table_3_chassis.rs`: Chassis (case type, manufacturer)
  - `table_4_processor.rs`: Processor information (match physical CPU model string)
  - `table_16_memarray.rs`: Memory array (total capacity)
  - `table_17_memdevice.rs`: Memory device (per-DIMM, match allocated guest RAM)
  - `table_32_bootinfo.rs`: Boot information
  - `builder.rs`: `SmbiosBuilder` — assembles all tables into a single memory block

#### 5.2.2 Key Details

**System Information (Type 1):**
```rust
pub struct SystemInformation {
    manufacturer: String,      // e.g., "Dell Inc.", "Lenovo", "HP"
    product_name: String,      // e.g., "OptiPlex 7090", "ThinkCentre"
    version: String,
    serial_number: String,     // Use guest UUID for consistency
    uuid: Uuid,                // Deterministic UUID from guest name hash
    wake_up_type: u8,          // 3 = AC Power
}
```

**Processor Information (Type 4):**
- **Critical:** Pass through actual CPU model string from physical CPU (via CPUID leaf 0x80000002–0x80000004)
- Socket designation, core count, thread count (match vCPU assignment)
- Status: enabled

**Memory (Type 16 + Type 17):**
- Total capacity = allocated guest RAM
- Type 17 (Memory Device): one entry per simulated 1GB or per actual DIMM count
- Speed: match physical RAM speed if available (else generic DDR4-3200)

#### 5.2.3 Realistic Vendor Choices
- **Manufacturers:** Dell, Lenovo, HP, ASUS, Gigabyte (pick one per guest)
- **BIOS Vendors:** AMI, Phoenix, Award (these are common and recognizable)
- **Product Names:** Follow real naming schemes (OptiPlex, ThinkCentre, ProDesk, etc.)

#### 5.2.4 Storage
- SMBIOS tables live at physical address 0xF0000 (traditional location)
- Entry point at 0xE0000–0xEFFFF
- Total size typically < 8KB

#### 5.2.5 Integration with Windows Guest Boot
- UEFI firmware (OVMF) reads SMBIOS from that address
- Passes to Windows bootloader
- Windows uses for hardware identity, license binding, driver selection

---

### 5.3 CPUID Stealth (Expand Existing)

The code in `enlil-core::cpuid::CpuidFilter` exists. Enhance it:

#### 5.3.1 Existing Implementation Check
- Verify `CpuidFilter::filter()` method exists
- Verify leaves 0x1, 0x4, 0xB, 0x40000xx are handled
- Add any missing leaves

#### 5.3.2 Enhanced Stealth Guarantees
- **Leaf 0x0:** Return correct vendor string matching physical CPU (GenuineIntel or AuthenticAMD)
- **Leaf 0x1:**
  - ECX bit 31 (hypervisor present) = 0 (critical!)
  - Return correct family/model/stepping from physical CPU
  - Return vCPU count (only assigned cores, not total system cores)
- **Leaf 0x80000000–0x80000004:** Pass through real CPU brand string (e.g., "Intel Core i7-12700K")
- **Leaf 0x40000000–0x400000FF:** Return all zeros (no hypervisor signature)
- **Reserved/undefined leaves:** Return all zeros
- **Per-vCPU APIC IDs:** Craft leaf 0xB topology (Extended Topology Enumeration) to report only assigned cores

#### 5.3.3 Precomputation for Fast Responses
**Critical for timing stealth:** Pre-compute all CPUID responses into a lookup table:

```rust
pub struct CpuidCache {
    responses: HashMap<(u32, u32), CpuidResult>,  // (leaf, subleaf) → (eax, ebx, ecx, edx)
}

impl CpuidCache {
    fn build(physical_cpu: &CpuInfo, vcpu_count: usize) -> Self {
        // Pre-compute every CPUID leaf result
        // On VMEXIT, do table lookup instead of computing
    }
}
```

Goal: CPUID exit round-trip **< 500 cycles** (real hardware: 100–200 cycles; without caching: 2000+ cycles)

---

### 5.4 Timing Stealth (Critical)

This is where detection happens. Implement all three sub-features:

#### 5.4.1 TSC Offsetting

**Concept:** VMEXIT has overhead (~1000+ cycles). Hide this by adjusting guest TSC offset.

**Intel VMX Implementation:**
- **VMCS field:** `TSC_OFFSET` (controls what RDTSC returns)
- Measure average VMEXIT cost (sample: 100 exits, record elapsed cycles)
- Adjust `TSC_OFFSET` downward to compensate: `TSC -= measured_overhead`
- Result: Guest time flows continuously despite hypervisor overhead

**Code location:** `enlil-core::vcpu::VcpuContext` → add `tsc_offset: i64` field

#### 5.4.2 APERF/MPERF Emulation (Critical — Defeats IET)

**Why this matters:** Modern anti-cheat uses IA32_APERF (Actual Performance Counter) instead of TSC because APERF counts real CPU cycles, not wall-clock. IET (Indirect Execution Timing) divergence tests:
1. Measure APERF before CPUID
2. Execute CPUID (causes VMEXIT → hypervisor overhead)
3. Measure APERF after
4. Overhead becomes visible as "too many cycles for CPUID"

**Solution: Shadow APERF/MPERF**

```rust
pub struct ApertMsrState {
    shadow_aperf: u64,
    shadow_mperf: u64,
    last_vmexit_cycles: u64,  // Track hypervisor time
}

impl ApertMsrState {
    fn update_for_vmexit(&mut self, exit_overhead_cycles: u64) {
        // Do NOT advance shadow counters during VMEXIT
        // This hides the overhead
        self.last_vmexit_cycles = exit_overhead_cycles;
    }
    
    fn update_for_guest_execution(&mut self, guest_cycles: u64) {
        // Advance shadow counters proportionally during guest time
        self.shadow_aperf += guest_cycles;
        self.shadow_mperf += guest_cycles;  // Keep ratio stable
    }
}
```

**Implementation:**
- Intercept RDMSR for 0xE8 (IA32_APERF) and 0xE7 (IA32_MPERF)
- Return shadow values instead of real values
- Maintain consistency: `APERF / MPERF ≈ 1.0` (CPU not throttled)

**Code location:** `enlil-core::msr::MsrHandler` → add APERF/MPERF interception

#### 5.4.3 LBR (Last Branch Record) Save/Restore

**Why this matters:** Anti-cheat checks the branch history stack after forcing VMEXIT (via CPUID). If the last branch target doesn't match the expected next instruction, a hypervisor is present.

**Solution: Hardware save/restore + sanitization**

**Intel VMX:**
- **VMCS field:** VM-exit controls, bit 22 = LBR save/restore enable
- **VMCS field:** VM-entry controls, bit 22 = LBR restore enable
- Hardware automatically saves/restores LBR MSRs (0x680–0x68F FROM, 0x6C0–0x6CF TO)
- **Sanitization:** After LBR save on VMEXIT, overwrite the most recent entry to remove the "branch to hypervisor" record

**AMD SVM:**
- **VMCB field:** SVM feature bit 1 = LBRV (LBR Virtualization)
- Hardware saves/restores DebugCtlMSR, LastBranchFromIP, LastBranchToIP during VMRUN/VMEXIT
- **Sanitization:** Same approach — overwrite most recent entry

**Code location:** `enlil-core::vcpu::VcpuContext` → add LBR save/restore handlers

---

### 5.5 Virtual TPM 2.0

**Goal:** Windows 11 requires TPM 2.0 for Secure Boot validation, BitLocker, Windows Hello

#### 5.5.1 Options
1. **Use swtpm (Software TPM):** Existing, tested, but external process
2. **Integrate tpm2-tss:** TPM 2.0 reference implementation (complex, ~50K LOC)
3. **Minimal TPM 2.0 emulation:** Implement only what Windows 11 requires (recommend for this phase)

#### 5.5.2 Minimal TPM Implementation
- Create `enlil-tpm2` crate
- Expose at MMIO address: **0xFED40000** (standard TPM 2.0 MMIO location)
- Implement:
  - **Interface:** TPM 2.0 CRB (Command Response Buffer) interface
  - **Commands:** Only those Windows 11 setup/boot uses:
    - TPM2_Startup
    - TPM2_SelfTest
    - TPM2_GetCapability
    - TPM2_ReadPublic (PCR reads)
    - TPM2_Quote (attestation)
    - TPM2_PCR_Read (PCR values)
  - **PCR Banks:** SHA256 (others optional)
  - **Storage:** PCR values stored per-guest (persist to disk for consistency)

#### 5.5.3 CRB Interface
```rust
pub struct TpmCrb {
    ctrl: TpmCrbCtrl,          // Control register
    status: TpmCrbStatus,      // Status register
    command_buffer: Vec<u8>,   // Command payload
    response_buffer: Vec<u8>,  // Response payload
}

impl TpmCrb {
    fn mmio_write(&mut self, offset: u64, data: &[u8]) {
        // Guest writes command
    }
    
    fn mmio_read(&self, offset: u64, size: usize) -> Vec<u8> {
        // Guest reads response
    }
}
```

#### 5.5.4 Integration
- Add to device bus: `enlil-devices::bus::Bus`
- Route 0xFED40000–0xFED40FFF MMIO accesses to TPM handler
- Per-guest TPM state (isolated)

---

### 5.6 Windows Boot Path (OVMF)

#### 5.6.1 Get OVMF Binary
```bash
# On Linux:
sudo apt install ovmf
# Find: /usr/share/OVMF/OVMF_CODE.fd (UEFI firmware code)
#       /usr/share/OVMF/OVMF_VARS.fd (NVRAM for EFI variables)
```

#### 5.6.2 Integration
- Load OVMF_CODE.fd into guest memory at 0xFFFE0000 (4GB - 128KB)
- Load OVMF_VARS.fd as "NV-RAM" (guest UEFI config)
- Pass ACPI tables (from 5.1), SMBIOS tables (from 5.2) through UEFI handoff
- Boot Windows 11 ISO from VirtIO-blk disk

#### 5.6.3 Device Requirements for OVMF → Windows 11 Boot
- Serial UART (COM1) for debug output
- Storage: VirtIO-blk or IDE (boot disk with Windows 11 ISO)
- Network: VirtIO-net (optional, but OVMF may try to PXE boot)
- Framebuffer: GOP (Graphics Output Protocol) — for Windows setup UI

#### 5.6.4 USB Boot Path (Phase 5 Goal)
- Create USB drive with:
  - OVMF_CODE.fd + OVMF_VARS.fd
  - Enlil configuration (JSON/TOML)
  - Windows 11 ISO (or a pre-installed image)
- Boot Enlil from USB
- Enlil loads everything and launches Windows guest

---

### 5.7 Windows-Specific Virtual Devices

#### 5.7.1 PS/2 Keyboard/Mouse (Fallback)
- Already in `enlil-devices`?
- Ensure early in boot (Windows looks for these)

#### 5.7.2 PCI Express Root Complex
- Ensure PCIe enumeration works
- Report all virtual devices as PCI endpoints
- VirtIO devices appear with correct PCI IDs

#### 5.7.3 ACPI Power Management
- S5 (shutdown) state — required
- S3 (sleep) / S4 (hibernation) — optional for Phase 5

#### 5.7.4 Virtual GPU (Minimal)
- **Option A:** Headless (serial console only) — simplest for Phase 5
- **Option B:** Software framebuffer (QXL/VGA) — allows Windows GUI setup
- **Recommendation:** Start with Option A (headless), add GUI later

#### 5.7.5 Check Existing Device Coverage
- Serial UART: ✅ (Phase 2)
- VirtIO-blk: ✅ (Phase 3)
- VirtIO-net: ✅ (Phase 3)
- Virtual switch: ✅ (Phase 3)
- Interrupt controller (LAPIC/IOAPIC): ✅ (Phase 3)
- Timers (PIT/HPET): ✅ (Phase 3)

---

### 5.8 Anti-Detection Testing

#### 5.8.1 Automated Test Suite (in Guest)
- Create Windows executable (`windows-test.exe`) that runs inside guest
- Tests:
  - CPUID checks (leaf 0x1 bit 31, leaf 0x40000000)
  - RDTSC timing (measure delta around CPUID — should be ~50–100 cycles, not 2000+)
  - IET divergence (custom implementation using IA32_APERF)
  - LBR stack post-CPUID (verify last branch target is correct)
  - SMBIOS validation (reasonable manufacturer, product, serial)
  - ACPI table validation (realistic OEM strings, table count)
  - Registry checks (no Hyper-V/VMware entries in `HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Services`)
  - Device checks (no virtio drivers visible in Device Manager, no VMware tools)

#### 5.8.2 Run Standard Tools
- **pafish**: Paranoid Fish anti-VM detection
  - Get: https://github.com/a0rtega/pafish
  - Compile for Windows, run in guest
  - **Goal:** All checks pass (no hypervisor detected)
  
- **al-khaser**: Advanced anti-VM tool
  - Get: https://github.com/LordNoteworthy/al-khaser
  - **Goal:** All checks pass

- **Custom IET Test:**
  ```asm
  ; Measure CPUID execution time via APERF
  RDMSR IA32_APERF (0xE8)  → rax1
  CPUID (leaf 0x1)
  RDMSR IA32_APERF (0xE8)  → rax2
  
  cycles = rax2 - rax1
  if cycles > threshold {
    hypervisor detected (VMEXIT overhead)
  }
  ```
  - **Threshold:** Target <100 cycles for CPUID (with APERF shadowing, hypervisor overhead is hidden)

#### 5.8.3 Windows 11 Setup Validation
- Boot Windows 11 installer
- Proceed through setup (no hypervisor detection errors)
- Reach desktop
- Run pafish + al-khaser + custom IET test — all pass
- **Milestone achieved**

---

## Implementation Sequence

### Week 1–2: Phase 0.2 (KVM Backend)
- [ ] Add KVM dependencies
- [ ] Implement `HypervisorBackend` for KVM
- [ ] Boot minimal Linux guest to serial shell
- [ ] **Validation:** Simple Linux guest runs

### Week 3: ACPI + SMBIOS Synthesis (5.1 + 5.2)
- [ ] Create `enlil-acpi` crate (RSDP, XSDT, FADT, MADT, DSDT, MCFG)
- [ ] Create `enlil-smbios` crate (all table types)
- [ ] Build from guest config (vcpu_count, memory, etc.)
- [ ] Validate with `iasl`
- [ ] **Validation:** Realistic ACPI/SMBIOS dumps

### Week 4: CPUID + Timing Stealth (5.3 + 5.4)
- [ ] Expand `CpuidFilter` for all leaves
- [ ] Implement CPUID cache (precomputation)
- [ ] Implement TSC offsetting
- [ ] Implement APERF/MPERF shadowing
- [ ] Implement LBR save/restore + sanitization
- [ ] **Validation:** CPUID < 500 cycles, APERF consistent

### Week 5: Virtual TPM 2.0 (5.5)
- [ ] Create `enlil-tpm2` crate
- [ ] Implement CRB interface
- [ ] Route MMIO to TPM
- [ ] PCR storage
- [ ] **Validation:** TPM commands respond correctly

### Week 6: OVMF + Boot (5.6 + 5.7)
- [ ] Download OVMF
- [ ] Load into guest memory
- [ ] Pass ACPI/SMBIOS through UEFI handoff
- [ ] Ensure required devices (serial, storage, network)
- [ ] **Validation:** OVMF → UEFI shell boots

### Week 7: Windows 11 Boot
- [ ] Create bootable USB with OVMF + Enlil + Windows ISO
- [ ] Boot from USB on real hardware (or QEMU)
- [ ] Launch Windows 11 guest
- [ ] Reach desktop
- [ ] **Validation:** Windows setup completes

### Week 8: Anti-Detection Testing (5.8)
- [ ] Write custom IET divergence test
- [ ] Run pafish (should pass all checks)
- [ ] Run al-khaser (should pass all checks)
- [ ] Run custom IET test (should pass)
- [ ] **Milestone:** Windows 11 + pafish + al-khaser + IET test = **PASS**

---

## Risk Mitigation

| Risk | Mitigation |
|------|-----------|
| KVM backend takes longer than expected | Fallback: use existing QEMU as reference, leverage RustVMM crates |
| ACPI/SMBIOS synthesis too complex | Use existing crate (e.g., `acpi`), pre-generate tables from real hardware |
| CPUID cache causes timing variance | Pre-compute all leaves at guest startup, ensure lookup is inline |
| APERF/MPERF emulation misses edge cases | Test against reference implementations (Cloud Hypervisor, QEMU) |
| LBR sanitization causes crashes | Disable LBR if detection is unstable, try CPUID caching alone first |
| Windows doesn't boot with synthetic ACPI/SMBIOS | Use OVMF's built-in fallback (simple ACPI tables) first, then enhance |
| Pafish/al-khaser still detects hypervisor | Profile with VTune / perf to find which check fails; iterate |

---

## Success Criteria

✅ **Phase 5 Complete When:**
1. Windows 11 ISO boots to desktop in Enlil guest
2. pafish runs inside guest with **0 hypervisor detections**
3. al-khaser runs inside guest with **0 hypervisor detections**
4. Custom IET divergence test shows **<100 cycles CPUID overhead** (APERF shadowing working)
5. All system info (SMBIOS, ACPI, CPUID) is realistic
6. Serial console works (debug output visible)
7. Storage and network devices functional
8. Guest is stable under load (no crashes)

---

## Files to Create/Modify

### New Crates
- `crates/enlil-acpi/` → ACPI table synthesis
- `crates/enlil-smbios/` → SMBIOS table synthesis
- `crates/enlil-tpm2/` → Virtual TPM 2.0

### Modify Existing
- `enlil-core/src/lib.rs` → Add KVM backend
- `enlil-core/src/cpuid.rs` → Expand `CpuidFilter`, add cache
- `enlil-core/src/msr.rs` → Add APERF/MPERF interception
- `enlil-core/src/vcpu.rs` → Add TSC offset, LBR fields
- `enlil-devices/src/bus.rs` → Route TPM MMIO

### Configuration
- `enlil-config/` → Add ACPI/SMBIOS customization fields
- Example guest config with Windows 11 settings

---

## Next: Phase 0.2 Implementation Starts Now
