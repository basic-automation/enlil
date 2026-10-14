//! Per-guest virtual UEFI firmware (OVMF) plumbing (item 5.6).
//!
//! Every guest gets its own virtual UEFI: the launcher maps the guest's OVMF
//! CODE image **read-only** just below the top of the 32-bit address space and
//! the guest's private VARS store **writable** just below the code — the same
//! split-QEMU layout (`-drive if=pflash,format=raw,readonly=on` for CODE,
//! a per-guest copy for VARS). The boot vCPU starts at the x86 reset vector
//! (`0xFFFF_FFF0`, inside the code flash) exactly like power-on hardware, and
//! OVMF brings itself up from there; Windows then boots in UEFI mode off the
//! guest's disks.
//!
//! [`UefiFlashImage::load_for_guest`] is the whole load flow: read the CODE
//! image named by the guest's `[guest.<id>.firmware]` block, stage the VARS
//! template to a per-guest writable store under the given state directory
//! (the template is copied only when the staged file does not exist yet, so a
//! guest's NVRAM survives reboots), and validate both images (size bounds,
//! 4 KiB erase-block alignment, and the `_FVH` firmware-volume signature).
//! [`UefiFlashImage::into_buffers`] then copies the images into page-aligned
//! host buffers the KVM memory slots point at.
//!
//! The flash window is derived purely from the image sizes, so it is fully
//! unit-testable without `/dev/kvm`: [`FlashLayout::for_images`] computes the
//! guest-physical placement and [`FlashLayout::check_ram_no_overlap`] rejects
//! a guest RAM region that would collide with it (the launcher must keep RAM
//! below the flash — a single contiguous RAM mapping cannot also cover the
//! top of the 4 GiB window).
//!
//! Manual/QEMU verification (needs `/dev/kvm` and real OVMF images, so it is
//! not part of `cargo test`): point a guest's `[guest.<id>.firmware]` block
//! at `OVMF_CODE_4M.fd` / `OVMF_VARS_4M.fd`, boot via
//! [`crate::orchestrator::boot_first_guest_uefi`], and confirm the UEFI shell
//! (or the Windows installer) comes up on the guest console; a second boot
//! must see the first boot's NVRAM (e.g. boot-order changes) intact.

#[cfg(target_os = "linux")]
pub use linux::{FlashBuffers, FlashLayout, UefiFlashImage, FLASH_TOP_GPA, RESET_VECTOR_GPA};

#[cfg(target_os = "linux")]
mod linux {
    use crate::error::{Error, Result};
    use crate::kvm_backend::GuestRam;
    use enlil_config::FirmwareConfig;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Guest-physical address of the x86 reset vector. Real hardware starts
    /// here (`CS:IP = F000:FFF0`); an OVMF CODE image's SEC entry point lives
    /// at this address, so the boot vCPU is prepared for exactly this.
    pub const RESET_VECTOR_GPA: u64 = 0xFFFF_FFF0;

    /// Top of the 32-bit guest-physical address space. Firmware flash is
    /// mapped immediately below this, code first, then the variable store.
    pub const FLASH_TOP_GPA: u64 = 0x1_0000_0000;

    /// Offset of the `_FVH` signature inside a firmware volume header
    /// (UEFI PI, FV header field `Signature` at +0x28).
    const FVH_SIGNATURE_OFFSET: usize = 0x28;
    const FVH_SIGNATURE: &[u8; 4] = b"_FVH";
    /// Smallest plausible flash image (a 64 KiB variable store).
    const MIN_FLASH_BYTES: usize = 64 * 1024;
    /// Largest plausible flash image (well above the 4 MiB OVMF builds).
    const MAX_FLASH_BYTES: usize = 16 * 1024 * 1024;
    /// Flash erase-block granularity both images must be a multiple of.
    const FLASH_BLOCK_BYTES: usize = 4096;

    /// Guest-physical placement of one guest's firmware flash: the CODE image
    /// mapped read-only at the very top of 4 GiB, the private VARS store
    /// writable directly below it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FlashLayout {
        /// Guest-physical base of the read-only CODE image.
        pub code_gpa: u64,
        /// CODE image size in bytes.
        pub code_size: u64,
        /// Guest-physical base of the writable VARS store.
        pub vars_gpa: u64,
        /// VARS store size in bytes.
        pub vars_size: u64,
    }

    impl FlashLayout {
        /// Compute the flash placement for images of `code_len` / `vars_len`
        /// bytes, validating the sizes.
        ///
        /// # Errors
        /// [`Error::Config`] if either size is out of bounds, not a multiple
        /// of the flash erase block, or the pair would not cover the reset
        /// vector inside the code image.
        pub fn for_images(code_len: usize, vars_len: usize) -> Result<Self> {
            for (label, len) in [("code", code_len), ("vars", vars_len)] {
                if len < MIN_FLASH_BYTES {
                    return Err(Error::Config(format!(
                        "firmware {label} image is {len} bytes, below the {MIN_FLASH_BYTES}-byte minimum"
                    )));
                }
                if len > MAX_FLASH_BYTES {
                    return Err(Error::Config(format!(
                        "firmware {label} image is {len} bytes, above the {MAX_FLASH_BYTES}-byte maximum"
                    )));
                }
                if !len.is_multiple_of(FLASH_BLOCK_BYTES) {
                    return Err(Error::Config(format!(
                        "firmware {label} image is {len} bytes, not a multiple of the \
                         {FLASH_BLOCK_BYTES}-byte flash erase block"
                    )));
                }
            }
            let code_size = code_len as u64;
            let vars_size = vars_len as u64;
            let code_gpa = FLASH_TOP_GPA - code_size;
            let vars_gpa = code_gpa.checked_sub(vars_size).ok_or_else(|| {
                Error::Config(format!(
                    "firmware images ({code_size} + {vars_size} bytes) do not fit below 4 GiB"
                ))
            })?;
            // The reset vector must land inside the code flash; with the size
            // bounds above this always holds, but it is the invariant the boot
            // path depends on, so check it explicitly.
            if !(code_gpa <= RESET_VECTOR_GPA && RESET_VECTOR_GPA < FLASH_TOP_GPA) {
                return Err(Error::Config(format!(
                    "firmware code window [{code_gpa:#x}, {FLASH_TOP_GPA:#x}) does not \
                     contain the reset vector {RESET_VECTOR_GPA:#x}"
                )));
            }
            Ok(Self {
                code_gpa,
                code_size,
                vars_gpa,
                vars_size,
            })
        }

        /// Reject a guest RAM region that would overlap the flash window.
        /// `load_base`/`ram_bytes` are the guest-physical placement of the
        /// RAM mapping; the flash sits at the top of 4 GiB, so RAM must end
        /// at or below [`FlashLayout::vars_gpa`].
        ///
        /// # Errors
        /// [`Error::Config`] if the RAM size is zero, the range overflows, or
        /// it reaches into the flash window.
        pub fn check_ram_no_overlap(&self, load_base: u64, ram_bytes: usize) -> Result<()> {
            if ram_bytes == 0 {
                return Err(Error::Config(
                    "UEFI boot needs a non-zero guest RAM size".into(),
                ));
            }
            let ram_end = (load_base as u128)
                .checked_add(ram_bytes as u128)
                .ok_or_else(|| {
                    Error::Config(format!(
                        "guest RAM range [{load_base:#x}, +{ram_bytes:#x}) overflows"
                    ))
                })?;
            if ram_end > self.vars_gpa as u128 {
                return Err(Error::Config(format!(
                    "guest RAM [{load_base:#x}, {ram_end:#x}) overlaps the firmware flash \
                     window [{:#x}, {FLASH_TOP_GPA:#x}) — keep RAM below the flash \
                     (reduce memory_mb or move the load base)",
                    self.vars_gpa
                )));
            }
            Ok(())
        }
    }

    /// Validate one raw firmware image: size bounds, erase-block alignment,
    /// and the `_FVH` firmware-volume signature at the head of the image.
    fn check_image(label: &str, bytes: &[u8]) -> Result<()> {
        if bytes.len() < MIN_FLASH_BYTES || bytes.len() > MAX_FLASH_BYTES {
            return Err(Error::Config(format!(
                "firmware {label} image is {} bytes, outside the {MIN_FLASH_BYTES}..={MAX_FLASH_BYTES}-byte range",
                bytes.len()
            )));
        }
        if !bytes.len().is_multiple_of(FLASH_BLOCK_BYTES) {
            return Err(Error::Config(format!(
                "firmware {label} image is {} bytes, not a multiple of the \
                 {FLASH_BLOCK_BYTES}-byte flash erase block",
                bytes.len()
            )));
        }
        let sig = bytes
            .get(FVH_SIGNATURE_OFFSET..FVH_SIGNATURE_OFFSET + 4)
            .ok_or_else(|| {
                Error::Config(format!(
                    "firmware {label} image is too small for a firmware-volume header"
                ))
            })?;
        if sig != FVH_SIGNATURE {
            return Err(Error::Config(format!(
                "firmware {label} image has no _FVH firmware-volume signature at +{FVH_SIGNATURE_OFFSET:#x}"
            )));
        }
        Ok(())
    }

    /// One guest's validated virtual-UEFI firmware: the CODE and VARS images
    /// plus their guest-physical layout. Build with [`UefiFlashImage::new`]
    /// (validates raw bytes) or [`UefiFlashImage::load_for_guest`] (the full
    /// config-driven load flow).
    pub struct UefiFlashImage {
        code: Vec<u8>,
        vars: Vec<u8>,
        layout: FlashLayout,
    }

    impl UefiFlashImage {
        /// Validate raw CODE/VARS image bytes and compute their layout.
        ///
        /// # Errors
        /// [`Error::Config`] if either image fails [`check_image`] or the
        /// pair's layout is invalid.
        pub fn new(code: Vec<u8>, vars: Vec<u8>) -> Result<Self> {
            check_image("code", &code)?;
            check_image("vars", &vars)?;
            let layout = FlashLayout::for_images(code.len(), vars.len())?;
            Ok(Self { code, vars, layout })
        }

        /// The guest-physical flash placement for these images.
        #[must_use]
        pub const fn layout(&self) -> FlashLayout {
            self.layout
        }

        /// The full config-driven load flow for one guest: read the CODE image
        /// named by `fw`, stage the VARS template to the guest's private
        /// writable store under `state_dir`, then validate both images.
        ///
        /// Returns the image plus the staged per-guest VARS path (the file
        /// the running variable store is written back to).
        ///
        /// # Errors
        /// [`Error::Config`] if the state directory cannot be created, either
        /// image cannot be read, or validation fails.
        pub fn load_for_guest(
            fw: &FirmwareConfig,
            guest_id: &str,
            state_dir: &Path,
        ) -> Result<(Self, PathBuf)> {
            let code = fs::read(&fw.code).map_err(|e| {
                Error::Config(format!(
                    "guest '{guest_id}': cannot read firmware.code '{}': {e}",
                    fw.code.display()
                ))
            })?;
            let staged_vars = Self::stage_vars_copy(&fw.vars, guest_id, state_dir)?;
            let vars = fs::read(&staged_vars).map_err(|e| {
                Error::Config(format!(
                    "guest '{guest_id}': cannot read staged firmware vars '{}': {e}",
                    staged_vars.display()
                ))
            })?;
            Ok((Self::new(code, vars)?, staged_vars))
        }

        /// Copy the VARS template to the guest's private writable store
        /// `<state_dir>/<guest_id>-OVMF_VARS.fd`, unless that file already
        /// exists — an existing store is the guest's live NVRAM and must never
        /// be clobbered with the template.
        ///
        /// # Errors
        /// [`Error::Config`] if the state directory cannot be created or the
        /// template cannot be copied.
        pub fn stage_vars_copy(
            template: &Path,
            guest_id: &str,
            state_dir: &Path,
        ) -> Result<PathBuf> {
            fs::create_dir_all(state_dir).map_err(|e| {
                Error::Config(format!(
                    "guest '{guest_id}': cannot create firmware state dir '{}': {e}",
                    state_dir.display()
                ))
            })?;
            let dest = state_dir.join(format!("{guest_id}-OVMF_VARS.fd"));
            if !dest.exists() {
                fs::copy(template, &dest).map_err(|e| {
                    Error::Config(format!(
                        "guest '{guest_id}': cannot stage firmware vars template '{}' to '{}': {e}",
                        template.display(),
                        dest.display()
                    ))
                })?;
            }
            Ok(dest)
        }

        /// Copy the images into page-aligned host buffers suitable for KVM
        /// memory slots. Consumes the image; the returned buffers must outlive
        /// the VM that maps them.
        #[must_use]
        pub fn into_buffers(self) -> FlashBuffers {
            let mut code_ram = GuestRam::new(self.code.len());
            code_ram.as_mut_slice().copy_from_slice(&self.code);
            let mut vars_ram = GuestRam::new(self.vars.len());
            vars_ram.as_mut_slice().copy_from_slice(&self.vars);
            FlashBuffers {
                code: code_ram,
                vars: vars_ram,
                layout: self.layout,
            }
        }
    }

    /// Page-aligned, KVM-mappable firmware flash buffers owned by the
    /// [`GuestRuntime`](crate::orchestrator::GuestRuntime) for the VM's
    /// lifetime: CODE mapped read-only, VARS mapped writable.
    pub struct FlashBuffers {
        /// Read-only CODE image buffer, mapped at [`FlashLayout::code_gpa`].
        pub code: GuestRam,
        /// Writable per-guest VARS buffer, mapped at [`FlashLayout::vars_gpa`].
        pub vars: GuestRam,
        /// The guest-physical placement of both.
        pub layout: FlashLayout,
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Unwrap the `Err` side of a `Result` whose `Ok` type is not `Debug`
        /// (so neither `unwrap_err` nor `expect_err` can be used).
        fn expect_err<T>(result: std::result::Result<T, crate::Error>, what: &str) -> crate::Error {
            match result {
                Ok(_) => panic!("expected {what} to fail"),
                Err(err) => err,
            }
        }

        /// A minimal plausible flash image: 64 KiB with the `_FVH` signature
        /// at the firmware-volume header offset.
        fn fake_image() -> Vec<u8> {
            let mut img = vec![0u8; 64 * 1024];
            img[FVH_SIGNATURE_OFFSET..FVH_SIGNATURE_OFFSET + 4].copy_from_slice(FVH_SIGNATURE);
            img
        }

        #[test]
        fn flash_layout_places_code_below_4g_and_vars_below_code() {
            let layout = FlashLayout::for_images(4 * 1024 * 1024, 512 * 1024).unwrap();
            assert_eq!(layout.code_gpa, 0xFFC0_0000);
            assert_eq!(layout.code_size, 4 * 1024 * 1024);
            assert_eq!(layout.vars_gpa, 0xFFB8_0000);
            assert_eq!(layout.vars_size, 512 * 1024);
            assert!(layout.code_gpa <= RESET_VECTOR_GPA);
        }

        #[test]
        fn flash_layout_rejects_bad_sizes() {
            // Tiny / unaligned / oversized images are rejected before any KVM state exists.
            assert!(FlashLayout::for_images(0, 64 * 1024).is_err());
            assert!(FlashLayout::for_images(4 * 1024 * 1024, 1000).is_err());
            assert!(FlashLayout::for_images(4 * 1024 * 1024 + 1, 64 * 1024).is_err());
            assert!(FlashLayout::for_images(32 * 1024 * 1024, 64 * 1024).is_err());
        }

        #[test]
        fn flash_image_rejects_missing_fvh_signature() {
            let code = vec![0u8; 1024 * 1024]; // right size, no _FVH
            let vars = fake_image();
            let err = expect_err(UefiFlashImage::new(code, vars), "image without _FVH");
            assert!(err.to_string().contains("_FVH"), "unexpected: {err}");
        }

        #[test]
        fn flash_image_accepts_valid_pair() {
            let image = UefiFlashImage::new(fake_image(), fake_image()).unwrap();
            let layout = image.layout();
            assert_eq!(layout.code_gpa, FLASH_TOP_GPA - 64 * 1024);
            assert_eq!(layout.vars_gpa, FLASH_TOP_GPA - 128 * 1024);
        }

        #[test]
        fn ram_overlapping_flash_is_rejected() {
            let layout = FlashLayout::for_images(4 * 1024 * 1024, 512 * 1024).unwrap();
            // 8 GiB of RAM at 0 reaches straight through the flash window.
            let err = expect_err(
                layout.check_ram_no_overlap(0, 8 * 1024 * 1024 * 1024),
                "RAM overlapping the flash window",
            );
            assert!(
                err.to_string().contains("overlaps the firmware flash"),
                "unexpected: {err}"
            );
            // Zero RAM is rejected too (GuestRam would panic on it).
            assert!(layout.check_ram_no_overlap(0, 0).is_err());
        }

        #[test]
        fn ram_below_flash_is_accepted() {
            let layout = FlashLayout::for_images(4 * 1024 * 1024, 512 * 1024).unwrap();
            // 3 GiB at 0 ends at 0xC000_0000, below the vars base 0xFFB8_0000.
            layout
                .check_ram_no_overlap(0, 3 * 1024 * 1024 * 1024)
                .unwrap();
            // RAM ending exactly at the vars base is fine.
            layout
                .check_ram_no_overlap(0, layout.vars_gpa as usize)
                .unwrap();
        }

        /// Unique scratch dir per test (no tempfile dev-dependency).
        fn scratch_dir(tag: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "enlil-uefi-fw-test-{}-{}-{}",
                std::process::id(),
                tag,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[test]
        fn stage_vars_copy_does_not_clobber_an_existing_store() {
            let dir = scratch_dir("stage");
            let template = dir.join("template.fd");
            fs::write(&template, fake_image()).unwrap();

            let staged = UefiFlashImage::stage_vars_copy(&template, "win11", &dir).unwrap();
            assert_eq!(staged, dir.join("win11-OVMF_VARS.fd"));

            // The guest writes NVRAM (the store diverges from the template)...
            let mut live = fake_image();
            live[0x100] = 0xAB;
            fs::write(&staged, &live).unwrap();
            // ...and a second staging must not clobber it.
            let staged2 = UefiFlashImage::stage_vars_copy(&template, "win11", &dir).unwrap();
            assert_eq!(staged2, staged);
            assert_eq!(fs::read(&staged2).unwrap()[0x100], 0xAB);
            fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn stage_vars_copy_rejects_a_missing_template() {
            let dir = scratch_dir("stage-missing");
            let err = expect_err(
                UefiFlashImage::stage_vars_copy(&dir.join("nope.fd"), "win11", &dir.join("state")),
                "missing vars template",
            );
            assert!(
                err.to_string().contains("cannot stage firmware vars"),
                "unexpected: {err}"
            );
            fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn load_for_guest_reads_code_and_stages_vars() {
            let dir = scratch_dir("load");
            let code_path = dir.join("OVMF_CODE.fd");
            let vars_template = dir.join("OVMF_VARS.fd");
            fs::write(&code_path, fake_image()).unwrap();
            fs::write(&vars_template, fake_image()).unwrap();
            let fw = FirmwareConfig {
                code: code_path,
                vars: vars_template,
            };
            let state_dir = dir.join("state");
            let (image, staged) = UefiFlashImage::load_for_guest(&fw, "win11", &state_dir).unwrap();
            assert_eq!(staged, state_dir.join("win11-OVMF_VARS.fd"));
            assert!(staged.exists());
            assert_eq!(image.layout().code_size, 64 * 1024);
            fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn load_for_guest_reports_a_missing_code_image() {
            let dir = scratch_dir("load-missing");
            let fw = FirmwareConfig {
                code: dir.join("no-code.fd"),
                vars: dir.join("no-vars.fd"),
            };
            let err = expect_err(
                UefiFlashImage::load_for_guest(&fw, "win11", &dir),
                "missing code image",
            );
            assert!(
                err.to_string().contains("firmware.code"),
                "unexpected: {err}"
            );
            fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn into_buffers_are_page_aligned_and_hold_the_images() {
            let code = fake_image();
            let vars = fake_image();
            let buffers = UefiFlashImage::new(code.clone(), vars.clone())
                .unwrap()
                .into_buffers();
            assert_eq!(buffers.code.host_addr() % 4096, 0);
            assert_eq!(buffers.vars.host_addr() % 4096, 0);
            assert_eq!(&buffers.code.as_slice()[..code.len()], code.as_slice());
            assert_eq!(&buffers.vars.as_slice()[..vars.len()], vars.as_slice());
        }
    }
}
