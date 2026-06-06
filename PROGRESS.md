# Enlil — Progress Log

Running log for the autonomous daily routine. Newest entry first. Each entry
records what was done, the research that informed it, exact test results, and
the recommended next step so the next run (which has no memory) can resume.

---

## 2026-06-07 — Session: device IRQ delivery — line wiring, assembled-bus wiring, I/O APIC MMIO (Phase 0.2)

A multi-increment session. Three complete, independently-green, tested commits on the branch,
each a full orient→build→verify trip, forming one coherent arc: **make device interrupts actually
reach a vCPU, and let a guest program the routing.** This took the 06-06(c) recommended next
step #1, path (a) — "wire the IRQ lines to delivery (native/userspace), no KVM needed". `/dev/kvm`
is **still absent on this runner (no nested virt)**, so every increment is the pure-userspace half,
fully exercised by unit/integration tests. Test count: 683 → **693**.

### Increment 1 — `SharedInterruptController` + `clear_irq` (`d1423ad`)
Both the PIT (`attach_irq0`) and UART (`attach_irq_line`) exposed level sinks (`IrqLine`) but
nothing consumed them — interrupts went nowhere. Added
`enlil_devices::interrupt::SharedInterruptController` (`Arc<Mutex<InterruptController>>`, `Send` to
match the `IrqLine` bound) with:
- `.line(irq) -> impl Fn(bool) + Send + use<>`: a level sink that calls `deliver_irq(irq)` on a
  rising edge and the new `InterruptController::clear_irq(irq)` on a falling edge. It satisfies
  **both** `IrqLine` traits (PIT's in `enlil-devices`, UART's in `enlil-core`) through their
  `Fn(bool)+Send` blanket impls — one wiring path, no cross-crate dep.
- `.with(|c| …)` for locked access (program RTEs, read pending vectors), `.new`/`.from_controller`.
- `InterruptController::clear_irq` forwards to `IoApic::clear_irq` (no-op for edge-triggered ISA
  lines, correct deassert for a level-triggered RTE).
- +6 tests (rising-edge delivery, PIT true/false pulse still latches one edge, masked RTE delivers
  nothing, separate lines hit their own vectors, clones share one controller, level-triggered
  clear-on-falling-edge).

### Increment 2 — `DeviceBus::standard_pc_with_interrupts` (`fec39ac`)
`standard_pc` assembled COM1+PIT+PCIe but wired no interrupts. Added a sibling factory taking a
`&SharedInterruptController` that attaches PIT channel-0 → `IRQ_PIT` (0) and COM1 → `IRQ_COM1` (4)
**before** boxing them into the bus (you can't reach a device's non-`PioDevice` methods once boxed).
The caller owns the PIC. New `IRQ_PIT`/`IRQ_COM1` constants. +2 `DeviceBus` tests driving the *real*
bus: enabling the UART THRE interrupt via an `io_out` exit raises IRQ4 into LAPIC 0's IRR; a masked
RTE delivers nothing.

### Increment 3 — `IoApicMmio` front-end (`940aaf2`)
The I/O APIC had `mmio_read/write` methods but wasn't mounted as a bus device, so a guest couldn't
program the redirection table — RTEs stayed masked and *no* device IRQ could ever be delivered.
Added `IoApicMmio` (`MmioDevice` at `0xFEC0_0000`, one 4 KiB page) forwarding `IOREGSEL`/`IOWIN` to
the shared controller's I/O APIC; `DeviceBus::add_ioapic`; and `standard_pc_with_interrupts` now
mounts it. The increment-2 UART test was upgraded to program IRQ4's RTE **through the MMIO aperture**
(2 dword writes, as a guest OS does) — closing the loop end-to-end. +2 `interrupt::line` tests.

### Test results (exact)
- `cargo build` (workspace) → **OK**.
- `cargo fmt --all -- --check` → **OK** (ran `cargo fmt --all`).
- `cargo clippy --all-targets --workspace -- -D warnings` → **OK** (clean; `interrupt::line` is in
  the strict-deny `enlil-devices` crate — used `truncate::u32_of` for the dword write and `const fn`
  on `IoApicMmio::new`, no new `#[allow]`).
- `cargo test --workspace` → **693 passed, 0 failed, 1 ignored** (was 683; +10 across the three
  increments). The 1 ignored is the `/dev/kvm` self-skip.
- `/dev/kvm` paths (`serial_console_smoke`, `kvm_create_vm_and_map_memory`): **not run — no
  `/dev/kvm` (no nested virt)**; they self-skip.
- no_std custom-target build: **N/A** — no crate is `#![no_std]` yet.

### Design note (so tomorrow doesn't re-litigate it)
The **LAPIC is deliberately NOT a bus `MmioDevice`.** It sits at one physical address
(`0xFEE0_0000`) for *every* vCPU and an access implicitly targets the accessing vCPU's LAPIC; a
shared `MmioBus` has no "current vCPU" notion. LAPIC MMIO belongs in the per-vCPU exit path (or the
in-kernel chip via `KVM_CREATE_IRQCHIP`). Only the genuinely-shared I/O APIC is on the bus. Today's
tests therefore enable LAPIC 0 via `pic.with(|c| c.lapics[0].write_register(0x0F0, …))` (the
`LAPIC_SVR` const isn't re-exported from `enlil-devices`).

### Recommended next step (tomorrow)
1. **KVM binding (blocked on `/dev/kvm`):** when a nested-virt runner is available, bind
   `SharedInterruptController` delivery to the in-kernel chip via `vm.set_irq_line(irq, level)` /
   `irqfd`+`EventFd`, and have `KvmBackend` build its bus through `standard_pc_with_interrupts`
   instead of hand-mounting a lone serial port in `serial_console_smoke`. Add a `/dev/kvm`-gated
   test that boots a blob, takes a timer/serial interrupt, and observes the vector. **If still no
   `/dev/kvm`, skip and pick #2/#3.**
2. **Legacy 8259 PIC (pure userspace, unblocked):** early boot starts in PIC mode before the OS
   switches to the I/O APIC. We have no dual-8259 emulation (ICW1-4/OCW, IRR/ISR/IMR, cascade,
   EOI) at `0x20`/`0xA0`. This is a real early-boot gap and pairs with today's interrupt work, but
   it's a sizeable single increment — scope it carefully.
3. **CMOS/RTC (`0x70`/`0x71`, unblocked):** another device a guest touches very early (and a
   common anti-detection probe — must read plausible values). Smaller than the 8259.
4. **PIT hardening (Firecracker #2777)** was reconsidered and **deferred**: its "forbid re-creating
   channels after boot" doesn't map cleanly to our model (no separate channel-create op distinct
   from a control-word write; a running OS legitimately reloads counts). Revisit only with a precise
   threat model and a "boot complete" signal from the run loop.

A multi-increment session (3–4h budget). Four complete, independently-green, tested commits on
the branch, each one trip through orient→build→verify. `/dev/kvm` is **still absent on this
runner (no nested virt)**, so all KVM/guest-boot paths self-skip — every increment below is the
pure-userspace half and is fully exercised by unit/integration tests. Test count: 672 → **683**.

### Increment 1 — ECAM MMIO front-end over a shared root complex (`d764622`)
The 06-06(b) recommended next step. `enlil-devices::pcie` had the legacy CAM (`0xCF8`/`0xCFC`)
front-end but no MMIO ECAM, and CAM owned its `PcieRootComplex` by value so a second front-end
couldn't share the device set.
- Refactored `PciConfigIo` to hold `SharedRootComplex` (`Rc<RefCell<PcieRootComplex>>`); kept
  `new(PcieRootComplex)` (wraps) and added `with_shared` + `shared()`. Removed the `&`-returning
  `root()`/`root_mut()` (can't borrow through a `RefCell`) — two tests updated to `shared().borrow()`.
- Added `EcamSpace` (`MmioDevice`): forwards `mmio_read/write` to `ecam_read/ecam_write` over a
  256-bus / 256 MiB window (`ECAM_SEGMENT_SIZE = 256<<20`) at the root's `ecam_base`. ECAM offset
  == config offset, so no second B/D/F decode. 8-byte access combines two dwords.
- `DeviceBus::add_pcie(root) -> SharedRootComplex` mounts CAM (PIO) + ECAM (MMIO) over one root
  and seeds a default Intel 440FX host bridge at 0:0.0 if absent.
- **Research** (RESEARCH.md 2026-06-06(c)): OSDev PCIe / Linux acpi-info confirm
  `phys = ecam_base + ((bus<<20)|(dev<<15)|(fn<<12)|reg)`; cloud-hypervisor `PciConfigMmio` over a
  shared `PciBus` is the canonical shared-store shape. +6 tests.

### Increment 2 — PIT channel-0 IRQ0 edge sink (`8431951`)
Carried-over step #1's platform-independent half. Added an `IrqLine` trait (`set_level(bool)`,
blanket `impl` for `Fn(bool)+Send`) in `enlil-devices::timer::pit` — mirrors the UART's IRQ4 line
but defined in `enlil-devices` because it's the lower crate (can't depend on `enlil-core`).
`Pit::attach_irq0` stores a `Box<dyn IrqLine>`; `Pit::tick` pulses `true`/`false` once per
channel-0 terminal-count edge, so a level-driven `set_irq_line(0, level)` or
`InterruptController::deliver_irq(0)` sees exactly one edge. +2 tests. (Actual KVM/native delivery
wiring still needs `/dev/kvm`.)

### Increment 3 — bounded 16550 RX FIFO with hardware-accurate overrun (`525c191`)
Outstanding since 06-05 (vm-superio issue #17). `UartState::inject_input` was unbounded
(`rx_fifo.extend`) → a guest that never drains COM1 grows host memory without bound. Capped at
`RX_FIFO_CAPACITY = 4096`; on overflow the incoming byte is dropped and the LSR **Overrun Error**
bit (`0x02`, sticky, cleared on LSR read) is set — the real 16550 behaviour, more transparent than
silently dropping the earliest input, and earliest bytes are preserved. Reused the existing
`lsr_overrides` field. +2 tests.

### Increment 4 — `DeviceBus::standard_pc` factory (`913689c`)
No assembled bus existed (the KVM smoke test hand-built one). `standard_pc(serial_output)` mounts
COM1 UART + 8254 PIT + PCIe (CAM+ECAM+host bridge) at canonical fixed addresses over one shared
root, returning `(DeviceBus, SharedRootComplex)`. `DEFAULT_ECAM_BASE = 0xB000_0000` matches the
MCFG table `enlil-devices::acpi` emits. Ties increments 1–3 into something a backend can
instantiate directly; advances the "Linux to serial shell" milestone. +1 test.

### Test results (exact)
- `cargo build` (workspace) → **OK**.
- `cargo fmt --all -- --check` → **OK** (after `cargo fmt --all`).
- `cargo clippy --all-targets --workspace -- -D warnings` → **OK** (clean; `pcie`/`pit` are in the
  strict-deny `enlil-devices` crate — used `u32_of` for the ECAM 8-byte split, no new `#[allow]`).
- `cargo test --workspace` → **683 passed, 0 failed** (was 672; +11 across the four increments).
- `/dev/kvm` paths (`serial_console_smoke`, `kvm_create_vm_and_map_memory`): **not run — no
  `/dev/kvm` (no nested virt)**; they self-skip.
- no_std custom-target build: **N/A** — no crate is `#![no_std]` yet.

### Recommended next step (tomorrow)
1. **Wire the IRQ lines to delivery.** Both the PIT (`attach_irq0`) and UART (`attach_irq_line`)
   now expose level sinks but nothing consumes them. Two viable paths: (a) **native/userspace** —
   wire them to `enlil_devices::interrupt::InterruptController::deliver_irq(0/4)` (no KVM needed,
   fully testable here: a tick-driven test asserting IRQ0 reaches a LAPIC IRR); (b) **KVM** —
   `vm.set_irq_line(0/4, level)` / `irqfd`, **needs a `/dev/kvm` runner**. Prefer (a) — it's
   unblocked and exercises the in-userspace interrupt path the native-VMX backend will use.
2. **Have the KVM backend build its bus via `DeviceBus::standard_pc`** instead of hand-mounting a
   lone serial port in `serial_console_smoke`, so the guest-boot path gets PIT + PCI for free
   (run only on a KVM runner).
3. **PIT hardening (Firecracker #2777):** once IRQ0 delivery is live, forbid (re)programming PIT
   channels after guest boot and stop channel-0 when idle. Needs a "boot complete" signal from the
   run loop, so sequence it after step 1/2.

## 2026-06-06 (b) — Legacy PCI Configuration Mechanism #1 (0xCF8/0xCFC) on the bus (Phase 0.2)

The previous run's recommended next step #1 (channel-0 → IRQ0 + UART IRQ4 wiring to KVM) is
**still blocked — no `/dev/kvm` on this runner (no nested virt)**. Took step #2 instead, which
the roadmap explicitly names next ("Then PCI config (`0xCF8/0xCFC`)"): the legacy PCI
**Configuration Mechanism #1** port pair. Pure userspace, fully testable here, mirrors the
06-04 serial / 06-06 PIT bus-mounts.

### Situation found
`enlil-devices::pcie` had a complete `PcieRootComplex` with device storage and an **ECAM** MMIO
decode (`ecam_read`/`ecam_write`), plus `PciConfigSpace` (per-function config space with BAR
size-detection) — but **nothing implemented the legacy CAM PIO front-end**. A guest BIOS that
enumerates PCI via `0xCF8`/`0xCFC` *before* bringing up ECAM saw open-bus `0xFF` on those ports
→ no host bridge, no devices found. Neither the ECAM nor the CAM front-end was mounted on any
bus yet; this run wires the CAM half (the one a legacy/early-boot guest hits first).

### What I did (one increment: PCI Mechanism #1 becomes a real bus device)
- **`enlil_devices::pcie::PciConfigIo`** — a `PioDevice` over `0xCF8..=0xCFF` (constants
  `CONFIG_ADDRESS_PORT` / `CONFIG_DATA_PORT`) wrapping a `PcieRootComplex`. A write to the
  `CONFIG_ADDRESS` window (`0xCF8`-`0xCFB`) latches `config_address` (sub-dword writes merge in
  place via `write_address`); the enable bit (31) is honoured. Reads/writes of the `CONFIG_DATA`
  window (`0xCFC`-`0xCFF`) are **byte-steered by port offset** (`reg | (port - 0xCFC)`) and the
  latched B/D/F + register are folded back into an ECAM-style offset (`target_offset`) so decode
  **reuses `PcieRootComplex::ecam_read`/`ecam_write`** — a single shared config-space decode
  path with the future ECAM front-end. A disabled config cycle reads open-bus and drops writes.
- **`DeviceBus::add_pci_config_io`** in `enlil-core/src/device_bus.rs` (thin wrapper over
  `add_pio`, mirrors `add_pit`/`add_serial`).

### Research (informed the design) — logged in `RESEARCH.md` (2026-06-06, Mechanism #1 entry)
PCI Local Bus spec / OSDev "PCI" / Wikipedia "PCI configuration space" (CONFIG_ADDRESS layout,
enable bit, **byte-steering pitfall** — low two reg bits are always zero so sub-dword CONFIG_DATA
access is selected by port offset, masked/shifted in software; Mechanism #1 reaches only the
first 256 bytes). cloud-hypervisor `pci::PciConfigIo` + rust-hypervisor-firmware `src/pci.rs`
(canonical Rust reference: thin address-latching front-end over a shared config store; reuse one
decode path for CAM + ECAM).

### Test results (exact)
- `cargo build` (workspace) → **OK** (0 errors).
- `cargo fmt --all -- --check` → **OK** (after `cargo fmt --all`).
- `cargo clippy --all-targets --workspace -- -D warnings` → **OK** (clean; `pcie` is in the
  strict-deny `enlil-devices` crate — masked truncating casts match the existing `ecam_read`
  idiom, no new `#[allow]`).
- `cargo test --workspace` → **672 passed, 0 failed** (was 662; +10). New: 9 `pcie` unit tests
  (`config_address_latches_and_reads_back`, `byte_writes_to_config_address_merge_in_place`,
  `mechanism1_reads_vendor_and_device_id`, `mechanism1_subword_access_steers_by_data_port_offset`,
  `disabled_config_cycle_reads_open_bus`, `mechanism1_absent_device_reads_all_ones`,
  `mechanism1_write_reaches_config_space`, `write_disabled_config_cycle_is_dropped`,
  `port_range_claims_the_eight_legacy_cam_ports`) + 1 `DeviceBus` integration test
  (`guest_enumerates_pci_through_cf8_cfc_on_the_bus`).
- `/dev/kvm` paths: **not run — no `/dev/kvm` (no nested virt)**; they self-skip.
- no_std custom-target build: **N/A** — no crate is `#![no_std]` yet.

### Recommended next step (tomorrow)
1. **Mount ECAM on the MMIO bus** over the *same* `PcieRootComplex` the CAM front-end wraps.
   This needs shared ownership of the root complex (the CAM `PciConfigIo` currently owns it by
   value) — wrap it in `Rc<RefCell<PcieRootComplex>>` (or an `Arc<Mutex>` if it must cross
   threads) and give both `PciConfigIo` and a new `EcamSpace` MMIO device handles, so CAM and
   ECAM mutate one device set. Add a host bridge (`create_host_bridge`) at BDF 0:0.0 by default
   so a guest finds *something* at boot. Pure userspace, fully testable here.
2. **Channel-0 → IRQ0 + UART IRQ4 wiring to KVM** (carried over, still blocked). Give `Pit` an
   `IrqLine` sink pulsed from `Pit::tick`; wire both it and the UART `IrqLine` to KVM via
   `set_irq_line(0/4, level)` / `irqfd`. **Needs a `/dev/kvm`-capable runner** — if still
   unavailable, do the platform-independent half (PIT `IrqLine` + a `tick`-driven edge test).
3. **Hardening (Firecracker #2777):** once IRQ0 is live, forbid (re)programming PIT channels
   after guest boot, and stop channel-0 when idle.

## 2026-06-06 — 8254 PIT on the device bus + read-back command (Phase 0.2)

The previous run's recommended next step was (1) wire the UART `IrqLine` to KVM — **blocked,
no `/dev/kvm` on this runner (no nested virt)** — or (2) the next early-boot PIO devices,
**PIT (`0x40-0x43`)** and PCI config (`0xCF8/0xCFC`). Picked (2)'s PIT half: it's pure
userspace, fully testable here, and mirrors the 06-04 serial bus-mount.

### Situation found
`enlil-devices::timer::Pit` was a complete-ish i8254 emulation (3 channels, modes 0-3 ticking,
single-channel latch, `channel0_frequency`) **but it implemented none of the bus traits** —
nothing called `PioDevice`, so a guest touching `0x40-0x43` got open-bus `0xFF` (a dead
giveaway / breaks early timer calibration). It also **ignored the read-back command**
(`write_command` returned early for `channel_idx == 3`), and had no null-count status bit.

### What I did (one increment: the PIT becomes a real, more-transparent bus device)
- **`impl PioDevice for Pit`** in `enlil-devices/src/timer/pit.rs` over `0x40..=0x43`
  (`PIT_PORT_BASE`/`PIT_PORT_COUNT` constants): byte-wide reads return the low byte of the
  `u32`, writes consume the low byte. The bus passes the absolute port, which `read_port`/
  `write_port` already decode (`port & 0x3`, `0x43` = command).
- **Read-back command** (`Pit::read_back`): control word bits 7-6 = `11` → `/COUNT` (bit 5
  low) latches each *selected* channel's current count, `/STATUS` (bit 4 low) latches a status
  byte; bits 3-1 select channels 2/1/0. A pending latch isn't overwritten by a second
  read-back (datasheet). `read_data` now delivers a latched **status byte before** any latched
  count.
- **Status byte + `null_count`**: added `CounterStatus { output, null_count }` (a sub-struct so
  `PitChannel` stays ≤ 3 bools — same pattern as the existing `ByteLatch`, avoids the
  zero-`#[allow]` `struct_excessive_bools` deny). `null_count` is set on a control-word write
  and cleared on count load; `status_byte()` packs OUT-pin/null-count/RW-access/mode/BCD per
  the 8254 datasheet. Added `ChannelMode::bits()` / `AccessMode::bits()` for clean enum→field
  encoding (no `as` casts).
- **`DeviceBus::add_pit(Pit)`** in `enlil-core/src/device_bus.rs` (thin wrapper over `add_pio`,
  mirrors `add_serial`).

### Research (informed the design) — logged in `RESEARCH.md` (2026-06-06)
Intel 8254 datasheet / OSDev PIT (read-back command + status-byte layout), QEMU/Linux
`i8254.c` + `KVM_CREATE_PIT2` (KVM normally emulates the PIT **in-kernel**, so it bypasses our
userspace bus — but Enlil's own native-VMX backend will have no in-kernel chip and **must**
carry a userspace PIT, so this is correct), and Firecracker #2777 (hardening: forbid creating
PIT channels after boot; channel-0 left running costs steal time).

### Test results (exact)
- `cargo build` (workspace) → **OK** (0 errors).
- `cargo fmt --all -- --check` → **OK**.
- `cargo clippy --all-targets --workspace -- -D warnings` → **OK** (clean; PIT is in the
  strict-deny `enlil-devices` crate; the `CounterStatus` extraction keeps it under the
  `struct_excessive_bools` threshold with no `#[allow]`).
- `cargo test --workspace` → **662 passed, 0 failed** (was 654; +8). New: 6 PIT unit tests
  (`test_pio_device_claims_four_command_ports`, `test_pio_write_programs_channel_low_byte_only`,
  `test_pio_read_returns_count_in_low_byte`, `test_read_back_latches_status_before_count`,
  `test_read_back_null_count_set_before_load`, `test_read_back_only_selected_channels`) + 2
  `DeviceBus` integration tests (`pit_on_the_bus_programs_and_reads_back_a_channel_count`,
  `pit_and_serial_coexist_on_the_pio_bus`).
- `/dev/kvm` paths (`serial_console_smoke`, `kvm_create_vm_and_map_memory`): **not run — no
  `/dev/kvm` (no nested virt)**; they self-skip. No PIT IRQ delivery was exercised (none was
  written this run).
- no_std custom-target build: **N/A** — no crate is `#![no_std]` yet.

### Recommended next step (tomorrow)
1. **Channel-0 → IRQ0 + UART IRQ4 wiring.** Give `Pit` an `IrqLine`-style sink (mirror the
   UART's `attach_irq_line`) pulsed from `Pit::tick` when channel 0 reaches terminal count,
   and wire both it and the existing UART `IrqLine` to KVM via `vm.set_irq_line(0/4, level)`
   through the in-kernel IRQ chip (or `irqfd`/`EventFd`). Add a `/dev/kvm`-gated test. **Needs
   a KVM-capable runner** — if still unavailable, do the platform-independent half (the PIT
   `IrqLine` + a `tick`-driven edge test) and leave the `set_irq_line` call for the KVM runner.
2. **PCI config space (`0xCF8/0xCFC`)** as a `PioDevice` — the address/data port pair a guest
   uses to enumerate the PCI bus early in boot. Pure userspace, fully testable here.
3. **Hardening (Firecracker #2777):** once IRQ0 is live, forbid (re)programming PIT channels
   after guest boot, and stop channel-0 when idle to avoid steal time.

## 2026-06-05 — Interrupt-driven 16550: IER-honoured IIR + pluggable IRQ4 line (Phase 0.2)

The previous run's recommended next step: give `SerialPort` an IRQ4 sink fired on
RX-available / THR-empty, honouring IER and producing the correct IIR identification byte.
Done — the platform-independent half (fully testable here); the KVM `set_irq_line` wiring is
now the single remaining sub-task (Linux/`/dev/kvm`-only).

### Situation found
`UartState` was polled-only: it stored `ier`/`mcr` and a constant `iir = IIR_NO_INTERRUPT`
field that `read_register(IIR_REG)` returned verbatim — it never reflected pending sources and
never signalled an IRQ. Linux's 8250 driver can run polled (so the 06-04 smoke test works),
but its **default is interrupt-driven**, so a real serial shell needs IRQ4. No `UartState`
register field is referenced outside `serial.rs`, so the IIR could be made computed safely.

### What I did (one increment: the UART interrupt model + IRQ-line sink)
- **Computed IIR honouring IER** (`compute_iir`): highest-priority *enabled and pending*
  source — RX-available (`0x04`) outranks THR-empty (`0x02`); `0x01` = none. Removed the
  stored `iir` field.
- **THR-empty latch** (`thr_empty_pending`): set when ETBEI transitions on (THR is always
  empty in emulation) and re-armed on every TX (`write_data`); **cleared by an IIR read**
  when THRE is the reported source (per PC16550D / the linux-serial IIR/LSR-ordering pitfall).
  RX-available is level-derived from the FIFO + ERBFI (cleared by draining RBR, *not* by an
  IIR read).
- **`IrqLine` trait + pluggable sink:** `UartState::set_irq_line` / `SerialPort::attach_irq_line`
  store a `Box<dyn IrqLine>`; `update_irq` recomputes the level after *every* state-changing
  access (register read/write, `inject_input`, `read_byte`) and calls `set_level` only on a
  real edge — matching QEMU's `qemu_set_irq` and vm-superio's eventfd `Trigger`. Blanket
  `impl IrqLine for Fn(bool) + Send` so the KVM backend can wire it with a closure
  (`move |level| vm.set_irq_line(4, level)`). `interrupt_pending()` exposes the level for a
  poll-after-exit driver that doesn't use the callback.
- **IER/IIR bit constants** added (`IER_RX_AVAILABLE`/`IER_THR_EMPTY`/…, `IIR_THR_EMPTY`/
  `IIR_RX_AVAILABLE`). IIR FIFO bits 6-7 deliberately left clear (FCR unemulated, like
  vm-superio) so the guest probes us as a plain 8250 — works polled *or* interrupt-driven.

### Research (informed the design) — logged in `RESEARCH.md` (Part III, 2026-06-05)
rust-vmm **`vm-superio` `Serial`** + **PC16550D datasheet**: confirmed the IIR priority,
THRE-acknowledge-on-IIR-read semantics, and the `Trigger`-style level sink. **Pitfall**
(linux-serial "IIR/LSR out-of-sync"): the THRE latch must be re-evaluated relative to read
ordering — `update_irq` runs *after* each access mutation so the asserted level always matches
the computed IIR.

### Test results (exact)
- `cargo fmt --all -- --check` → **OK**
- `cargo clippy --all-targets --workspace -- -D warnings` → **OK** (clean)
- `cargo test --workspace` → **654 passed, 0 failed** (was 647; +7). `enlil-core`: 122 (was 115).
  New: `test_no_interrupt_when_sources_disabled`, `test_rx_available_interrupt`,
  `test_thr_empty_interrupt_set_and_acked_by_iir_read`, `test_rx_outranks_thr_empty_in_iir`,
  `test_irq_line_edges_on_rx`, `test_irq_line_accepts_a_closure`,
  `test_serial_port_drives_irq_line`.
- `serial_console_smoke` + `kvm_create_vm_and_map_memory`: **self-skipped — `/dev/kvm` not
  present (no nested virt).** The interrupt logic added this run is pure userspace and is
  fully exercised; the KVM IRQ delivery path is not yet written.
- no_std custom-target build: **N/A** — no crate is `#![no_std]` yet.

### Recommended next step (tomorrow)
1. **Wire `IrqLine` to KVM.** In `KvmBackend`, after each `run_vcpu` (or via an `irqfd`/
   `EventFd` registered with `KVM_IRQFD`), assert/deassert IRQ4 through the in-kernel IRQ chip
   with `vm.set_irq_line(4, level)`. The simplest first cut: a poll-after-exit driver that
   calls `serial.interrupt_pending()` and `set_irq_line(4, that)`. Add a `/dev/kvm`-gated test
   that boots a blob enabling ETBEI/ERBFI and verifies an IRQ is taken (needs a KVM runner).
2. Then the next early-boot PIO devices: **PIT** (`0x40-0x43`) and **PCI config**
   (`0xCF8/0xCFC`) as `PioDevice`s on the bus.
3. Cap `UartState`'s RX `VecDeque` (drop-oldest) before wiring a real host-stdin source
   (vm-superio issue #17) — still outstanding.

---

## 2026-06-04 — 16550 UART on the device bus + real-mode KVM serial smoke test (Phase 0.2)

The previous run's recommended next step: register the 16550 UART as a `PioDevice` at
COM1 and add a `/dev/kvm`-gated test that boots a tiny blob writing to COM1. Done.

### Situation found
`enlil-core::serial` had a complete `UartState` (16550 register file: IER/IIR/LCR/LSR/
MCR/MSR/SCR + DLAB divisor, RX FIFO, pluggable `SerialOutput` sink) and a
`SerialMultiplexer`, but **nothing implemented `enlil_devices::bus::PioDevice`**, so the
UART could not be mounted on the real `DeviceBus`/`VmExitHandler` path landed on 06-03.
The `device_bus` tests used a hand-rolled `FakeSerial`, never the real UART.

### What I did (one increment: the UART becomes a real bus device, verified end-to-end)
- **`enlil-core::serial::SerialPort`** — a new `PioDevice` adapter bundling a `UartState`
  with its COM base port, claiming `[base, base+8)`. `pio_read/pio_write` map the absolute
  guest port to the 0–7 register offset (`port - base`) and forward to the UART; serial
  registers are byte-wide so writes take the low byte. `com1()` convenience ctor.
- **`DeviceBus::add_serial(SerialPort)`** helper (thin wrapper over `add_pio`).
- **`SerialOutputMode::Shared(Arc<Mutex<Vec<u8>>>)`** — a new sink mode that appends TX
  to a *caller-owned* buffer, so guest serial output stays observable after the device is
  moved into the bus (the host console/logger reads the same `Arc`). This is what made a
  real end-to-end assertion possible, and is the primitive the console/display path needs.
- **`KvmBackend::prepare_real_mode_vcpu(index, entry)`** — minimal flat 16-bit real-mode
  setup (all segment bases 0, `rip=entry`, `rflags=0x2`) so a small real-mode blob can run.
  Genuinely useful primitive (compiled, not executed here — no `/dev/kvm`).
- **Tests:** 5 `SerialPort` unit tests (range claim, port→offset mapping, TX to sink, LSR
  TX-ready, RX injection round-trip), 1 `Shared`-mode test, 1 `real_serial_uart_on_the_bus`
  (drives the real UART through the real `VmExitHandler` and asserts "OK" reached the
  shared sink — runs here, no KVM), and 1 `/dev/kvm`-gated **`serial_console_smoke`**:
  assembles `mov dx,0x3F8; mov al,'O'; out; mov al,'K'; out; hlt`, maps it at gphys 0x1000,
  runs it via `KvmBackend::run_vcpu` with the `DeviceBus` handler, and asserts the guest
  reached HLT and "OK" landed in the sink.

### Research (informed the design) — logged in `enlil-research-review.md` (2026-06-04)
rust-vmm **`vm-superio` `Serial`**: confirms our `UartState` register set is complete
(same registers; FCR need not be emulated — FIFO is always-on). **Pitfall surfaced:**
vm-superio raises RX/THR interrupts via a `Trigger`/eventfd; our UART is **polled-only**
(stores IER, never raises IRQ4). Linux's 8250 driver can run polled so a shell works, but
interrupt-driven mode needs IRQ4 into the in-kernel IRQ chip — logged as the next step.
Also noted vm-superio issue #17 (unbounded RX) → cap `inject_input` before wiring host stdin.

### Test results (exact)
- `cargo fmt --all -- --check` → **OK**
- `cargo clippy --all-targets --workspace -- -D warnings` → **OK** (clean)
- `cargo test --workspace` → **647 passed, 0 failed** (was 639; +8). `enlil-core`: 115.
- `serial_console_smoke` + `kvm_create_vm_and_map_memory`: **self-skipped — `/dev/kvm` not
  present on this runner (no nested virt).** `prepare_real_mode_vcpu` and the run loop are
  compiled but NOT executed here; needs a KVM-capable runner to actually exercise.
- no_std custom-target build: **N/A** — no crate is `#![no_std]` yet.

### Recommended next step (tomorrow)
1. **Interrupt-driven serial:** give `SerialPort` an IRQ4 sink (an `EventFd` raised via
   `KVM_IRQ_LINE` through the in-kernel IRQ chip) fired on RX-available / THR-empty,
   honoring IER and producing the correct IIR identification byte. This is what a default
   Linux 8250 driver expects and unblocks an actual interactive serial shell. The polled
   path already works (this run), so this is the next transparency increment.
2. Then the next early-boot PIO devices: **PIT** (`0x40-0x43`) and **PCI config**
   (`0xCF8/0xCFC`) as `PioDevice`s on the bus.
3. Cap `UartState`'s RX `VecDeque` (drop-oldest) before wiring a real host-stdin source
   (vm-superio issue #17).

---

## 2026-06-03 — Real device bus + KVM `VmExitHandler` bridge (Phase 2 / 0.2)

The previous run's recommended next step: give guest exits somewhere to go. Done —
the device bus is now a real dispatcher and the KVM backend is wired to it.

### Situation found
`enlil-devices::bus` was an **unused stub**: `PioBus`/`MmioBus` mapped a base address
to a device *index* (`BTreeMap<addr, usize>`) with **no owned devices, no dispatch, and
no upper range bound** — so `lookup(port)` returned the nearest-lower base even for a
port far past that device. `PioDevice`/`MmioDevice` traits existed but had zero
implementors or callers. `enlil-core`'s `kvm_backend::VmExitHandler` had nothing behind
it (`exit_handler.rs` is dead stubs). `enlil-core` did not depend on `enlil-devices`,
though `ARCHITECTURE.md` specifies `enlil-core ──► enlil-devices` and Phase 2 = "PIO/MMIO
exit handling in enlil-core".

### What I did (one increment: the PIO/MMIO dispatch path)
- **`enlil-devices/src/bus.rs` rewritten** into a real dispatching bus:
  - `PioBus`/`MmioBus` now **own** `Box<dyn PioDevice/MmioDevice>` keyed by base in a
    `BTreeMap`, registered over an explicit half-open range `[base, end)`. `register`
    **rejects empty ranges and overlaps** (checks the predecessor's `end` and the
    successor's `base`) → `Result<(), BusError>`.
  - Lookup finds the greatest base `<= addr` **and confirms `addr < end`** (fixes the
    stub's missing upper bound).
  - Byte-slice front door (`read(addr, &mut [u8])` / `write(addr, &[u8])`) matching the
    KVM exit / `VmExitHandler` shape, with little-endian width conversion to the typed
    `pio_read(port,size)->u32` / `mmio_read(offset,size)->u64` device methods. PIO passes
    the absolute port; MMIO passes `addr - base` as the offset.
  - **x86 open-bus semantics** for unmapped addresses: reads fill `0xFF`, writes dropped.
- **`enlil-core` ⇒ depends on `enlil-devices`** (architecturally-sanctioned edge) and a
  new **`enlil-core/src/device_bus.rs`**: `DeviceBus { pio, mmio }` with `add_pio`/
  `add_mmio` helpers and **`impl VmExitHandler for DeviceBus`** forwarding `io_in/io_out/
  mmio_read/mmio_write` straight to the bus — one bus, one decode path (rust-vmm
  `IoManager` model; see research note 2026-06-03).
- **Repo hygiene:** removed three stray `enlil-core/src/memory_dump_{1,2,3}.txt` dumps of
  `memory.rs` (junk from the `1f00b52` commit; not Rust, not referenced).

### Research (informed the design) — logged in `enlil-research-review.md` (2026-06-03)
rust-vmm `vm-device` `IoManager`/`PioManager`+`MmioManager` and Dragonball `dbs-device`
both register devices over an **address range** and route by the range that *contains* the
access — confirming the range-keyed, overlap-rejecting bus and the byte-slice/open-bus
front door over typed device accessors.

### Test results (exact)
- `cargo fmt --all -- --check` → **OK**
- `cargo clippy --all-targets --workspace -- -D warnings` → **OK** (clean; bus.rs is in
  the strict-deny `enlil-devices` crate — used edition-2024 let-chains to satisfy
  `collapsible_if`).
- `cargo test --workspace` → **639 passed, 0 failed** (was 630; +6 `bus` tests, +3
  `device_bus` bridge tests). New tests cover: PIO/MMIO read LE width, write reaching the
  owning device at the right port/offset/width (verified via `Rc<RefCell>` shared logs),
  open-bus on unmapped, overlap + empty-range rejection, and the `VmExitHandler` bridge
  end-to-end (a fake COM1 capturing guest bytes; MMIO offset routing).
- KVM `/dev/kvm` path: **not run (no nested virt on this runner)** — unchanged; this
  increment is pure userspace dispatch and needs no KVM.
- no_std custom-target build: **N/A** — no crate is `#![no_std]` yet (`bus.rs` uses
  `std::collections::BTreeMap`).

### Recommended next step (tomorrow)
1. **Register a real device on the bus.** Wrap the 16550 UART in `enlil-core::serial`
   (or a thin `enlil-devices` adapter) as a `PioDevice` at COM1 `0x3F8..0x400` and add it
   to `DeviceBus` so guest serial writes reach the console. Then a `/dev/kvm`-gated
   integration test that loads a tiny real-mode code blob which writes a byte to `0x3F8`
   and `hlt`s, runs it via `KvmBackend::run_vcpu` with the `DeviceBus` handler, and asserts
   the byte arrived — this finally exercises the full exit→device path and advances the
   Phase 0.2 "Linux to serial shell" milestone. (Needs a KVM-capable runner to actually
   run; otherwise it self-skips like the existing `kvm_create_vm_and_map_memory`.)
2. After serial: PIT (`0x40-0x43`) and the PCI config-space ports (`0xCF8/0xCFC`) are the
   next PIO devices a guest touches early in boot.

---

## 2026-06-02 (later still) — Zero `#[allow]`, strict clippy, all lints fixed for real

Per request: removed **every** `#[allow]` attribute (120 of them), kept the strict
`#![deny(clippy::all, clippy::pedantic, clippy::nursery)]`, and fixed every lint —
and every cascade lint the fixes produced — instead of suppressing.

- **Register/byte casts (~134):** added a small `enlil-devices::truncate` module
  (`Widen` trait + `u8_of`/`u16_of`/`u32_of`/`usize_of`, little-endian byte
  reconstruction — no lossy `as`, no panic) and rewrote intentional truncations to
  use it; signed TSC math uses `i64::cast_signed`/`u64::cast_unsigned`. (Renamed the
  trait method off `widen` to `to_u64` to dodge a future-std name collision.)
- **dead_code (~25):** re-exported genuinely-public-but-unreferenced API
  (`LoopbackBackend`, `PipeBackend`, `MsiCapability`, `IOAPIC_BASE`, `DeviceStatus`,
  `ClipboardEntry`), added accessors for stored-but-unread config fields, removed
  truly-unused private constants.
- **docs:** added accurate `# Panics`/`# Errors` sections (lock poisoning, asserts)
  and removed infallible `unwrap`s (`Trb::from_bytes` via explicit byte arrays).
- **structure:** `unused_self` → associated fns; `option_if_let_else` → `map_or`;
  `match_same_arms` combined; `unnecessary_wraps` (PS/2 `handle_data_byte` → `u8`);
  `significant_drop_tightening` (scoped lock guards); `vec_init_then_push` → `vec![]`;
  split two `too_many_lines` fns (`build_acpi_tables`, `CpuidStealthTable::build`);
  refactored two `struct_excessive_bools` (`PitChannel`→`ByteLatch`, `RedirectionEntry`
  →`RteFlags` sub-structs).

**Verified (exact CI commands):** `cargo fmt --all -- --check` OK · `cargo clippy
--all-targets --workspace -- -D warnings` OK · `cargo test --workspace` **630 passed,
0 failed**. `#[allow]` count in `*/src`: **0**. Strict deny groups still in place.

---

## 2026-06-02 (later) — Lint/format cleanup: CI "Check & Lint" is now green

Followed up on the build-fix PR by paying down the fmt + clippy debt that had
accumulated while the workspace was unbuildable (so CI had never passed).

- **`cargo fmt --all`** across the repo (87 files; formatting only).
- **Clippy under `deny(all, pedantic, nursery)`**: auto-fix pass + manual fixes.
  - `enlil-config/platform/hal/std/core`: fully fixed (no-op waker → `Waker::noop()`,
    slice instead of `&mut Vec`, iterator instead of indexed loop, `#[allow(dead_code)]`
    with rationale on in-progress Phase-5 table-synthesis scaffolding).
  - `enlil-devices` (the deep hardware-emulation crate): fixed the substantive lints
    (`semicolon_if_nothing_returned`, `unused_must_use` on an ignored `RoutingDecision`,
    `overly_complex_bool_expr`, `manual_clamp`, `manual_checked_ops`,
    `decimal_bitwise_operands`, `missing_const_for_fn` ×8, literal grouping,
    `items_after_statements`, unused vars/parens/mut, `dead_code` spec constants), and
    added a **curated, documented `#![allow(...)]`** for the pedantic/nursery lints that
    are domain-noise for register/byte code (`cast_possible_truncation`, `similar_names`,
    `unused_self`, `missing_panics_doc`, `match_same_arms`, `option_if_let_else`, …),
    consistent with the crate's existing `module_name_repetitions` allow. The strict
    `deny` groups stay in place so new code is still linted.
  - **Important footgun fixed:** the earlier `cargo clippy --fix` had stripped two
    test-only imports (`UsbDeviceClass`, `TrbType`), breaking the `enlil-devices` test
    build; restored them as test-scoped `use`s.
- **All three CI steps now pass locally** (the exact commands from `.github/workflows/ci.yml`):
  - `cargo fmt --all -- --check` → OK
  - `cargo clippy --all-targets --workspace -- -D warnings` → OK
  - `cargo test --workspace` → **629 passed, 0 failed**
- KVM `/dev/kvm` integration test still self-skips (no nested virt on this runner).

**Net:** PR #1 now contains the build fix **and** a green CI. The over-strict-but-never-
enforced lint policy was recalibrated to an enforceable one rather than churning 100+
register-code sites. Next step unchanged: wire the KVM backend into the run loop by
implementing `VmExitHandler` for `enlil-devices::bus::Bus`.

---

## 2026-06-02 — Restore buildable workspace + reconstruct KVM backend (Phase 0.2 / 5)

**Branch:** `claude/dazzling-heisenberg-quAdo` (harness policy requires a feature
branch + PR; DAILY-ROUTINE's "commit to master" was overridden by that policy).

### Situation found
The previous commit (`1f00b52 "Add KVM backend and Windows guest support"`) left
**`master` unbuildable**. `master` and the feature branch were identical. Root causes:

1. **`enlil-core/src/kvm_backend.rs` was corrupt** — it contained only the 80-byte
   string `[content omitted from context — 27455 bytes were written successfully to
   disk]` (a tool placeholder). The real ~27 KB of KVM code was *never committed*
   (git history confirms the blob was 80 bytes at the only commit that touched it).
   The em-dash in that placeholder was the `unknown start of token` compile error.
2. **`enlil-core/src/affinity.rs`** — `get_current_affinity()` was `const fn` but
   called a non-const function (`E0015`).
3. **`enlil-devices/src/net/tap.rs`** — imported a non-existent trait
   `NetworkBackend` (real name `NetBackend`), was missing the trait's
   `has_pending_rx` method, and used `libc` without it being a dependency.

### What I did (one increment: get master green again + a real KVM backend)
- **Reconstructed `kvm_backend.rs`** from scratch as a real, compiling backend:
  - Platform-independent exit model: `GuestExit` enum (+ `outcome()` →
    `RunOutcome::{Continue,Stopped}`) and a `VmExitHandler` trait
    (`io_in/io_out/mmio_read/mmio_write`) with a `RecordingHandler` for tests.
  - Linux/KVM impl (`#[cfg(target_os = "linux")]`): `KvmBackend::{new, map_memory,
    create_vcpu, run_vcpu, …}` over `kvm-ioctls`. `new()` opens `/dev/kvm`, checks
    API v12, creates the VM, sets the x86 TSS address, and creates the in-kernel
    IRQ chip. `run_vcpu()` translates every `VcpuExit` into a `GuestExit`,
    dispatching data transfer through the handler. `is_kvm_available()` gates tests.
- **Fixed `affinity.rs`**: dropped the erroneous `const`, removed an unused import.
- **Fixed `tap.rs`**: corrected the trait name, implemented `has_pending_rx` via
  non-blocking `poll(2)`, added `libc` to `enlil-devices` (linux deps), and cleaned
  the file's clippy issues in the code I touched (c-string literal instead of a
  fallible `CString::new().unwrap()`, inlined format args, `#[must_use]`, `# Errors`
  docs, lossless casts).
- **Repo hygiene**: removed ~54 tracked throwaway files from the repo root
  (`fix_*.ps1`, `count*.py`, `*_out.txt`, `kvm_parts/`, `assemble_kvm.py`, etc. —
  the prior run's failed piecewise-assembly scaffolding, with hardcoded `D:\` paths)
  and added `/scratch` to `.gitignore`.

### Research (informed the design)
Logged under `enlil-research-review.md` → "2026-06-02":
- rust-vmm **`vm-device` `IoManager`** uses exactly `pio/mmio _read/_write` dispatch
  → validates our `VmExitHandler` shape. Next integration must implement
  `VmExitHandler` *for* `enlil-devices::bus::Bus`, not a parallel decode path.
- `kvm-ioctls` `VcpuExit`: read exits' slices must be filled *before* the next
  `KVM_RUN` → our `run_vcpu` fills in place before returning the owned summary.
- `KVM_SET_USER_MEMORY_REGION2` + `guest_memfd` (2025) is for confidential VMs and
  is **incompatible** with host-side memory introspection; we deliberately use the
  classic `set_user_memory_region` so we can synthesise/inject ACPI/SMBIOS. The
  region2 path is a Phase 8/CVM concern only.

### Test results (exact)
- `cargo build` (whole workspace): **0 errors** (pre-existing dead-code warnings
  remain in untouched files: `smbios.rs`, `vtpm.rs`, `ps2/`, `usb/xhci/`, …).
- `cargo test` (whole workspace): **629 passed, 0 failed**, 17 test binaries.
- New `kvm_backend` tests: 5 passed (`terminal_exits_stop_the_loop`,
  `resumable_exits_continue_the_loop`, `recording_handler_captures_writes_and_read_sizes`,
  `default_handler_is_a_noop`, `kvm_create_vm_and_map_memory`).
- **`kvm_create_vm_and_map_memory` self-skipped: `/dev/kvm` is NOT present on this
  runner (no nested virt).** The real VM-creation / memory-map / vCPU path is
  compiled but **NOT executed** here — it prints "skipping" and returns. Not a
  fabricated pass; needs a KVM-capable runner to actually exercise.
- No_std custom-target build (`x86_64-unknown-enlil.json`): **N/A** — no crate is
  currently `#![no_std]`, so there is nothing to build for that target yet.

### Known issue for a future run (do NOT claim "complete" until addressed)
- **Workspace-wide clippy is NOT green.** Because the build had been broken,
  `cargo clippy` never ran across the tree; with `#![deny(clippy::all, pedantic,
  nursery)]` there are now **~200+ pre-existing lint errors** unmasked across
  `enlil-core` (`smbios.rs`, `vtpm.rs`, `acpi.rs`, …) and `enlil-devices`
  (`acpi/`, `usb/`, `ps2/`, `net/backend.rs`, …): mostly `missing_const_for_fn`,
  `doc_markdown` (backticks), `must_use_candidate`, `# Errors`/`# Panics` docs, and
  `cast_possible_truncation`. **My new/edited code is clippy-clean**; the debt is
  pre-existing. This is large and mechanical — do it crate-by-crate as its own
  increment(s), not in one shot.

### Recommended next step (tomorrow)
1. **Best next increment:** wire the KVM backend into the run loop — implement
   `VmExitHandler` for `enlil-devices::bus::Bus` (or a thin adapter) and replace
   the stub `enlil-core::exit_handler` functions, so serial/MMIO exits reach real
   devices. Add a `#[cfg]`-gated integration test that boots a tiny code blob to a
   `HLT` when `/dev/kvm` is available. This is the natural continuation and unblocks
   the Phase 0.2 "Linux to serial shell" milestone.
2. **Or, lower-risk:** start paying down the clippy debt one crate at a time
   (`enlil-core` first) so `cargo clippy` can become a real gate.
3. Consider asking for a KVM-enabled runner so the guest-boot path can actually run.
