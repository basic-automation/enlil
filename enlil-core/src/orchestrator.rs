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
//! It boots a raw **real-mode**, flat **32-bit protected-mode**, or **64-bit
//! long-mode** payload, and boots a whole **Linux `bzImage`**:
//! [`load_bzimage`](GuestRuntime::load_bzimage) places the protected-mode kernel,
//! `boot_params`, cmdline, and optional initrd into guest RAM, and
//! [`boot_kernel`](GuestRuntime::boot_kernel) enters it per the 32-bit boot
//! protocol. [`run_first_guest`] does the whole config→boot path end to end.
//!
//! `target_os = "linux"`-only, like the run loop it drives.

#[cfg(target_os = "linux")]
pub use linux::{run_first_guest, GuestBootSpec, GuestRuntime, KernelBoot};

#[cfg(target_os = "linux")]
mod linux {
    use crate::bzimage::{build_boot_params, parse_bzimage_header, PROTECTED_MODE_LOAD_ADDR};
    use crate::device_bus::DeviceBus;
    use crate::error::Error;
    use crate::kvm_backend::{GuestRam, KvmBackend, KvmVcpuState};
    use crate::run_loop::{LoopOutcome, StealthRunLoop};
    use crate::serial::{SerialOutput, SerialOutputMode};
    use crate::Result;
    use enlil_config::{EnlilConfig, GuestConfig};
    use enlil_devices::acpi::facs::{FacsWaking, ResumeTarget, FACS_LENGTH};
    use enlil_devices::stealth::cpuid::{CpuidStealthConfig, CpuidStealthTable};
    use enlil_devices::stealth::lbr::LbrPlatform;
    use enlil_devices::tpm::VirtualTpm;

    /// What to boot: one guest's payload and the resources it runs in. The boot
    /// CPU mode is chosen by which `prepare_*` constructor is called.
    pub struct GuestBootSpec {
        /// Guest name — also seeds the per-guest vTPM identity.
        pub name: String,
        /// Where the guest's COM1 output is routed.
        pub serial: SerialOutput,
        /// Guest RAM size in bytes.
        pub ram_bytes: usize,
        /// Guest-physical base the RAM is mapped at.
        pub load_base: u64,
        /// Entry point (guest-physical); the payload is loaded here and the boot
        /// vCPU starts executing at it. Must be within the mapped RAM.
        pub entry: u64,
        /// The code/data image loaded at `entry`.
        pub image: Vec<u8>,
        /// Seconds since the Unix epoch the RTC/CMOS calendar starts at.
        pub rtc_unix_secs: u64,
        /// Host virtualization vendor, for the LBR/PMC stealth model.
        pub platform: LbrPlatform,
    }

    impl GuestBootSpec {
        /// Build a boot spec from a guest's [`GuestConfig`] plus the payload to run.
        ///
        /// Maps the config-derived fields: the guest name, its RAM
        /// (`memory_mb` → bytes), and its serial sink (from
        /// [`SerialOutputMode::from_output_spec`], or [`Null`](SerialOutputMode::Null)
        /// when the config disables the serial console). The caller supplies the
        /// boot payload and its placement (`load_base`, `entry`), the RTC epoch,
        /// and the host `platform` — deriving `image`/`entry` from `config.kernel`
        /// needs the bzImage loader, a later slice — so this is the config→spec
        /// half of the top-level orchestrator (item 3.12).
        #[must_use]
        pub fn from_guest_config(
            config: &GuestConfig,
            image: Vec<u8>,
            load_base: u64,
            entry: u64,
            rtc_unix_secs: u64,
            platform: LbrPlatform,
        ) -> Self {
            let mode = if config.serial.enabled {
                SerialOutputMode::from_output_spec(&config.serial.output)
            } else {
                SerialOutputMode::Null
            };
            Self {
                serial: SerialOutput::new(&config.name, mode),
                name: config.name.clone(),
                ram_bytes: (config.memory_mb as usize).saturating_mul(1024 * 1024),
                load_base,
                entry,
                image,
                rtc_unix_secs,
                platform,
            }
        }
    }

    /// Where [`GuestRuntime::load_bzimage`] placed a loaded kernel — the entry
    /// point and the `boot_params` block, which the (later) kernel-entry slice
    /// hands to the vCPU.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct KernelBoot {
        /// Guest-physical entry of the protected-mode kernel.
        pub kernel_entry: u64,
        /// Guest-physical address of the `boot_params` zero page.
        pub boot_params: u64,
    }

    /// A prepared, runnable guest. Owns the guest RAM and the stealth run loop; the
    /// RAM outlives the run loop (the KVM memory slot points into it) because the
    /// two are dropped in that order.
    pub struct GuestRuntime {
        run: StealthRunLoop,
        /// Guest-physical base the RAM is mapped at — used to translate a
        /// guest-physical address (e.g. the FACS) to an offset into `ram`.
        load_base: u64,
        /// Backing store for the guest's RAM. Must be declared *after* `run` so it
        /// is dropped last — the KVM VM (inside `run`) referencing this memory must
        /// tear down before the buffer is freed.
        ram: GuestRam,
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
            let (mut run, ram, entry, load_base) = Self::assemble(spec)?;
            run.backend_mut().prepare_real_mode_vcpu(0, entry)?;
            Ok(Self {
                run,
                load_base,
                ram,
            })
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
            let (mut run, ram, entry, load_base) = Self::assemble(spec)?;
            run.backend_mut().prepare_protected_mode_vcpu(0, entry)?;
            Ok(Self {
                run,
                load_base,
                ram,
            })
        }

        /// Like [`prepare_real_mode`](Self::prepare_real_mode) but starts the boot
        /// vCPU in 64-bit **long mode** with paging on. This writes a minimal
        /// identity-mapping page-table tree into guest RAM (a PML4 → PDPT → PD
        /// chain with a single 2 MiB page mapping `[0, 2 MiB)`) at fixed low
        /// addresses and enters long mode at it — the mode a real x86-64 kernel
        /// and a 64-bit ACPI S3 resume trampoline run in.
        ///
        /// Requires `load_base == 0` (so guest-physical addresses equal RAM
        /// offsets and line up with the identity map). The three page tables
        /// (built by [`enlil_platform::memory::paging::build_identity_map_2mib`])
        /// occupy `[0x4000, 0x7000)`, so the payload must not reach `0x4000`, and
        /// the RAM must be at least that large.
        ///
        /// # Errors
        /// Returns [`Error::Config`] if `load_base != 0`, the payload overlaps the
        /// page-table region, or the RAM is too small; otherwise as
        /// [`prepare_real_mode`](Self::prepare_real_mode) but propagating
        /// `prepare_long_mode_vcpu`.
        pub fn prepare_long_mode(spec: GuestBootSpec) -> Result<Self> {
            use enlil_platform::memory::paging::{self, flags};

            // Page tables live just above the payload's low region; the builder
            // lays PML4 → PDPT → PD contiguously from this base.
            const TABLES_GPA: u64 = 0x4000;

            if spec.load_base != 0 {
                return Err(Error::Config(format!(
                    "long-mode boot requires load_base 0, got {:#x}",
                    spec.load_base
                )));
            }
            let image_end = spec.entry as usize + spec.image.len();
            if image_end > TABLES_GPA as usize {
                return Err(Error::Config(format!(
                    "long-mode payload ends at {image_end:#x}, overlapping the identity \
                     page tables at {TABLES_GPA:#x}"
                )));
            }

            let (mut run, mut ram, entry, load_base) = Self::assemble(spec)?;
            // Identity-map the low 2 MiB (covering the payload and the tables
            // themselves) with the reusable page-table builder (item 1.3).
            let layout = paging::build_identity_map_2mib(
                ram.as_mut_slice(),
                TABLES_GPA,
                2 * 1024 * 1024,
                flags::WRITABLE,
            )
            .map_err(|e| {
                Error::Config(format!("long-mode boot could not build page tables: {e:?}"))
            })?;
            run.backend_mut()
                .prepare_long_mode_vcpu(0, entry, layout.cr3)?;
            Ok(Self {
                run,
                load_base,
                ram,
            })
        }

        /// Shared setup for both boot modes: validate the spec, build the PC,
        /// install the run loop, allocate+load+map guest RAM, and create the boot
        /// vCPU. Returns the run loop, the RAM to keep alive, and the entry point;
        /// the caller applies the mode-specific `prepare_*_vcpu`.
        fn assemble(spec: GuestBootSpec) -> Result<(StealthRunLoop, GuestRam, u64, u64)> {
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

            // Apply CPUID stealth before the guest runs (LOCKED PRINCIPLE 1 —
            // transparency): clear the CPUID.1:ECX[31] hypervisor-present bit,
            // present the guest's own single-vCPU topology (not the host's
            // logical-processor count), and a PMU leaf consistent with the RDPMC
            // shadow — so a guest that reads CPUID cannot trivially detect the
            // hypervisor. The table takes the physical machine's identity via
            // from_host.
            let table = CpuidStealthTable::build(&CpuidStealthConfig::from_host(1, 1));
            run.apply_topology_stealth(&table)?;
            run.apply_pmu_stealth(&table)?;

            Ok((run, ram, spec.entry, spec.load_base))
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

        /// Snapshot the boot vCPU's architectural state (the item 2.3 primitive) —
        /// the volatile state a suspend must preserve. The guest RAM stays live in
        /// the KVM memory slot this runtime owns, so a snapshot plus that RAM is a
        /// complete resume point (groundwork for S3 suspend/resume, item 5.7, and
        /// hibernation, item 8.4).
        ///
        /// # Errors
        /// Propagates [`KvmBackend::save_vcpu_state`].
        pub fn snapshot_vcpu(&self) -> Result<KvmVcpuState> {
            self.run.backend().save_vcpu_state(0)
        }

        /// Restore the boot vCPU from a [`snapshot_vcpu`](Self::snapshot_vcpu)
        /// capture, rewinding it to exactly the point it was taken.
        ///
        /// # Errors
        /// Propagates [`KvmBackend::restore_vcpu_state`].
        pub fn restore_vcpu(&mut self, state: &KvmVcpuState) -> Result<()> {
            self.run.backend_mut().restore_vcpu_state(0, state)
        }

        /// Translate a guest-physical address to an offset into the owned RAM,
        /// bounds-checked so `[gpa, gpa+len)` lies within the mapped guest RAM.
        fn ram_range(&self, gpa: u64, len: usize) -> Result<std::ops::Range<usize>> {
            let start = gpa
                .checked_sub(self.load_base)
                .and_then(|o| usize::try_from(o).ok())
                .ok_or_else(|| Error::Config(format!("gpa {gpa:#x} is below the load base")))?;
            let end = start
                .checked_add(len)
                .filter(|&e| e <= self.ram.len())
                .ok_or_else(|| {
                    Error::Config(format!("gpa {gpa:#x}+{len:#x} overruns guest RAM"))
                })?;
            Ok(start..end)
        }

        /// Write `bytes` into guest RAM at guest-physical `gpa` — e.g. to load a
        /// kernel, initrd, or boot parameters before running. Bounds-checked.
        ///
        /// # Errors
        /// Returns [`Error::Config`] if `[gpa, gpa+bytes.len())` is outside the
        /// mapped guest RAM.
        pub fn write_guest_bytes(&mut self, gpa: u64, bytes: &[u8]) -> Result<()> {
            let range = self.ram_range(gpa, bytes.len())?;
            self.ram.as_mut_slice()[range].copy_from_slice(bytes);
            Ok(())
        }

        /// Read `len` bytes of guest RAM at guest-physical `gpa`. Bounds-checked.
        ///
        /// # Errors
        /// Returns [`Error::Config`] if `[gpa, gpa+len)` is outside the mapped
        /// guest RAM.
        pub fn read_guest_bytes(&self, gpa: u64, len: usize) -> Result<Vec<u8>> {
            let range = self.ram_range(gpa, len)?;
            Ok(self.ram.as_slice()[range].to_vec())
        }

        /// Load a Linux `bzImage` into guest RAM per the boot protocol (items
        /// 3.12 / 5.6): place the `boot_params` zero page and the NUL-terminated
        /// `cmdline` at fixed low addresses and the protected-mode kernel at
        /// [`PROTECTED_MODE_LOAD_ADDR`] (1 MiB), and return the [`KernelBoot`]
        /// placement. The `boot_params` E820 map advertises the guest's RAM as one
        /// usable region.
        ///
        /// This does the *placement*; entering the kernel with the boot-protocol
        /// register state (`RSI` → `boot_params`, protected mode at the entry) is
        /// a later slice. Requires `load_base == 0`.
        ///
        /// # Errors
        /// Returns [`Error::Config`] if `load_base != 0`, `image` is not a
        /// boot-protocol `bzImage`, or anything does not fit in guest RAM.
        pub fn load_bzimage(
            &mut self,
            image: &[u8],
            cmdline: &str,
            initrd: Option<&[u8]>,
        ) -> Result<KernelBoot> {
            /// Where the `boot_params` zero page is placed (64 KiB).
            const BOOT_PARAMS_GPA: u64 = 0x1_0000;
            /// Where the kernel command line is placed (128 KiB).
            const CMDLINE_GPA: u64 = 0x2_0000;

            if self.load_base != 0 {
                return Err(Error::Config("bzImage loading requires load_base 0".into()));
            }
            let info = parse_bzimage_header(image)
                .ok_or_else(|| Error::Config("not a boot-protocol bzImage".into()))?;
            if !info.is_loaded_high() {
                return Err(Error::Config(
                    "kernel is not loaded-high (a legacy zImage); only bzImage is supported".into(),
                ));
            }
            let kernel = image
                .get(info.protected_mode_kernel_offset..)
                .ok_or_else(|| {
                    Error::Config("bzImage shorter than its setup header claims".into())
                })?;

            // Place the initrd high in RAM (page-aligned), if any, and record
            // where — the kernel reads ramdisk_image/size from boot_params.
            let ram_len = self.ram.len() as u64;
            let kernel_end = PROTECTED_MODE_LOAD_ADDR + kernel.len() as u64;
            let (ramdisk_image, ramdisk_size) = match initrd {
                Some(initrd) if !initrd.is_empty() => {
                    let addr = (ram_len.saturating_sub(initrd.len() as u64)) & !0xFFF;
                    if addr < kernel_end {
                        return Err(Error::Config(
                            "initrd does not fit above the kernel in guest RAM".into(),
                        ));
                    }
                    self.write_guest_bytes(addr, initrd)?;
                    (
                        u32::try_from(addr)
                            .map_err(|_| Error::Config("initrd address exceeds 4 GiB".into()))?,
                        u32::try_from(initrd.len())
                            .map_err(|_| Error::Config("initrd larger than 4 GiB".into()))?,
                    )
                }
                _ => (0, 0),
            };

            // A standard-PC E820 map: low RAM below the EBDA, the EBDA + BIOS/VGA
            // hole [0x9FC00, 0x100000) reserved, and high RAM from 1 MiB up. The
            // kernel loads at 1 MiB, so `load_bzimage` always has >= 1 MiB of RAM.
            const LOW_RAM_END: u64 = 0x9_FC00;
            let e820 = [
                (0, LOW_RAM_END, 1),                                      // low usable
                (LOW_RAM_END, PROTECTED_MODE_LOAD_ADDR - LOW_RAM_END, 2), // EBDA + BIOS hole
                (
                    PROTECTED_MODE_LOAD_ADDR,
                    ram_len - PROTECTED_MODE_LOAD_ADDR,
                    1,
                ), // high usable
            ];
            let cmdline_ptr = u32::try_from(CMDLINE_GPA).expect("CMDLINE_GPA fits u32");
            let boot_params =
                build_boot_params(image, cmdline_ptr, ramdisk_image, ramdisk_size, &e820)
                    .ok_or_else(|| Error::Config("bzImage too short for its boot_params".into()))?;

            self.write_guest_bytes(BOOT_PARAMS_GPA, &boot_params)?;
            self.write_guest_bytes(CMDLINE_GPA, cmdline.as_bytes())?;
            self.write_guest_bytes(CMDLINE_GPA + cmdline.len() as u64, &[0])?; // NUL-terminate
            self.write_guest_bytes(PROTECTED_MODE_LOAD_ADDR, kernel)?;

            Ok(KernelBoot {
                kernel_entry: PROTECTED_MODE_LOAD_ADDR,
                boot_params: BOOT_PARAMS_GPA,
            })
        }

        /// Point the boot vCPU at a kernel [`load_bzimage`](Self::load_bzimage)
        /// placed, per the Linux 32-bit boot protocol: flat protected mode at the
        /// kernel entry with `RSI` → the `boot_params`. After this, [`run`](Self::run)
        /// executes the kernel. This is the entry counterpart to `load_bzimage`.
        ///
        /// # Errors
        /// Propagates [`KvmBackend::prepare_linux_boot_vcpu`].
        pub fn boot_kernel(&mut self, boot: &KernelBoot) -> Result<()> {
            self.run
                .backend_mut()
                .prepare_linux_boot_vcpu(0, boot.kernel_entry, boot.boot_params)
        }

        /// Enter a loaded kernel via its **64-bit** entry point
        /// (`kernel_entry + 0x200`) in long mode with `RSI` → the `boot_params` —
        /// the 64-bit Linux boot protocol modern kernels prefer. Installs a minimal
        /// identity map (`[0, 2 MiB)`) in guest RAM and enters long mode at the
        /// 64-bit entry. Requires `load_base == 0`.
        ///
        /// # Errors
        /// Returns [`Error::Config`] if `load_base != 0`; otherwise propagates the
        /// page-table write and [`KvmBackend::prepare_linux_boot_vcpu_64`].
        pub fn boot_kernel_64(&mut self, boot: &KernelBoot) -> Result<()> {
            /// Offset of the 64-bit entry point from the protected-mode kernel base
            /// (Linux boot protocol).
            const ENTRY_64_OFFSET: u64 = 0x200;
            if self.load_base != 0 {
                return Err(Error::Config(
                    "64-bit kernel entry requires load_base 0".into(),
                ));
            }
            let pml4 = self.install_identity_page_tables()?;
            self.run.backend_mut().prepare_linux_boot_vcpu_64(
                0,
                boot.kernel_entry + ENTRY_64_OFFSET,
                boot.boot_params,
                pml4,
            )
        }

        /// Write a minimal identity-mapping page-table tree — PML4 → PDPT → PD with
        /// a single 2 MiB page covering `[0, 2 MiB)` — into guest RAM at fixed low
        /// addresses (`0x4000`/`0x5000`/`0x6000`), returning the PML4 gpa. The
        /// long-mode kernel-entry path uses it. Assumes `load_base == 0`.
        fn install_identity_page_tables(&mut self) -> Result<u64> {
            const PML4_GPA: u64 = 0x4000;
            const PDPT_GPA: u64 = 0x5000;
            const PD_GPA: u64 = 0x6000;
            self.write_guest_bytes(PML4_GPA, &(PDPT_GPA | 0x3).to_le_bytes())?; // present|write
            self.write_guest_bytes(PDPT_GPA, &(PD_GPA | 0x3).to_le_bytes())?;
            self.write_guest_bytes(PD_GPA, &0x83u64.to_le_bytes())?; // present|write|PS
            Ok(PML4_GPA)
        }

        /// Resume the guest from an ACPI S3 (suspend-to-RAM) transition: read the
        /// FACS (the OS wrote its firmware waking vector there before suspending)
        /// from guest RAM at `facs_gpa` — the address the FADT's `FIRMWARE_CTRL`
        /// points at — decode the waking vector, and prepare the boot vCPU to
        /// re-enter at it (item 5.7). Returns the [`ResumeTarget`] that was armed.
        ///
        /// The guest's RAM is untouched (S3 preserves it), so the OS's own
        /// resume trampoline at the waking vector restores the rest of its state.
        /// A real-mode waking vector (the common case) re-enters in real mode; a
        /// 64-bit waking vector needs a long-mode entry, which the vCPU-prepare
        /// helpers do not offer yet, so it is rejected rather than entered wrong.
        ///
        /// # Errors
        /// Returns [`Error::Config`] if `facs_gpa` is outside guest RAM, the FACS
        /// is malformed, no waking vector is armed, or the armed vector needs an
        /// unsupported long-mode entry; propagates `prepare_real_mode_vcpu`.
        pub fn resume_from_s3(&mut self, facs_gpa: u64) -> Result<ResumeTarget> {
            let facs = self.read_guest_bytes(facs_gpa, FACS_LENGTH as usize)?;
            let waking = FacsWaking::from_facs(&facs)
                .ok_or_else(|| Error::Config("FACS buffer too short".into()))?;
            let target = waking.resume_target();
            match target {
                ResumeTarget::RealMode(vector) => {
                    self.run
                        .backend_mut()
                        .prepare_real_mode_vcpu(0, u64::from(vector))?;
                    Ok(target)
                }
                ResumeTarget::Extended(vector) => Err(Error::Config(format!(
                    "S3 X waking vector {vector:#x} needs a long-mode resume entry, \
                     which is not implemented yet"
                ))),
                ResumeTarget::None => Err(Error::Config(
                    "no S3 waking vector armed in the FACS".into(),
                )),
            }
        }

        /// Resume from S3 by first *discovering* the FACS from the guest's live
        /// ACPI tables — walk RSDP → XSDT/RSDT → FADT → FACS starting at
        /// `rsdp_gpa` — then [`resume_from_s3`](Self::resume_from_s3). This is the
        /// realistic entry point: the guest OS placed its tables in RAM, so the
        /// FACS address is only knowable by walking them (item 5.7).
        ///
        /// Requires `load_base == 0` (the ACPI walker indexes guest RAM by
        /// guest-physical address).
        ///
        /// # Errors
        /// Returns [`Error::Config`] if `load_base != 0` or the FACS cannot be
        /// discovered; otherwise as [`resume_from_s3`](Self::resume_from_s3).
        pub fn resume_from_s3_via_rsdp(&mut self, rsdp_gpa: u64) -> Result<ResumeTarget> {
            if self.load_base != 0 {
                return Err(Error::Config(
                    "ACPI-table discovery requires load_base 0".into(),
                ));
            }
            let facs_gpa =
                enlil_devices::acpi::discover::find_facs_address(self.ram.as_slice(), rsdp_gpa)
                    .ok_or_else(|| {
                        Error::Config(
                            "could not discover the FACS from the guest's ACPI tables".into(),
                        )
                    })?;
            self.resume_from_s3(facs_gpa)
        }
    }

    /// Boot the first guest defined in `config` from a Linux `bzImage`, running it
    /// until it halts / resets / sleeps or `max_entries` guest entries elapse.
    ///
    /// The end-to-end orchestration a top-level binary drives (item 3.12): map the
    /// guest's config to a [`GuestBootSpec`] (RAM from `memory_mb`, serial sink,
    /// name-seeded vTPM; boot base at guest-physical 0, host-detected stealth
    /// platform), assemble the [`GuestRuntime`], [`load_bzimage`](GuestRuntime::load_bzimage)
    /// the kernel with `cmdline` and optional `initrd`,
    /// [`boot_kernel`](GuestRuntime::boot_kernel), and [`run`](GuestRuntime::run).
    ///
    /// # Errors
    /// Returns [`Error::Config`] if `config` has no guests; otherwise propagates
    /// the [`GuestRuntime`] preparation, load, and run errors.
    pub fn run_first_guest(
        config: &EnlilConfig,
        kernel: &[u8],
        initrd: Option<&[u8]>,
        cmdline: &str,
        max_entries: usize,
    ) -> Result<LoopOutcome> {
        let guest = config
            .guest
            .values()
            .next()
            .ok_or_else(|| Error::Config("configuration has no guests to boot".into()))?;
        // The boot payload is unused (the kernel is placed at 1 MiB by
        // load_bzimage); a single hlt is a harmless placeholder at entry 0.
        let spec = GuestBootSpec::from_guest_config(
            guest,
            vec![0xF4],
            0,
            0,
            0,
            LbrPlatform::detect_host(),
        );
        let mut runtime = GuestRuntime::prepare_real_mode(spec)?;
        let boot = runtime.load_bzimage(kernel, cmdline, initrd)?;
        runtime.boot_kernel(&boot)?;
        runtime.run(0, max_entries)
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
    fn boot_spec_maps_a_guest_config_without_kvm() {
        use enlil_config::{GuestConfig, SchedulingMode, SerialPortConfig};

        let config = GuestConfig {
            name: "win11".into(),
            cpus: vec![0, 1],
            memory_mb: 512,
            kernel: None,
            initrd: None,
            cmdline: "console=ttyS0".into(),
            scheduling: SchedulingMode::Auto,
            disks: vec![],
            serial: SerialPortConfig::default(),
            mac: None,
        };
        let spec = GuestBootSpec::from_guest_config(
            &config,
            vec![0xF4], // hlt
            0x1000,
            0x1000,
            42,
            LbrPlatform::AmdSvm,
        );
        assert_eq!(spec.name, "win11");
        assert_eq!(spec.ram_bytes, 512 * 1024 * 1024, "memory_mb → bytes");
        assert_eq!(spec.load_base, 0x1000);
        assert_eq!(spec.entry, 0x1000);
        assert_eq!(spec.rtc_unix_secs, 42);
        assert_eq!(spec.image, vec![0xF4]);
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
    fn resume_from_s3_discovers_the_facs_via_the_acpi_tables() {
        use enlil_devices::acpi::facs::{FacsBuilder, ResumeTarget};
        use enlil_devices::acpi::fadt::FadtBuilder;
        use enlil_devices::acpi::rsdp::RsdpBuilder;
        use enlil_devices::acpi::xsdt::XsdtBuilder;

        if !is_kvm_available() {
            eprintln!(
                "skipping resume_from_s3_discovers_the_facs_via_the_acpi_tables: no /dev/kvm"
            );
            return;
        }
        // A full ACPI table set laid out in guest RAM (based at gpa 0):
        //   0x0800: resume payload — out 0x3F8,'W'; hlt
        //   0x1000: FACS, waking vector = 0x0800
        //   0x2000: FADT, FIRMWARE_CTRL = 0x1000
        //   0x4000: XSDT listing the FADT
        //   0x5000: RSDP pointing at the XSDT
        let mut image = vec![0u8; 0x5100];
        #[rustfmt::skip]
        let wake: [u8; 7] = [0xB0, 0x57, 0xBA, 0xF8, 0x03, 0xEE, 0xF4]; // mov al,'W'; mov dx,0x3F8; out; hlt
        image[0x800..0x800 + wake.len()].copy_from_slice(&wake);
        let mut facs = FacsBuilder::new().build();
        facs[12..16].copy_from_slice(&0x800u32.to_le_bytes());
        image[0x1000..0x1000 + facs.len()].copy_from_slice(&facs);
        let fadt = FadtBuilder::new(0x3000).firmware_ctrl(0x1000).build();
        image[0x2000..0x2000 + fadt.len()].copy_from_slice(&fadt);
        let xsdt = XsdtBuilder::new().add_table(0x2000).build();
        image[0x4000..0x4000 + xsdt.len()].copy_from_slice(&xsdt);
        let rsdp = RsdpBuilder::new().xsdt_address(0x4000).build();
        image[0x5000..0x5000 + rsdp.len()].copy_from_slice(&rsdp);

        let sink = Arc::new(Mutex::new(Vec::new()));
        let spec = GuestBootSpec {
            name: "s3-discover".into(),
            serial: SerialOutput::new("s3-discover", SerialOutputMode::Shared(Arc::clone(&sink))),
            ram_bytes: 0x1_0000,
            load_base: 0,
            entry: 0,
            image,
            rtc_unix_secs: 0,
            platform: LbrPlatform::AmdSvm,
        };
        let mut guest = match GuestRuntime::prepare_real_mode(spec) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping resume_from_s3_discovers_the_facs_via_the_acpi_tables: {e}");
                return;
            }
        };

        // Discover the FACS from the RSDP and resume into the waking vector.
        let target = guest
            .resume_from_s3_via_rsdp(0x5000)
            .expect("discover FACS and resume");
        assert_eq!(target, ResumeTarget::RealMode(0x800));
        assert_eq!(guest.run(0, 100).unwrap(), LoopOutcome::Halted);
        assert_eq!(
            &*sink.lock().unwrap(),
            b"W",
            "resumed at the FACS waking vector discovered by walking the ACPI tables"
        );
    }

    #[test]
    fn orchestrates_a_long_mode_guest() {
        if !is_kvm_available() {
            eprintln!("skipping orchestrates_a_long_mode_guest: no /dev/kvm");
            return;
        }
        // 64-bit payload: mov al,'L'; mov edx,0x3F8; out dx,al; hlt. If it runs to
        // hlt and emits 'L', long-mode entry + the identity page tables worked.
        #[rustfmt::skip]
        let image: Vec<u8> = vec![
            0xB0, 0x4C,                   // mov al, 'L'
            0xBA, 0xF8, 0x03, 0x00, 0x00, // mov edx, 0x3F8
            0xEE,                         // out dx, al
            0xF4,                         // hlt
        ];
        let sink = Arc::new(Mutex::new(Vec::new()));
        let spec = GuestBootSpec {
            name: "long".into(),
            serial: SerialOutput::new("long", SerialOutputMode::Shared(Arc::clone(&sink))),
            ram_bytes: 0x1_0000,
            load_base: 0,
            entry: 0x1000,
            image,
            rtc_unix_secs: 0,
            platform: LbrPlatform::AmdSvm,
        };
        let mut guest = match GuestRuntime::prepare_long_mode(spec) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping orchestrates_a_long_mode_guest: {e}");
                return;
            }
        };
        assert_eq!(guest.run(0x1000, 100).unwrap(), LoopOutcome::Halted);
        assert_eq!(
            &*sink.lock().unwrap(),
            b"L",
            "guest executed in 64-bit long mode through the identity page tables"
        );
    }

    #[test]
    fn long_mode_rejects_a_nonzero_load_base_without_kvm() {
        let spec = GuestBootSpec {
            name: "long-badbase".into(),
            serial: SerialOutput::new("long-badbase", SerialOutputMode::Null),
            ram_bytes: 0x1_0000,
            load_base: 0x1000, // must be 0 for the identity map
            entry: 0x1000,
            image: vec![0xF4],
            rtc_unix_secs: 0,
            platform: LbrPlatform::AmdSvm,
        };
        assert!(GuestRuntime::prepare_long_mode(spec).is_err());
    }

    #[test]
    fn run_first_guest_boots_a_bzimage_kernel_to_halt() {
        use enlil_config::{
            EnlilConfig, GuestConfig, HypervisorConfig, SchedulingMode, SerialPortConfig, UsbConfig,
        };
        use std::collections::HashMap;

        if !is_kvm_available() {
            eprintln!("skipping run_first_guest_boots_a_bzimage_kernel_to_halt: no /dev/kvm");
            return;
        }
        // A fake bzImage whose protected-mode kernel is a single hlt.
        let mut image = vec![0u8; 0x401];
        image[0x1F1] = 1; // setup_sects → kernel at (1+1)*512 = 0x400
        image[0x1FE..0x200].copy_from_slice(&0xAA55u16.to_le_bytes());
        image[0x202..0x206].copy_from_slice(b"HdrS");
        image[0x206..0x208].copy_from_slice(&0x020Fu16.to_le_bytes());
        image[0x211] = 0x01; // loadflags: LOADED_HIGH (a real bzImage)
        image[0x400] = 0xF4; // hlt

        let mut guests = HashMap::new();
        guests.insert(
            "vm1".into(),
            GuestConfig {
                name: "vm1".into(),
                cpus: vec![0],
                memory_mb: 2, // 2 MiB — room for the kernel at 1 MiB
                kernel: None,
                initrd: None,
                cmdline: "console=ttyS0".into(),
                scheduling: SchedulingMode::Auto,
                disks: vec![],
                serial: SerialPortConfig::default(),
                mac: None,
            },
        );
        let config = EnlilConfig {
            hypervisor: HypervisorConfig::default(),
            guest: guests,
            usb: UsbConfig::default(),
        };

        match run_first_guest(&config, &image, None, "console=ttyS0", 100) {
            Ok(outcome) => assert_eq!(
                outcome,
                LoopOutcome::Halted,
                "the config-driven kernel booted and halted"
            ),
            // KVM present but VM setup unavailable (e.g. capability) → honest skip.
            Err(e) => eprintln!("skipping run_first_guest_boots_a_bzimage_kernel_to_halt: {e}"),
        }
    }

    #[test]
    fn load_bzimage_places_kernel_boot_params_and_cmdline() {
        if !is_kvm_available() {
            eprintln!("skipping load_bzimage_places_kernel_boot_params_and_cmdline: no /dev/kvm");
            return;
        }
        // A fake bzImage: a valid setup header with setup_sects = 1 (so the
        // protected-mode kernel starts at (1+1)*512 = 0x400), followed by a
        // recognizable "kernel" body.
        let mut image = vec![0u8; 0x410];
        image[0x1F1] = 1; // setup_sects
        image[0x1FE..0x200].copy_from_slice(&0xAA55u16.to_le_bytes());
        image[0x202..0x206].copy_from_slice(b"HdrS");
        image[0x206..0x208].copy_from_slice(&0x020Fu16.to_le_bytes());
        image[0x211] = 0x01; // loadflags: LOADED_HIGH (a real bzImage)
        let kernel_body: [u8; 16] = [
            0xDE, 0xAD, 0xBE, 0xEF, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
        ];
        image[0x400..0x410].copy_from_slice(&kernel_body);

        let spec = GuestBootSpec {
            name: "kload".into(),
            serial: SerialOutput::new("kload", SerialOutputMode::Null),
            ram_bytes: 0x11_0000, // > 1 MiB so the kernel fits at 0x100000
            load_base: 0,
            entry: 0,
            image: vec![0xF4], // boot payload unused; we only load the kernel
            rtc_unix_secs: 0,
            platform: LbrPlatform::AmdSvm,
        };
        let mut guest = match GuestRuntime::prepare_real_mode(spec) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping load_bzimage_places_kernel_boot_params_and_cmdline: {e}");
                return;
            }
        };

        // Include a small initrd to exercise the ramdisk placement.
        let initrd = [0x11u8, 0x22, 0x33, 0x44];
        let boot = guest
            .load_bzimage(&image, "console=ttyS0", Some(&initrd))
            .expect("load bzImage");
        assert_eq!(boot.kernel_entry, 0x10_0000);
        assert_eq!(boot.boot_params, 0x1_0000);

        // The protected-mode kernel body landed at 1 MiB.
        assert_eq!(
            guest
                .read_guest_bytes(0x10_0000, kernel_body.len())
                .unwrap(),
            kernel_body
        );
        // The cmdline landed (NUL-terminated) at 0x20000.
        assert_eq!(
            guest.read_guest_bytes(0x2_0000, 13).unwrap(),
            b"console=ttyS0"
        );
        assert_eq!(guest.read_guest_bytes(0x2_0000 + 13, 1).unwrap(), vec![0]);
        // boot_params carries the copied setup header + the cmdline pointer.
        let bp = guest.read_guest_bytes(0x1_0000, 0x1000).unwrap();
        assert_eq!(bp[0x1F1], 1, "setup_sects copied into boot_params");
        assert_eq!(
            u32::from_le_bytes(bp[0x228..0x22C].try_into().unwrap()),
            0x2_0000,
            "cmd_line_ptr points at the cmdline"
        );
        // A standard-PC E820: low usable, reserved BIOS hole, high usable.
        assert_eq!(bp[0x1E8], 3, "three E820 entries");
        assert_eq!(u64::from_le_bytes(bp[0x2D0..0x2D8].try_into().unwrap()), 0);
        assert_eq!(
            u64::from_le_bytes(bp[0x2D8..0x2E0].try_into().unwrap()),
            0x9_FC00,
            "low usable RAM below the EBDA"
        );
        assert_eq!(u32::from_le_bytes(bp[0x2E0..0x2E4].try_into().unwrap()), 1);
        // Second entry (at 0x2D0 + 20 = 0x2E4): the EBDA + BIOS/VGA hole, reserved.
        assert_eq!(
            u64::from_le_bytes(bp[0x2E4..0x2EC].try_into().unwrap()),
            0x9_FC00,
            "BIOS hole starts at 0x9FC00"
        );
        assert_eq!(
            u32::from_le_bytes(bp[0x2F4..0x2F8].try_into().unwrap()),
            2,
            "BIOS hole is reserved"
        );
        // The initrd was placed high in RAM and recorded in boot_params.
        let ramdisk_image = u32::from_le_bytes(bp[0x218..0x21C].try_into().unwrap());
        let ramdisk_size = u32::from_le_bytes(bp[0x21C..0x220].try_into().unwrap());
        assert_eq!(ramdisk_size, 4, "ramdisk_size records the initrd length");
        assert!(
            u64::from(ramdisk_image) >= 0x10_0000,
            "initrd placed above the kernel"
        );
        assert_eq!(
            guest.read_guest_bytes(u64::from(ramdisk_image), 4).unwrap(),
            initrd,
            "initrd bytes landed at ramdisk_image"
        );

        // boot_kernel points the vCPU at the kernel per the boot protocol:
        // protected mode at the entry, RSI → boot_params. Verify via a snapshot
        // (no real kernel needed to check the register state).
        guest.boot_kernel(&boot).expect("prepare kernel entry");
        let snap = guest.snapshot_vcpu().expect("snapshot after boot_kernel");
        assert_eq!(snap.regs.rip, 0x10_0000, "rip at the kernel entry");
        assert_eq!(snap.regs.rsi, 0x1_0000, "rsi points at boot_params");
        assert_ne!(snap.sregs.cr0 & 1, 0, "protected mode (CR0.PE) enabled");

        // The 64-bit entry path: long mode at kernel_entry + 0x200, RSI preserved.
        guest
            .boot_kernel_64(&boot)
            .expect("prepare 64-bit kernel entry");
        let snap64 = guest
            .snapshot_vcpu()
            .expect("snapshot after boot_kernel_64");
        assert_eq!(snap64.regs.rip, 0x10_0200, "rip at the 64-bit entry");
        assert_eq!(snap64.regs.rsi, 0x1_0000, "rsi points at boot_params");
        assert_ne!(snap64.sregs.cr0 & (1 << 31), 0, "paging (CR0.PG) enabled");
        assert_ne!(
            snap64.sregs.efer & (1 << 10),
            0,
            "long mode active (EFER.LMA)"
        );
    }

    #[test]
    fn guest_ram_bytes_round_trip_and_bounds_check() {
        if !is_kvm_available() {
            eprintln!("skipping guest_ram_bytes_round_trip_and_bounds_check: no /dev/kvm");
            return;
        }
        let spec = GuestBootSpec {
            name: "ramio".into(),
            serial: SerialOutput::new("ramio", SerialOutputMode::Null),
            ram_bytes: 0x2000,
            load_base: 0x1000,
            entry: 0x1000,
            image: vec![0xF4], // hlt
            rtc_unix_secs: 0,
            platform: LbrPlatform::AmdSvm,
        };
        let mut guest = match GuestRuntime::prepare_real_mode(spec) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping guest_ram_bytes_round_trip_and_bounds_check: {e}");
                return;
            }
        };
        // Write near the top of RAM (gpa 0x1000..0x3000) and read it back.
        guest
            .write_guest_bytes(0x2500, &[0xDE, 0xAD, 0xBE, 0xEF])
            .expect("write within RAM");
        assert_eq!(
            guest.read_guest_bytes(0x2500, 4).unwrap(),
            vec![0xDE, 0xAD, 0xBE, 0xEF]
        );
        // Below the load base and past the end are rejected.
        assert!(
            guest.write_guest_bytes(0x0, &[0]).is_err(),
            "below load base"
        );
        assert!(
            guest.read_guest_bytes(0x2FFE, 4).is_err(),
            "read overruns the end of RAM"
        );
    }

    #[test]
    fn snapshot_and_restore_rewinds_the_boot_vcpu() {
        if !is_kvm_available() {
            eprintln!("skipping snapshot_and_restore_rewinds_the_boot_vcpu: no /dev/kvm");
            return;
        }
        // out 0x3F8,'A'; hlt — greets once per run from the entry point.
        #[rustfmt::skip]
        let image: Vec<u8> = vec![
            0xB0, 0x41,       // mov al, 'A'
            0xBA, 0xF8, 0x03, // mov dx, 0x3F8
            0xEE,             // out dx, al
            0xF4,             // hlt
        ];
        let sink = Arc::new(Mutex::new(Vec::new()));
        let spec = GuestBootSpec {
            name: "rewind".into(),
            serial: SerialOutput::new("rewind", SerialOutputMode::Shared(Arc::clone(&sink))),
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
                eprintln!("skipping snapshot_and_restore_rewinds_the_boot_vcpu: {e}");
                return;
            }
        };

        // Snapshot at the entry (RIP = 0x1000), run to the hlt (RIP advances past
        // the code and 'A' is emitted), then restore — rewinding RIP to the entry.
        let snap = guest.snapshot_vcpu().expect("snapshot boot vcpu");
        assert_eq!(guest.run(0x1000, 100).unwrap(), LoopOutcome::Halted);
        guest.restore_vcpu(&snap).expect("restore boot vcpu");
        // Running again re-executes the payload from the rewound entry.
        assert_eq!(guest.run(0x1000, 100).unwrap(), LoopOutcome::Halted);
        assert_eq!(
            &*sink.lock().unwrap(),
            b"AA",
            "restore rewound the vCPU so the payload ran twice"
        );
    }

    #[test]
    fn orchestrated_guest_sees_the_hypervisor_bit_cleared() {
        if !is_kvm_available() {
            eprintln!("skipping orchestrated_guest_sees_the_hypervisor_bit_cleared: no /dev/kvm");
            return;
        }
        // Read CPUID.1, extract ECX bit 31 (hypervisor-present), and emit it as
        // ASCII '0' (clear = stealthy) or '1' (set = detectable) over COM1:
        //   mov eax, 1
        //   cpuid
        //   shr ecx, 31        ; ecx = the hypervisor-present bit
        //   add cl, '0'
        //   mov al, cl
        //   mov dx, 0x3F8 ; out dx, al
        //   hlt
        #[rustfmt::skip]
        let image: Vec<u8> = vec![
            0x66, 0xB8, 0x01, 0x00, 0x00, 0x00, // mov eax, 1
            0x0F, 0xA2,                         // cpuid
            0x66, 0xC1, 0xE9, 0x1F,             // shr ecx, 31
            0x80, 0xC1, 0x30,                   // add cl, '0'
            0x88, 0xC8,                         // mov al, cl
            0xBA, 0xF8, 0x03,                   // mov dx, 0x3F8
            0xEE,                               // out dx, al
            0xF4,                               // hlt
        ];
        let sink = Arc::new(Mutex::new(Vec::new()));
        let spec = GuestBootSpec {
            name: "stealth-check".into(),
            serial: SerialOutput::new("stealth-check", SerialOutputMode::Shared(Arc::clone(&sink))),
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
                eprintln!("skipping orchestrated_guest_sees_the_hypervisor_bit_cleared: {e}");
                return;
            }
        };
        assert_eq!(guest.run(0x1000, 100).unwrap(), LoopOutcome::Halted);
        assert_eq!(
            &*sink.lock().unwrap(),
            b"0",
            "the orchestrator applied CPUID stealth: hypervisor-present bit is clear"
        );
    }

    #[test]
    fn resume_from_s3_re_enters_at_the_facs_waking_vector() {
        use enlil_devices::acpi::facs::{FacsBuilder, ResumeTarget};

        if !is_kvm_available() {
            eprintln!("skipping resume_from_s3_re_enters_at_the_facs_waking_vector: no /dev/kvm");
            return;
        }
        // Lay out one image over guest RAM at 0x1000:
        //   offset 0x500 (gpa 0x1500): the FACS, with OSPM's waking vector = 0x1600
        //   offset 0x600 (gpa 0x1600): the resume payload — out 0x3F8,'W'; hlt
        let mut image = vec![0u8; 0x610];
        #[rustfmt::skip]
        let wake: [u8; 7] = [
            0xB0, 0x57,       // mov al, 'W'
            0xBA, 0xF8, 0x03, // mov dx, 0x3F8
            0xEE,             // out dx, al
            0xF4,             // hlt
        ];
        image[0x600..0x600 + wake.len()].copy_from_slice(&wake);
        let mut facs = FacsBuilder::new().build();
        facs[12..16].copy_from_slice(&0x1600u32.to_le_bytes()); // firmware waking vector
        image[0x500..0x500 + facs.len()].copy_from_slice(&facs);

        let sink = Arc::new(Mutex::new(Vec::new()));
        let spec = GuestBootSpec {
            name: "s3".into(),
            serial: SerialOutput::new("s3", SerialOutputMode::Shared(Arc::clone(&sink))),
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
                eprintln!("skipping resume_from_s3_re_enters_at_the_facs_waking_vector: {e}");
                return;
            }
        };

        // Resume: read the FACS at gpa 0x1500, decode the waking vector, and
        // re-point the boot vCPU at 0x1600.
        let target = guest.resume_from_s3(0x1500).expect("resume from S3");
        assert_eq!(target, ResumeTarget::RealMode(0x1600));
        assert_eq!(guest.run(0x1000, 100).unwrap(), LoopOutcome::Halted);
        assert_eq!(
            &*sink.lock().unwrap(),
            b"W",
            "guest re-entered at the FACS firmware waking vector"
        );

        // An unarmed FACS (no waking vector) is rejected.
        let mut guest2 = GuestRuntime::prepare_real_mode(GuestBootSpec {
            name: "s3-unarmed".into(),
            serial: SerialOutput::new("s3-unarmed", SerialOutputMode::Null),
            ram_bytes: 0x2000,
            load_base: 0x1000,
            entry: 0x1000,
            image: {
                let mut img = vec![0u8; 0x540];
                img[0x500..0x500 + 64].copy_from_slice(&FacsBuilder::new().build());
                img
            },
            rtc_unix_secs: 0,
            platform: LbrPlatform::AmdSvm,
        })
        .expect("prepare unarmed guest");
        assert!(
            guest2.resume_from_s3(0x1500).is_err(),
            "an unarmed FACS has no waking vector to resume to"
        );
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
