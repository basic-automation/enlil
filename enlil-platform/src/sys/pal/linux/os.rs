//! OS identity — `std::env::consts`-style constants plus a runtime query.

/// Compile-time OS constants, mirroring `std::env::consts`.
pub mod consts {
    /// Operating system: `"linux"`.
    pub const OS: &str = "linux";
    /// Architecture (the only hosted arch supported).
    pub const ARCH: &str = "x86_64";
    /// OS family.
    pub const FAMILY: &str = "unix";
}

/// Runtime OS identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Info {
    /// Operating system name.
    pub os: &'static str,
    /// Architecture name.
    pub arch: &'static str,
    /// OS family name.
    pub family: &'static str,
}

/// Returns the runtime OS identity.
///
/// Constant today (the backend is fixed at compile time); kept as a
/// function so future backends can detect the OS at runtime.
#[must_use]
pub const fn info() -> Info {
    Info {
        os: consts::OS,
        arch: consts::ARCH,
        family: consts::FAMILY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_matches_consts() {
        let i = info();
        assert_eq!(i.os, consts::OS);
        assert_eq!(i.arch, consts::ARCH);
        assert_eq!(i.family, consts::FAMILY);
        assert_eq!(consts::OS, "linux");
        assert_eq!(consts::FAMILY, "unix");
    }
}
