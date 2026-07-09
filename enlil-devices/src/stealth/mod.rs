//! Anti-detection stealth modules
//!
//! Provides CPUID interception, timing stealth (TSC/APERF/MPERF),
//! LBR save/restore, and PMC virtualization to defeat anti-VM detection.

pub mod cpuid;
pub mod detection;
pub mod lbr;
pub mod pmc;
pub mod timing;
