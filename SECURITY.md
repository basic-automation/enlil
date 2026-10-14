# Security Policy

## Project status — read this first

**Enlil 1.0.0 is the first stable release. It has not been independently audited.**

The bare-metal path boots from UEFI, enters AMD SVM guest mode, and runs real
guests directly on the hardware with no host OS underneath. Per-guest isolation
in 1.0 rests on nested page tables (mixed 2 MiB / 4 KiB granularity with guard
pages), carved memory regions, VMCB intercept control (CPUID / MSR / RDTSC /
exception virtualization), and the transparency (stealth) layer.

The full production picture is not finished:

- The privileged service VM (ROADMAP.md Phase 6.6) and the end-to-end
  bare-metal milestone (Phase 6.7) remain open.
- Enlil's own IOMMU DMA-remapping programming (Phase 6.4) is **partially** in
  place: the VT-d DMA-remapping tables are programmed (root + context tables,
  per-guest domains, translation enabled), but interrupt remapping and the
  AMD-Vi side are still pending, so treat device passthrough as **partially
  DMA-confined on the bare-metal path**.
- Side channels inherent to shared hardware (timing, cache/memory-bus
  contention, transient execution) are not mitigated.
- Nothing above has been independently audited.

Concretely, **do not run untrusted or hostile guests under Enlil if your threat
model requires containment of DMA-capable devices or side-channel resistance**,
and do not rely on it to isolate workloads that matter until the gaps above are
closed and reviewed. Treat every guest as sharing a trust domain with the host
for those vectors.

## Supported versions

| Version | Supported |
| --- | --- |
| `1.0.x` | ✅ Security fixes |
| `0.1.0-beta.*` and anything older | ❌ No |

There is no long-term support branch. Fixes land on `master` and appear in the
next 1.0.x release; there are no backports.

## Reporting a vulnerability

**Please report privately — do not open a public issue for a security problem.**

Use GitHub's private vulnerability reporting on this repository:
**Security → Report a vulnerability** (or
[github.com/basic-automation/enlil/security/advisories/new](https://github.com/basic-automation/enlil/security/advisories/new)).
That channel is private to the maintainers and lets us discuss and prepare a fix before
anything is disclosed.

Helpful things to include, as far as you have them: the affected version or commit, the
host CPU and virtualization extension (Intel VT-x / AMD-V), the guest OS, a description of
the impact, and the smallest reproduction you can manage — a failing test, a guest payload,
or a serial log from the QEMU+OVMF harness is ideal.

**What to expect.** This is a small project, so please calibrate accordingly: an
acknowledgement is the first goal, then an assessment of whether the report is in scope and
reproducible, then a fix. Timelines are best effort rather than contractual. You will be
credited in the advisory and release notes unless you would rather not be.

## What counts as a vulnerability

Because Enlil is a hypervisor, the interesting boundaries are the ones between a guest and
everything it should not reach:

- **Guest-to-host escape** — any path by which guest code executes, reads, or writes
  outside its own VM: hypervisor memory, the kernel heap, page tables, or host devices.
- **Guest-to-guest isolation failure** — one guest reading, writing, or influencing
  another's memory, vCPU state, or devices. The NPT/EPT separation and the per-guest
  memory carve are the load-bearing pieces here.
- **DMA escape** — a passed-through device programmed to DMA outside its guest's assigned
  memory. This is a known residual gap on the bare-metal path (see above), so reports in
  this area are welcome as hardening input even though the underlying work is tracked in
  [`ROADMAP.md`](ROADMAP.md) Phase 6.4 rather than treated as a surprise.
- **Hypervisor detection** — Enlil's stated goal is that a guest cannot tell it is
  virtualized, so a reliable detection vector (a CPUID tell, a timing side channel that
  distinguishes trapped from native execution, an ACPI/SMBIOS artifact, an inconsistent
  TSC/PIT relationship) is treated as a real defect, not a cosmetic one.
- **Host privilege escalation** through the management console, the Unix-socket control
  protocol, or the configuration parser.
- **Denial of service by a guest against the host** or against other guests — for example
  a guest that can wedge a physical core it does not own.

## What is out of scope right now

These are known gaps, already tracked in [`ROADMAP.md`](ROADMAP.md) rather than secrets:

- Anything that requires the bare-metal production path to be finished (Phase 6.7). Running
  the KVM-backed development VMM means you are relying on the **host kernel's** isolation,
  not Enlil's.
- Per-guest DMA isolation. The IOMMU (VT-d DMAR / AMD-Vi IVRS) is discovered and parsed;
  the VT-d DMA-remapping tables are now programmed (root + context tables, per-guest domains,
  translation enabled — ROADMAP 6.4, T-6.16), but interrupt remapping and the AMD-Vi side are
  still pending, so treat device passthrough as partially confined.
- Side channels inherent to sharing physical hardware (Spectre/Meltdown-class transient
  execution, cache and memory-bus contention). Mitigations are not yet implemented.
- Findings that require host root, physical access, or modifying the hypervisor binary —
  the host is inside the trust boundary.
- Reports generated solely by a scanner with no demonstrated impact.

If you are unsure whether something is in scope, report it anyway and say so.
