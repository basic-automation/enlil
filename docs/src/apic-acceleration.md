# APICv / AVIC interrupt acceleration

Software LAPIC emulation costs a VM exit on nearly every guest APIC touch
(TPR read, EOI write, self-IPI) and on every interrupt delivery. At scale —
many vCPUs, interrupt-heavy virtio devices — those exits dominate. Enlil
therefore supports hardware interrupt virtualization on both vendors:

| Vendor | Technology | HAL module | Policy |
|---|---|---|---|
| Intel | APICv + posted interrupts | `enlil-hal/src/apicv.rs` | `enlil-devices/src/interrupt/accel.rs` |
| AMD | AVIC | `enlil-hal/src/avic.rs` | `enlil-devices/src/interrupt/accel.rs` |

## Intel: APICv + posted interrupts

Three VM-execution controls compose the feature (Intel SDM Vol. 3C §29):

- **APIC-register virtualization** (secondary bit 8): guest TPR/EOI/self-IPI
  accesses are emulated against the *virtual-APIC page* instead of exiting.
- **Virtual-interrupt delivery** (secondary bit 9): pending/delivered state
  lives in the virtual-APIC page; EOI writes exit only for vectors set in the
  *EOI-exit bitmap* (4×64-bit VMCS fields, default all-clear = no EOI exits).
- **Posted-interrupt processing** (pin-based bit 7): the hypervisor posts an
  interrupt by setting a bit in the 64-byte *posted-interrupt descriptor*
  (PIR bitmap) and — only if the outstanding-notification bit was clear —
  sends one notification IPI. The CPU then injects every pending vector into
  the guest with no VM exit.

Enablement recipe (all host-testable in the HAL):

1. Probe `IA32_VMX_TRUE_PINBASED_CTLS` / `IA32_VMX_TRUE_PROCBASED_CTLS`
   → `apicv::ApicvCaps`.
2. Build `apicv::ApicvConfig { posted_interrupts, x2apic_mode,
   notification_vector }` and `check_against` the caps. Note the SDM §27.3
   constraints the config encodes: x2APIC virtualization and APIC-access
   virtualization are mutually exclusive; posted interrupts require
   virtual-interrupt delivery plus the "acknowledge interrupt on exit"
   VM-exit control (bit 15); virtual-interrupt delivery requires
   "external-interrupt exiting" (primary bit 0).
3. Per vCPU: `apicv::ApicvVcpu` owns the descriptor, virtual-APIC page and
   APIC-access page; `ApicvConfig::vmcs_fields` yields the `VMWRITE` pairs
   (descriptor address, notification vector, page addresses, TPR threshold,
   EOI bitmaps).
4. Delivery: `ApicvVcpu::post(vector)` → `PostAction::Notify` means "send the
   notification IPI now" (vector = NV, destination = descriptor NDST, which
   the backend refreshes on vCPU migration).

## AMD: AVIC

Each vCPU gets a 4 KiB *backing page* holding its virtual APIC state at the
usual xAPIC register offsets; the CPU delivers interrupts straight from it.
Two per-VM tables complete the picture: the *logical APIC ID table* (guest
logical → guest physical ID) and the *physical APIC ID table* (guest physical
→ host physical ID, plus an IsRunning bit per entry). Posting = set the IRR
bit in the target's backing page, then consult its table entry: if IsRunning,
write the guest APIC ID to the *doorbell MSR* (`C001_011Bh`) so the CPU
re-evaluates the page; if not running, the bit simply waits for the next
`VMRUN`. Enablement is one VMCB bit (`INT_CONTROL` bit 13) plus the three
page/table pointers, programmed by `avic::arm_avic`.

## Policy layer

`interrupt::InterruptAccel` (in `enlil-devices`) owns the per-VM posted state
and exposes one entry point: `post(vcpu, vector) -> AccelPostAction`. The
backend refreshes placement with `place_vcpu` (migration) and `set_running`
(guest entry/exit). `AccelMode::select` picks APICv > AVIC > software from
host capabilities. `AccelMode::Software` — and any out-of-range vCPU — falls
back to the existing emulated-LAPIC path, which is unchanged.

## Hardware-gated end-to-end plan

`/dev/kvm` does not exist on the dev host and there is no bare-metal CI
runner, so end-to-end verification is gated on real hardware and documented
here rather than claimed. On an Intel (APICv-capable) and an AMD
(AVIC-capable) bare-metal host, the bring-up sequence is:

1. **Probe**: `RDMSR` the TRUE_CTLS MSRs (Intel) / `CPUID Fn8000_000A`
   (AMD); assert `ApicvCaps` / `feature::AVIC` agree with the HAL decode.
2. **Arm**: create a 2-vCPU guest with `InterruptAccel` in the hardware mode;
   program the VMCS/VMCB fields from the HAL recipe; boot to a shell.
3. **Functional**: drive virtio-blk/net interrupts and IPIs; assert guest
   devices work and no spurious vectors arrive (compare against the software
   path on the same workload).
4. **Performance**: count VM exits per interrupt (exit-reason counters)
   before/after; expect APIC-access/EOI exits to collapse to ~0 and per-IRQ
   exits to drop to the notification-IPI/doorbell only.
5. **Migration**: pin vCPUs across pCPUs mid-workload; assert NDST/table
   updates keep delivery correct and no interrupt is lost (PIR/IRR drain
   check).

On KVM there is nothing to program: APICv/posted interrupts are managed by
the host kernel (`kvm_intel.enable_apicv`) and apply transparently under the
in-kernel irqchip.
