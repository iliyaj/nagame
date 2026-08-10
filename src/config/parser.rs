// SPDX-FileCopyrightText: 2026 iliyaj
// SPDX-License-Identifier: GPL-3.0-or-later

//! Creates, parses, and serializes nagame configuration values.

use super::types::*;
use anyhow::Result;

impl Config {
    /// Create a default configuration for testing
    pub fn new_default() -> Self {
        Self {
            profiles: vec![Profile {
                name: "default".to_string(),
                outputs: vec![OutputConfig {
                    name: "*".to_string(),
                    enabled: true,
                    mode: None,
                    scale: None,
                    position: None,
                    transform: None,
                    adaptive_sync: None,
                }],
                exec: vec![],
                wallpaper: None,
                wallpaper_dir: None,
            }],
        }
    }

    /// Parse configuration from TOML string
    pub fn from_toml(content: &str) -> Result<Self> {
        let config: Self = toml::from_str(content)?;
        config.validate()?;
        Ok(config)
    }

    /// Serialize configuration to TOML string
    pub fn to_toml(&self) -> Result<String> {
        let toml = toml::to_string_pretty(self)?;
        Ok(toml)
    }
}

/// Helper functions for parsing specific config elements
impl OutputConfig {
    /// Create a new output configuration with defaults
    pub fn new(name: String) -> Self {
        Self {
            name,
            enabled: true,
            mode: None,
            scale: None,
            position: None,
            transform: None,
            adaptive_sync: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::new_default();
        assert_eq!(config.profiles.len(), 1);
        assert_eq!(config.profiles[0].name, "default");
    }

    #[test]
    fn test_toml_roundtrip() {
        let original = Config::new_default();
        let toml = original.to_toml().expect("Failed to serialize");
        let parsed = Config::from_toml(&toml).expect("Failed to parse");

        assert_eq!(original.profiles.len(), parsed.profiles.len());
        assert_eq!(original.profiles[0].name, parsed.profiles[0].name);
    }
}
