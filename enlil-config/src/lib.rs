pub mod types;
pub mod validate;

pub use types::*;
pub use validate::validate_config;

use std::path::Path;

/// Load and parse an Enlil configuration file.
pub fn load_config(path: &Path) -> anyhow::Result<EnlilConfig> {
    let content = std::fs::read_to_string(path)?;
    let config: EnlilConfig = toml::from_str(&content)?;
    let errors = validate_config(&config);
    if !errors.is_empty() {
        anyhow::bail!("Config validation failed:\n{}", errors.join("\n"));
    }
    Ok(config)
}
