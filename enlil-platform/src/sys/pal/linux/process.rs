//! Process bindings — mirrors `std::sys::process`'s role.

/// Terminates the process with `code`.
pub fn exit(code: i32) -> ! {
    std::process::exit(code);
}

/// Aborts the process abnormally.
pub fn abort() -> ! {
    std::process::abort()
}

/// The current process id.
#[must_use]
pub fn id() -> u32 {
    std::process::id()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_is_nonzero() {
        assert_ne!(id(), 0);
    }
}
