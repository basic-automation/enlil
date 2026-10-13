//! Randomness bindings — mirrors `std::sys::random`'s role.

use crate::io::{IoError, IoErrorKind, IoResult};

/// Fills `buf` with cryptographic randomness from the OS (`getrandom(2)`).
///
/// # Errors
///
/// Returns an error if the OS RNG fails.
pub fn fill_bytes(buf: &mut [u8]) -> IoResult<()> {
    let mut offset = 0;
    while offset < buf.len() {
        // SAFETY: `getrandom` fills at most the remaining bytes of `buf`.
        let n = unsafe {
            libc::getrandom(
                buf[offset..].as_mut_ptr().cast::<libc::c_void>(),
                buf.len() - offset,
                0,
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(IoError::from(err));
        }
        offset += usize::try_from(n).map_err(|_| IoError::new(IoErrorKind::Other))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_succeeds() {
        let mut buf = [0u8; 64];
        fill_bytes(&mut buf).unwrap();
        // Not a randomness test — just proves the syscall path works and
        // touches the whole buffer (all-zero output would be astonishing).
        assert!(buf.iter().any(|&b| b != 0));
    }

    #[test]
    fn fill_empty_is_noop() {
        fill_bytes(&mut []).unwrap();
    }
}
