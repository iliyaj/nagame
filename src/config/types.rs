// SPDX-FileCopyrightText: 2026 iliyaj
// SPDX-License-Identifier: GPL-3.0-or-later

//! Defines the serializable types used by nagame's configuration.

use serde::{Deserialize, Serialize};

/// Main configuration structure
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// List of display profiles
    #[serde(rename = "profile")]
    pub profiles: Vec<Profile>,
}

/// A display profile that defines output configurations and commands
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Profile name (for identification)
    pub name: String,

    /// Output configurations for this profile
    #[serde(rename = "output")]
    pub outputs: Vec<OutputConfig>,

    /// Commands to execute when this profile is activated
    #[serde(default)]
    pub exec: Vec<String>,

    /// Wallpaper to set when this profile is activated (specific file)
    pub wallpaper: Option<String>,

    /// Directory for random wallpapers; takes precedence over `wallpaper`.
    pub wallpaper_dir: Option<String>,
}

/// Configuration for a single output/display
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputConfig {
    /// Output name (e.g., "eDP-1", "HDMI-A-2")
    pub name: String,

    /// Whether the output is enabled
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Display mode (e.g., "1920x1080@60")
    pub mode: Option<String>,

    /// Display scaling factor
    pub scale: Option<f64>,

    /// Position [x, y] in the global coordinate space
    pub position: Option<[i32; 2]>,

    /// Display transform/rotation
    pub transform: Option<Transform>,

    /// Adaptive sync (FreeSync/G-Sync)
    pub adaptive_sync: Option<bool>,
}

/// Display transform/rotation options
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transform {
    Normal,
    #[serde(rename = "90")]
    Rotate90,
    #[serde(rename = "180")]
    Rotate180,
    #[serde(rename = "270")]
    Rotate270,
    Flipped,
    #[serde(rename = "flipped-90")]
    Flipped90,
    #[serde(rename = "flipped-180")]
    Flipped180,
    #[serde(rename = "flipped-270")]
    Flipped270,
}

impl Transform {
    /// Convert to Wayland transform value
    pub fn to_wayland_transform(self) -> u32 {
        match self {
            Transform::Normal => 0,
            Transform::Rotate90 => 1,
            Transform::Rotate180 => 2,
            Transform::Rotate270 => 3,
            Transform::Flipped => 4,
            Transform::Flipped90 => 5,
            Transform::Flipped180 => 6,
            Transform::Flipped270 => 7,
        }
    }
}

/// Default value for enabled field
fn default_enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_parsing() {
        let toml_str = r#"
[[profile]]
name = "laptop"
exec = ["awww img ~/wallpapers/laptop.jpg"]

  [[profile.output]]
  name = "eDP-1"
  mode = "1920x1080@60"
  scale = 1.5
  position = [0, 0]

[[profile]]
name = "docked"
exec = ["awww img ~/wallpapers/desktop.jpg"]

  [[profile.output]]
  name = "eDP-1"
  enabled = false

  [[profile.output]]
  name = "HDMI-A-2"
  mode = "2560x1440@144"
  scale = 1.0
  position = [0, 0]
        "#;

        let config: Config = toml::from_str(toml_str).expect("Failed to parse config");

        assert_eq!(config.profiles.len(), 2);
        assert_eq!(config.profiles[0].name, "laptop");
        assert_eq!(config.profiles[0].outputs.len(), 1);
        assert_eq!(config.profiles[0].outputs[0].name, "eDP-1");
        assert_eq!(
            config.profiles[0].outputs[0].mode,
            Some("1920x1080@60".to_string())
        );

        assert_eq!(config.profiles[1].name, "docked");
        assert_eq!(config.profiles[1].outputs.len(), 2);
        assert!(!config.profiles[1].outputs[0].enabled);
    }

    #[test]
    fn test_transform_conversion() {
        assert_eq!(Transform::Normal.to_wayland_transform(), 0);
        assert_eq!(Transform::Rotate90.to_wayland_transform(), 1);
        assert_eq!(Transform::Rotate180.to_wayland_transform(), 2);
        assert_eq!(Transform::Rotate270.to_wayland_transform(), 3);
    }
}
