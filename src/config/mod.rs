// SPDX-FileCopyrightText: 2026 iliyaj
// SPDX-License-Identifier: GPL-3.0-or-later

//! Loads and validates nagame's TOML configuration.

pub mod parser;
pub mod types;

pub use types::*;

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;
use tokio::fs;
use tracing::{debug, info};

impl Config {
    /// Load configuration from a TOML file
    pub async fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        debug!("Loading config from: {}", path.display());

        let content = fs::read_to_string(path)
            .await
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let config: Config = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        info!(
            "Successfully loaded config with {} profiles",
            config.profiles.len()
        );

        // Validate configuration
        config.validate()?;

        Ok(config)
    }

    /// Validate the configuration for common errors
    pub fn validate(&self) -> Result<()> {
        if self.profiles.is_empty() {
            anyhow::bail!("Configuration must contain at least one profile");
        }

        let mut profile_names = HashSet::new();
        for profile in &self.profiles {
            if !profile_names.insert(profile.name.as_str()) {
                anyhow::bail!("Duplicate profile name '{}'", profile.name);
            }
            profile
                .validate()
                .with_context(|| format!("Invalid profile '{}'", profile.name))?;
        }

        Ok(())
    }

    /// Validate local paths and shell syntax without connecting to other services.
    pub async fn validate_environment(&self) -> Result<()> {
        for profile in &self.profiles {
            if let Some(wallpaper) = &profile.wallpaper {
                let path = expand_home(wallpaper);
                let metadata = fs::metadata(&path).await.with_context(|| {
                    format!(
                        "Wallpaper for profile '{}' is unavailable: {}",
                        profile.name,
                        path.display()
                    )
                })?;
                if !metadata.is_file() || metadata.len() == 0 {
                    anyhow::bail!(
                        "Wallpaper for profile '{}' is not a non-empty file: {}",
                        profile.name,
                        path.display()
                    );
                }
            }

            if let Some(wallpaper_dir) = &profile.wallpaper_dir {
                let path = expand_home(wallpaper_dir);
                let metadata = fs::metadata(&path).await.with_context(|| {
                    format!(
                        "Wallpaper directory for profile '{}' is unavailable: {}",
                        profile.name,
                        path.display()
                    )
                })?;
                if !metadata.is_dir() {
                    anyhow::bail!(
                        "Wallpaper directory for profile '{}' is not a directory: {}",
                        profile.name,
                        path.display()
                    );
                }

                let mut entries = fs::read_dir(&path).await?;
                let mut found_image = false;
                while let Some(entry) = entries.next_entry().await? {
                    if is_supported_image(&entry.path()) {
                        found_image = true;
                        break;
                    }
                }
                if !found_image {
                    anyhow::bail!(
                        "Wallpaper directory for profile '{}' contains no supported images: {}",
                        profile.name,
                        path.display()
                    );
                }
            }

            for command in &profile.exec {
                let status = tokio::process::Command::new("sh")
                    .arg("-n")
                    .arg("-c")
                    .arg(command)
                    .status()
                    .await
                    .with_context(|| "Failed to run the shell syntax checker")?;
                if !status.success() {
                    anyhow::bail!(
                        "Profile '{}' contains invalid shell syntax: {}",
                        profile.name,
                        command
                    );
                }
            }
        }

        Ok(())
    }
}

/// Expands a leading `~/` or a bare `~`, leaving `~user` forms untouched.
pub fn expand_home(path: &str) -> std::path::PathBuf {
    if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    std::path::PathBuf::from(path)
}

pub(crate) fn is_supported_image(path: &Path) -> bool {
    path.is_file()
        && path.extension().is_some_and(|extension| {
            matches!(
                extension.to_string_lossy().to_ascii_lowercase().as_str(),
                "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp"
            )
        })
}

impl Profile {
    /// Validate a profile configuration
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            anyhow::bail!("Profile name cannot be empty");
        }

        if self.outputs.is_empty() {
            anyhow::bail!("Profile '{}' must contain at least one output", self.name);
        }

        let mut output_names = HashSet::new();
        for output in &self.outputs {
            if !output_names.insert(output.name.as_str()) {
                anyhow::bail!(
                    "Profile '{}' contains duplicate output selector '{}'",
                    self.name,
                    output.name
                );
            }
            output.validate().with_context(|| {
                format!(
                    "Invalid output '{}' in profile '{}'",
                    output.name, self.name
                )
            })?;
        }

        Ok(())
    }
}

impl OutputConfig {
    /// Validate an output configuration
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            anyhow::bail!("Output name cannot be empty");
        }

        // Validate mode string format if present
        if let Some(ref mode) = self.mode {
            self.parse_mode(mode).with_context(|| {
                format!("Invalid mode format '{}' for output '{}'", mode, self.name)
            })?;
        }

        // Validate scale
        if let Some(scale) = self.scale {
            if !scale.is_finite() || scale <= 0.0 || scale > 10.0 {
                anyhow::bail!("Scale must be between 0.0 and 10.0, got {}", scale);
            }
        }

        Ok(())
    }

    /// Parse mode string into components
    pub fn parse_mode(&self, mode: &str) -> Result<(u32, u32, Option<u32>)> {
        // Parse formats like "1920x1080", "1920x1080@60", "1920x1080@60.5"
        let (resolution, refresh) = match mode.split_once('@') {
            Some((resolution, refresh)) if !refresh.contains('@') => (resolution, Some(refresh)),
            Some(_) => anyhow::bail!("Mode contains more than one '@': '{}'", mode),
            None => (mode, None),
        };

        let res_parts: Vec<&str> = resolution.split('x').collect();
        if res_parts.len() != 2 {
            anyhow::bail!("Mode format must be WIDTHxHEIGHT[@REFRESH], got '{}'", mode);
        }

        let width: u32 = res_parts[0]
            .parse()
            .with_context(|| format!("Invalid width in mode '{}'", mode))?;
        let height: u32 = res_parts[1]
            .parse()
            .with_context(|| format!("Invalid height in mode '{}'", mode))?;
        if width == 0 || height == 0 {
            anyhow::bail!("Mode dimensions must be greater than zero, got '{}'", mode);
        }

        let refresh_rate = if let Some(refresh_str) = refresh {
            let rate: f64 = refresh_str
                .parse()
                .with_context(|| format!("Invalid refresh rate in mode '{}'", mode))?;
            if !rate.is_finite() || rate <= 0.0 || rate * 1000.0 > u32::MAX as f64 {
                anyhow::bail!(
                    "Refresh rate must be finite and greater than zero: '{}'",
                    mode
                );
            }
            Some((rate * 1000.0).round() as u32) // Convert to mHz
        } else {
            None
        };

        Ok((width, height, refresh_rate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(name: &str) -> OutputConfig {
        OutputConfig {
            name: name.to_string(),
            enabled: true,
            mode: None,
            scale: None,
            position: None,
            transform: None,
            adaptive_sync: None,
        }
    }

    #[test]
    fn rejects_unknown_fields() {
        let source = r#"
            [[profile]]
            name = "default"
            wallpapre = "/tmp/wallpaper.png"

              [[profile.output]]
              name = "DP-1"
        "#;

        assert!(Config::from_toml(source).is_err());
    }

    #[test]
    fn rejects_duplicate_names_and_outputs() {
        let duplicate_profiles = Config {
            profiles: vec![
                Profile {
                    name: "same".to_string(),
                    outputs: vec![output("DP-1")],
                    exec: vec![],
                    wallpaper: None,
                    wallpaper_dir: None,
                },
                Profile {
                    name: "same".to_string(),
                    outputs: vec![output("DP-2")],
                    exec: vec![],
                    wallpaper: None,
                    wallpaper_dir: None,
                },
            ],
        };
        assert!(duplicate_profiles.validate().is_err());

        let duplicate_outputs = Config {
            profiles: vec![Profile {
                name: "duplicate-outputs".to_string(),
                outputs: vec![output("DP-1"), output("DP-1")],
                exec: vec![],
                wallpaper: None,
                wallpaper_dir: None,
            }],
        };
        assert!(duplicate_outputs.validate().is_err());
    }

    #[test]
    fn rejects_invalid_mode_values() {
        let output = output("DP-1");

        for mode in ["0x1080@60", "1920x1080@0", "1920x1080@60@2"] {
            assert!(output.parse_mode(mode).is_err(), "accepted mode {mode}");
        }
    }
}
