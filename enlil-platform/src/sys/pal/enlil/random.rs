//! Randomness for the bare-metal kernel: CPU RDRAND.

use crate::io::IoResult;
use crate::sys::unsupported;

/// CPUID leaf 1, ECX bit 30: RDRAND supported.
const CPUID_1_ECX_RDRAND: u32 = 1 << 30;

/// Bounded RDRAND attempts per 64-bit word before giving up.
const RDRAND_RETRIES: u32 = 32;

/// Whether the CPU supports RDRAND (CPUID leaf 1, ECX bit 30).
///
/// Checked up front: executing RDRAND on a CPU without the feature raises
/// `#UD`, which is fatal on bare metal — a failed carry flag is not the
/// only failure mode.
fn rdrand_supported() -> bool {
    // `__cpuid` is a safe function on this toolchain.
    let info = core::arch::x86_64::__cpuid(1);
    info.ecx & CPUID_1_ECX_RDRAND != 0
}

/// Fills `buf` with hardware randomness (RDRAND).
///
/// # Errors
///
/// Returns [`Unsupported`](IoErrorKind::Unsupported) when the CPU has no
/// RDRAND (checked via CPUID) or the RNG keeps declining within
/// [`RDRAND_RETRIES`] attempts.
pub fn fill_bytes(buf: &mut [u8]) -> IoResult<()> {
    if !rdrand_supported() {
        return unsupported("CPU has no RDRAND");
    }
    let (chunks, remainder) = buf.as_chunks_mut::<8>();
    for chunk in chunks {
        let mut val = 0u64;
        let mut ok = false;
        for _ in 0..RDRAND_RETRIES {
            // SAFETY: RDRAND support was verified above; on success the
            // instruction writes `candidate`, on failure it leaves it
            // untouched.
            let mut candidate = 0u64;
            if unsafe { core::arch::x86_64::_rdrand64_step(&mut candidate) } == 1 {
                val = candidate;
                ok = true;
                break;
            }
        }
        if !ok {
            return unsupported("RDRAND keeps declining");
        }
        chunk.copy_from_slice(&val.to_le_bytes());
    }
    if !remainder.is_empty() {
        let mut block = [0u8; 8];
        fill_bytes(&mut block)?;
        remainder.copy_from_slice(&block[..remainder.len()]);
    }
    Ok(())
}
