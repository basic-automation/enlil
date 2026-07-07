//! Guest-boot orchestration (item 3.12).
//!
//! [`StealthRunLoop`](crate::run_loop::StealthRunLoop) is the KVM run loop, but
//! wiring one up — build the standard PC, install the stealth stack, allocate and
//! map guest RAM, load the payload, create and prepare the boot vCPU — was only
//! ever done ad hoc inside tests, with no reusable entry point a top-level binary
//! could call. This module is that seam: [`GuestRuntime`] assembles a runnable
//! guest from a [`GuestBootSpec`] and owns its resources for its lifetime, so the
//! future orchestrator binary (and the config-driven USB-routing wiring, item
//! 4.3) has one place to boot a guest.
//!
//! Today it boots a **real-mode** payload; a protected/long-mode kernel loader
//! (bzImage via `linux-loader`) is a later slice.
//!
//! `target_os = "linux"`-only, like the run loop it drives.

#[cfg(target_os = "linux")]
pub use linux::{GuestBootSpec, GuestRuntime};

#[cfg(target_os = "linux")]
mod linux {
    use crate::device_bus::DeviceBus;
    use crate::error::Error;
    use crate::kvm_backend::{GuestRam, KvmBackend};
    use crate::run_loop::{LoopOutcome, StealthRunLoop};
    use crate::serial::SerialOutput;
    use crate::Result;
    use enlil_devices::stealth::lbr::LbrPlatform;
    use enlil_devices::tpm::VirtualTpm;

    /// What to boot: one guest's real-mode payload and the resources it runs in.
    pub struct GuestBootSpec {
        /// Guest name — also seeds the per-guest vTPM identity.
        pub name: String,
        /// Where the guest's COM1 output is routed.
        pub serial: SerialOutput,
        /// Guest RAM size in bytes.
        pub ram_bytes: usize,
        /// Guest-physical base the RAM is mapped at.
        pub load_base: u64,
        /// Real-mode entry point (guest-physical); the payload is loaded here and
        /// the boot vCPU starts executing at it. Must be within the mapped RAM.
        pub entry: u64,
        /// The code/data image loaded at `entry`.
        pub image: Vec<u8>,
        /// Seconds since the Unix epoch the RTC/CMOS calendar starts at.
        pub rtc_unix_secs: u64,
        /// Host virtualization vendor, for the LBR/PMC stealth model.
        pub platform: LbrPlatform,
    }

    /// A prepared, runnable guest. Owns the guest RAM and the stealth run loop; the
    /// RAM outlives the run loop (the KVM memory slot points into it) because the
    /// two are dropped in that order.
    pub struct GuestRuntime {
        run: StealthRunLoop,
        /// Backing store for the guest's RAM. Must be declared *after* `run` so it
        /// is dropped last — the KVM VM (inside `run`) referencing this memory must
        /// tear down before the buffer is freed.
        _ram: GuestRam,
    }

    impl GuestRuntime {
        /// Assemble a runnable single-vCPU real-mode guest from `spec`: build the
        /// standard PC with a **per-guest-seeded** vTPM (so each guest's TPM
        /// identity is distinct — LOCKED PRINCIPLE 1 transparency), install the
        /// stealth run loop, allocate and map `ram_bytes` of guest RAM at
        /// `load_base`, load `image` at the entry offset, then create the boot
        /// vCPU and point it at the real-mode entry.
        ///
        /// # Errors
        /// Returns [`Error::Config`] if `entry` is below `load_base` or the image
        /// does not fit within the mapped RAM, [`Error::HypervisorError`] if the
        /// standard PC cannot be built, or propagates the KVM setup errors from
        /// [`KvmBackend`], [`StealthRunLoop::install`], `map_memory`,
        /// `create_vcpu`, and `prepare_real_mode_vcpu`.
        pub fn prepare_real_mode(spec: GuestBootSpec) -> Result<Self> {
            let (mut run, ram, entry) = Self::assemble(spec)?;
            run.backend_mut().prepare_real_mode_vcpu(0, entry)?;
            Ok(Self { run, _ram: ram })
        }

        /// Like [`prepare_real_mode`](Self::prepare_real_mode) but starts the boot
        /// vCPU in flat 32-bit **protected mode** (paging off, flat 4 GiB
        /// segments). Real-mode payloads can only address the low 1 MiB and so
        /// cannot reach the platform's high MMIO apertures (LAPIC `0xFEE0_0000`,
        /// I/O APIC `0xFEC0_0000`, HPET `0xFED0_0000`); a protected-mode payload
        /// can, which is the mode a real kernel's early setup runs in. The payload
        /// is loaded at `entry` exactly as in real mode.
        ///
        /// # Errors
        /// As [`prepare_real_mode`](Self::prepare_real_mode), but propagating
        /// `prepare_protected_mode_vcpu`.
        pub fn prepare_protected_mode(spec: GuestBootSpec) -> Result<Self> {
            let (mut run, ram, entry) = Self::assemble(spec)?;
            run.backend_mut().prepare_protected_mode_vcpu(0, entry)?;
            Ok(Self { run, _ram: ram })
        }

        /// Shared setup for both boot modes: validate the spec, build the PC,
        /// install the run loop, allocate+load+map guest RAM, and create the boot
        /// vCPU. Returns the run loop, the RAM to keep alive, and the entry point;
        /// the caller applies the mode-specific `prepare_*_vcpu`.
        fn assemble(spec: GuestBootSpec) -> Result<(StealthRunLoop, GuestRam, u64)> {
            if spec.entry < spec.load_base {
                return Err(Error::Config(format!(
                    "entry {:#x} is below the load base {:#x}",
                    spec.entry, spec.load_base
                )));
            }
            let offset = (spec.entry - spec.load_base) as usize;
            let end = offset
                .checked_add(spec.image.len())
                .filter(|&e| e <= spec.ram_bytes)
                .ok_or_else(|| {
                    Error::Config(format!(
                        "image of {} bytes at entry offset {offset:#x} does not fit in \
                         {} bytes of guest RAM",
                        spec.image.len(),
                        spec.ram_bytes
                    ))
                })?;
            debug_assert!(end <= spec.ram_bytes);

            let pc = DeviceBus::standard_pc_complete_seeded(
                spec.serial,
                spec.rtc_unix_secs,
                1,
                Some(VirtualTpm::seed_for_guest(&spec.name)),
            )
            .map_err(|e| Error::HypervisorError(format!("build standard pc: {e}")))?;

            let backend = KvmBackend::new_without_irqchip()?;
            let mut run = StealthRunLoop::install(backend, pc, spec.platform)?;

            let mut ram = GuestRam::new(spec.ram_bytes);
            ram.as_mut_slice()[offset..end].copy_from_slice(&spec.image);
            let host_addr = ram.host_addr();
            // SAFETY: `ram` is returned to and owned by the GuestRuntime, dropped
            // after `run`, so the mapped host buffer outlives the KVM VM slot.
            unsafe {
                run.backend_mut()
                    .map_memory(spec.load_base, host_addr, ram.len() as u64)
            }?;
            run.create_vcpu(0)?;
            Ok((run, ram, spec.entry))
        }

        /// Run the boot vCPU until it halts, resets to `reset_entry`, or commits a
        /// sleep transition — or `max_entries` guest entries elapse.
        ///
        /// # Errors
        /// Propagates [`StealthRunLoop::run_real_mode`].
        pub fn run(&mut self, reset_entry: u64, max_entries: usize) -> Result<LoopOutcome> {
            self.run.run_real_mode(0, reset_entry, max_entries)
        }

        /// Borrow the underlying run loop, e.g. to apply CPUID stealth before the
        /// first run.
        pub const fn run_loop_mut(&mut self) -> &mut StealthRunLoop {
            &mut self.run
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::kvm_backend::is_kvm_available;
    use crate::run_loop::LoopOutcome;
    use crate::serial::{SerialOutput, SerialOutputMode};
    use enlil_devices::stealth::lbr::LbrPlatform;
    use std::sync::{Arc, Mutex};

    #[test]
    fn image_that_overflows_guest_ram_is_rejected_without_kvm() {
        // Pure validation — no KVM needed. Entry near the top of RAM with an image
        // that runs past the end must be rejected as a config error.
        let spec = GuestBootSpec {
            name: "overflow".into(),
            serial: SerialOutput::new("overflow", SerialOutputMode::Null),
            ram_bytes: 0x1000,
            load_base: 0x1000,
            entry: 0x1000 + 0xFF0,
            image: vec![0u8; 0x100],
            rtc_unix_secs: 0,
            platform: LbrPlatform::AmdSvm,
        };
        assert!(GuestRuntime::prepare_real_mode(spec).is_err());
    }

    #[test]
    fn entry_below_load_base_is_rejected_without_kvm() {
        let spec = GuestBootSpec {
            name: "below".into(),
            serial: SerialOutput::new("below", SerialOutputMode::Null),
            ram_bytes: 0x1000,
            load_base: 0x2000,
            entry: 0x1000,
            image: vec![0x90],
            rtc_unix_secs: 0,
            platform: LbrPlatform::AmdSvm,
        };
        assert!(GuestRuntime::prepare_real_mode(spec).is_err());
    }

    #[test]
    fn orchestrates_a_real_mode_guest_to_a_hlt() {
        if !is_kvm_available() {
            eprintln!("skipping orchestrates_a_real_mode_guest_to_a_hlt: no /dev/kvm");
            return;
        }
        // out 0x3F8,'K'; hlt — the guest greets over COM1 then halts.
        #[rustfmt::skip]
        let image: Vec<u8> = vec![
            0xB0, 0x4B,       // mov al, 'K'
            0xBA, 0xF8, 0x03, // mov dx, 0x3F8
            0xEE,             // out dx, al
            0xF4,             // hlt
        ];
        let sink = Arc::new(Mutex::new(Vec::new()));
        let spec = GuestBootSpec {
            name: "greeter".into(),
            serial: SerialOutput::new("greeter", SerialOutputMode::Shared(Arc::clone(&sink))),
            ram_bytes: 0x2000,
            load_base: 0x1000,
            entry: 0x1000,
            image,
            rtc_unix_secs: 0,
            platform: LbrPlatform::AmdSvm,
        };

        let mut guest = match GuestRuntime::prepare_real_mode(spec) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping orchestrates_a_real_mode_guest_to_a_hlt: {e}");
                return;
            }
        };
        let outcome = guest.run(0x1000, 100).expect("run guest");
        assert_eq!(outcome, LoopOutcome::Halted, "guest halts after greeting");
        assert_eq!(&*sink.lock().unwrap(), b"K", "guest emitted its greeting");
    }

    #[test]
    fn orchestrates_a_protected_mode_guest_reaching_high_mmio() {
        if !is_kvm_available() {
            eprintln!(
                "skipping orchestrates_a_protected_mode_guest_reaching_high_mmio: no /dev/kvm"
            );
            return;
        }
        // 32-bit protected-mode payload:
        //   mov eax, [0xFED00000]   ; read the HPET (high MMIO — real mode can't reach it)
        //   mov al, 'P'
        //   mov edx, 0x3F8
        //   out dx, al              ; greet only if the high read did not fault
        //   hlt
        #[rustfmt::skip]
        let image: Vec<u8> = vec![
            0xA1, 0x00, 0x00, 0xD0, 0xFE, // mov eax, [0xFED00000]
            0xB0, 0x50,                   // mov al, 'P'
            0xBA, 0xF8, 0x03, 0x00, 0x00, // mov edx, 0x3F8
            0xEE,                         // out dx, al
            0xF4,                         // hlt
        ];
        let sink = Arc::new(Mutex::new(Vec::new()));
        let spec = GuestBootSpec {
            name: "pm-greeter".into(),
            serial: SerialOutput::new("pm-greeter", SerialOutputMode::Shared(Arc::clone(&sink))),
            ram_bytes: 0x2000,
            load_base: 0x1000,
            entry: 0x1000,
            image,
            rtc_unix_secs: 0,
            platform: LbrPlatform::AmdSvm,
        };

        let mut guest = match GuestRuntime::prepare_protected_mode(spec) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping orchestrates_a_protected_mode_guest_reaching_high_mmio: {e}");
                return;
            }
        };
        let outcome = guest.run(0x1000, 100).expect("run guest");
        assert_eq!(
            outcome,
            LoopOutcome::Halted,
            "guest halts after the high read"
        );
        assert_eq!(
            &*sink.lock().unwrap(),
            b"P",
            "guest read high MMIO in protected mode and greeted"
        );
    }
}
