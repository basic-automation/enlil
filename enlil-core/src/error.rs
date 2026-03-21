//! Error types for enlil-core.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("memory error: {0}")]
    Memory(String),

    #[error("vcpu error: {0}")]
    Vcpu(String),

    #[error("vm error: {0}")]
    Vm(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("serial error: {0}")]
    Serial(String),

    #[error("cpuid error: {0}")]
    Cpuid(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
