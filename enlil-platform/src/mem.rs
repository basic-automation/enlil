//! Freestanding memory builtins (`memcpy`, `memmove`, `memset`, `memcmp`).
//!
//! On a hosted target the C library provides these symbols and LLVM-generated
//! code calls them freely. On the freestanding `x86_64-unknown-enlil` target
//! there is no C library, and cargo's `-Zbuild-std` does not enable
//! compiler-builtins' `mem` feature (only the mangled Rust symbols are built),
//! so the platform layer provides the four C-ABI symbols itself.
//!
//! The loops use volatile accesses so LLVM cannot lower them back into calls
//! to the very symbols being defined (which would recurse forever).

use core::ffi::{c_int, c_void};

/// Copy `n` bytes from `src` to `dest`.
///
/// The regions must not overlap (use [`memmove`] otherwise).
///
/// # Safety
///
/// `src` must be readable for `n` bytes, `dest` writable for `n` bytes, and
/// the two regions must not overlap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcpy(dest: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    let dest = dest.cast::<u8>();
    let src = src.cast::<u8>();
    let mut i = 0;
    while i < n {
        // SAFETY: caller guarantees `src.add(i)` / `dest.add(i)` are valid.
        unsafe {
            core::ptr::write_volatile(dest.add(i), core::ptr::read_volatile(src.add(i)));
        }
        i += 1;
    }
    dest.cast::<c_void>()
}

/// Copy `n` bytes from `src` to `dest`, correctly handling overlap.
///
/// # Safety
///
/// `src` must be readable for `n` bytes and `dest` writable for `n` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memmove(dest: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    let dest = dest.cast::<u8>();
    let src = src.cast::<u8>();
    if (dest as usize) < (src as usize) {
        // Dest starts below src: forward copy cannot clobber unread bytes.
        let mut i = 0;
        while i < n {
            // SAFETY: caller guarantees validity of both ranges.
            unsafe {
                core::ptr::write_volatile(dest.add(i), core::ptr::read_volatile(src.add(i)));
            }
            i += 1;
        }
    } else {
        // Dest starts at or above src: copy backward instead.
        let mut i = n;
        while i > 0 {
            i -= 1;
            // SAFETY: caller guarantees validity of both ranges.
            unsafe {
                core::ptr::write_volatile(dest.add(i), core::ptr::read_volatile(src.add(i)));
            }
        }
    }
    dest.cast::<c_void>()
}

/// Fill `n` bytes at `dest` with the byte value `c`.
///
/// # Safety
///
/// `dest` must be writable for `n` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memset(dest: *mut c_void, c: c_int, n: usize) -> *mut c_void {
    // C semantics: `memset` converts `c` to `unsigned char`; the truncation
    // and sign loss are intentional.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let byte = c as u8;
    let dest = dest.cast::<u8>();
    let mut i = 0;
    while i < n {
        // SAFETY: caller guarantees `dest.add(i)` is writable.
        unsafe {
            core::ptr::write_volatile(dest.add(i), byte);
        }
        i += 1;
    }
    dest.cast::<c_void>()
}

/// Compare `n` bytes at `s1` and `s2`; returns 0 if equal, otherwise the
/// difference of the first differing bytes.
///
/// # Safety
///
/// Both `s1` and `s2` must be readable for `n` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcmp(s1: *const c_void, s2: *const c_void, n: usize) -> c_int {
    let s1 = s1.cast::<u8>();
    let s2 = s2.cast::<u8>();
    let mut i = 0;
    while i < n {
        // SAFETY: caller guarantees readability of both ranges.
        let (a, b) = unsafe {
            (
                core::ptr::read_volatile(s1.add(i)),
                core::ptr::read_volatile(s2.add(i)),
            )
        };
        if a != b {
            return c_int::from(a) - c_int::from(b);
        }
        i += 1;
    }
    0
}
