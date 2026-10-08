//! Anti-detection stealth modules
//!
//! Provides CPUID interception, timing stealth (TSC/APERF/MPERF),
//! LBR save/restore, and PMC virtualization to defeat anti-VM detection.
//!
//! [`suites`] curates the pafish + al-khaser check inventories (roadmap item
//! 5.8, T-5.4) and maps every check onto the stealth in this module.

pub mod cpuid;
pub mod detection;
pub mod lbr;
pub mod pmc;
pub mod suites;
pub mod timing;
