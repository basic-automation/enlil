#![deny(clippy::all, clippy::pedantic, clippy::nursery)]

pub mod mac;
pub mod types;
pub mod validate;

pub use mac::{is_hypervisor_oui, mac_rejection_reason, parse_mac};
pub use types::*;
pub use validate::validate_config;

use std::path::Path;

/// Load and parse an Enlil configuration file.
///
/// # Errors
///
/// Returns an error if the file cannot be read, the TOML is malformed,
/// or validation fails.
pub fn load_config(path: &Path) -> anyhow::Result<EnlilConfig> {
    let content = std::fs::read_to_string(path)?;
    let config: EnlilConfig = toml::from_str(&content)?;
    let errors = validate_config(&config);
    if !errors.is_empty() {
        anyhow::bail!("Config validation failed:\n{}", errors.join("\n"));
    }
    Ok(config)
}
