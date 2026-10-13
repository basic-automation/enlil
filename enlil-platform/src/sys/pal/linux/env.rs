//! Environment bindings — mirrors `std::sys::env`'s role.

/// Re-exported so `sys::env::VarError` names the same type on every backend.
pub use crate::sys::VarError;

/// Fetches the environment variable `key`.
///
/// # Errors
///
/// Returns [`VarError::NotPresent`] if the variable is unset and
/// [`VarError::NotUnicode`] if it is not valid Unicode.
pub fn var(key: &str) -> Result<String, VarError> {
    std::env::var(key).map_err(|e| match e {
        std::env::VarError::NotPresent => VarError::NotPresent,
        std::env::VarError::NotUnicode(_) => VarError::NotUnicode,
    })
}

/// Process arguments, mirroring `std::env::Args`.
#[derive(Debug)]
pub struct Args {
    inner: std::env::Args,
}

/// Returns the process arguments.
#[must_use]
pub fn args() -> Args {
    Args {
        inner: std::env::args(),
    }
}

impl Iterator for Args {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        self.inner.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn var_missing_is_not_present() {
        assert_eq!(
            var("ENLIL_SYS_TEST_DEFINITELY_UNSET_12345"),
            Err(VarError::NotPresent)
        );
    }

    #[test]
    fn var_roundtrip() {
        // SAFETY: single-threaded test; no other thread touches this var.
        unsafe {
            std::env::set_var("ENLIL_SYS_TEST_VAR", "hello");
        }
        assert_eq!(var("ENLIL_SYS_TEST_VAR"), Ok(String::from("hello")));
        // SAFETY: same as above.
        unsafe {
            std::env::remove_var("ENLIL_SYS_TEST_VAR");
        }
        assert_eq!(var("ENLIL_SYS_TEST_VAR"), Err(VarError::NotPresent));
    }

    #[test]
    fn args_has_argv0() {
        assert!(args().next().is_some());
    }
}
