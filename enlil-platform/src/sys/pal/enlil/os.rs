//! OS identity for the bare-metal Enlil kernel.

/// Compile-time OS constants, mirroring `std::env::consts`.
pub mod consts {
    /// Operating system: `"enlil"` — the kernel is its own OS.
    pub const OS: &str = "enlil";
    /// Architecture.
    pub const ARCH: &str = "x86_64";
    /// OS family: the kernel is not Unix; it is its own family.
    pub const FAMILY: &str = "enlil";
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
#[must_use]
pub const fn info() -> Info {
    Info {
        os: consts::OS,
        arch: consts::ARCH,
        family: consts::FAMILY,
    }
}
