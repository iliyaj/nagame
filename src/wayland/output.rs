// SPDX-FileCopyrightText: 2026 iliyaj
// SPDX-License-Identifier: GPL-3.0-or-later

//! Discovers Wayland outputs and applies profile-defined display configurations.

use super::client::WaylandClient;
use super::protocols::{HeadConfiguration, OutputHead, OutputMode};
use crate::config::Profile;
use crate::profile::matcher::resolve_profile_outputs;
use anyhow::{anyhow, Result};
use tracing::{error, info, warn};
use wayland_client::protocol::wl_output;

/// Output manager for handling display configuration
pub struct OutputManager {
    wayland_client: Option<WaylandClient>,
}

impl OutputManager {
    /// Create a new output manager
    pub fn new() -> Self {
        Self {
            wayland_client: None,
        }
    }

    /// Initialize with Wayland client (with retry logic for startup)
    pub async fn initialize(&mut self) -> Result<()> {
        info!("Initializing output manager with Wayland client");

        // Retry with backoff while the compositor starts.
        let mut attempts = 0;
        let max_attempts = 5;
        let mut delay_ms = 500; // Start with 500ms

        loop {
            attempts += 1;

            match WaylandClient::new().await {
                Ok(client) => {
                    self.wayland_client = Some(client);
                    info!("Output manager initialized successfully");
                    return Ok(());
                }
                Err(e) => {
                    if attempts >= max_attempts {
                        error!(
                            "Failed to initialize Wayland client after {} attempts: {}",
                            attempts, e
                        );
                        warn!("Output management will be disabled");
                        return Ok(());
                    }

                    warn!(
                        "Wayland connection attempt {} failed ({}), retrying in {}ms",
                        attempts, e, delay_ms
                    );
                    tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                    delay_ms = (delay_ms * 2).min(5000); // Cap at 5 seconds
                }
            }
        }
    }

    /// Check if output management is available
    pub fn is_available(&self) -> bool {
        self.wayland_client
            .as_ref()
            .map(|c| c.has_output_management())
            .unwrap_or(false)
    }

    /// Refreshes outputs, reconnecting to Wayland when necessary.
    pub async fn refresh_outputs(&mut self) -> Result<()> {
        // If client not initialized, try to initialize it now
        if self.wayland_client.is_none() {
            match WaylandClient::new().await {
                Ok(client) => {
                    info!("Wayland client successfully initialized on retry");
                    self.wayland_client = Some(client);
                }
                Err(e) => {
                    // Only log at debug level to avoid spam
                    tracing::debug!("Wayland client still not available: {}", e);
                    return Ok(()); // Return Ok to avoid breaking the main loop
                }
            }
        }

        if let Some(client) = &mut self.wayland_client {
            client.refresh_outputs().await?;
        }
        Ok(())
    }

    /// Duplicate of the Wayland connection fd to wait on for readability
    pub fn connection_fd(&self) -> Option<Result<std::os::fd::OwnedFd>> {
        self.wayland_client.as_ref().map(|c| c.connection_fd())
    }

    /// Dispatches queued and signalled events, returning the number handled.
    pub fn pump_events(&mut self) -> Result<usize> {
        let Some(client) = &mut self.wayland_client else {
            return Ok(0);
        };
        let queued = client.dispatch_pending()?;
        Ok(queued + client.read_and_dispatch()?)
    }

    /// Dispatch anything already queued and flush outgoing requests before blocking
    pub fn flush_before_wait(&mut self) -> Result<usize> {
        let Some(client) = &mut self.wayland_client else {
            return Ok(0);
        };
        let dispatched = client.dispatch_pending()?;
        client.flush()?;
        Ok(dispatched)
    }

    /// Which complete configuration the compositor has us on
    pub fn configuration_generation(&self) -> u64 {
        self.wayland_client
            .as_ref()
            .map(|c| c.configuration_generation())
            .unwrap_or(0)
    }

    /// Get all connected output heads
    pub fn get_heads(&self) -> Vec<&OutputHead> {
        if let Some(client) = &self.wayland_client {
            let mut heads: Vec<_> = client.get_outputs().values().collect();
            heads.sort_by(|left, right| left.name.cmp(&right.name));
            heads
        } else {
            Vec::new()
        }
    }

    /// Get a specific head by name
    pub fn get_head(&self, name: &str) -> Option<&OutputHead> {
        self.wayland_client
            .as_ref()
            .and_then(|c| c.get_output(name))
    }

    /// Find the best matching mode for given parameters
    pub fn find_mode(&self, head_name: &str, mode_str: &str) -> Option<OutputMode> {
        let head = self.get_head(head_name)?;

        // Parse mode string like "1920x1080@60"
        let (target_width, target_height, target_refresh) = parse_mode_string(mode_str).ok()?;

        let mut best_match: Option<OutputMode> = None;
        let mut best_delta = i32::MAX;

        for mode in &head.modes {
            if mode.width != target_width || mode.height != target_height {
                continue;
            }

            if let Some(refresh) = target_refresh {
                let delta = (mode.refresh_mhz - refresh).abs();
                if delta < best_delta || best_match.is_none() {
                    best_match = Some(mode.clone());
                    best_delta = delta;
                }
            } else if mode.preferred {
                return Some(mode.clone());
            } else if best_match.is_none()
                || mode.refresh_mhz > best_match.as_ref().unwrap().refresh_mhz
            {
                best_match = Some(mode.clone());
            }
        }

        best_match
    }

    /// Apply a profile configuration
    pub async fn apply_profile(&mut self, profile: &Profile) -> Result<bool> {
        info!(
            "Applying display configuration for profile '{}'",
            profile.name
        );

        // Check if client is available first
        if self.wayland_client.is_none() {
            return Err(anyhow!("Wayland client not initialized"));
        }

        if !self.is_available() {
            warn!("wlr-output-management not available - cannot apply display configuration");
            return Ok(false);
        }

        // Check if current configuration already matches profile
        if self.config_matches_profile(profile) {
            info!("Display configuration already matches profile '{}' - skipping apply to avoid flicker", profile.name);
            return Ok(true);
        }

        let heads = self.get_heads();
        let resolved_outputs = resolve_profile_outputs(profile, &heads)
            .ok_or_else(|| anyhow!("Profile no longer matches the connected outputs"))?;

        // Pre-resolve modes and concrete head names to avoid borrow conflicts.
        let mut output_configs = Vec::new();
        for (output, head_name) in resolved_outputs {
            let mode = if output.enabled {
                output
                    .mode
                    .as_ref()
                    .map(|mode_str| {
                        self.find_mode(&head_name, mode_str).ok_or_else(|| {
                            anyhow!(
                                "No mode matching '{}' is available for output '{}'",
                                mode_str,
                                head_name
                            )
                        })
                    })
                    .transpose()?
            } else {
                None
            };

            let transform = output.transform.map(|t| match t {
                crate::config::Transform::Normal => wl_output::Transform::Normal,
                crate::config::Transform::Rotate90 => wl_output::Transform::_90,
                crate::config::Transform::Rotate180 => wl_output::Transform::_180,
                crate::config::Transform::Rotate270 => wl_output::Transform::_270,
                crate::config::Transform::Flipped => wl_output::Transform::Flipped,
                crate::config::Transform::Flipped90 => wl_output::Transform::Flipped90,
                crate::config::Transform::Flipped180 => wl_output::Transform::Flipped180,
                crate::config::Transform::Flipped270 => wl_output::Transform::Flipped270,
            });

            output_configs.push((output, head_name, mode, transform));
        }

        // Now get mutable reference to client and use it
        let client = match self.wayland_client.as_mut() {
            Some(client) => client,
            None => return Err(anyhow!("Wayland client not initialized")),
        };

        // Create new configuration
        client.create_configuration().await?;

        // Configure each output
        for (output, head_name, mode, transform) in output_configs {
            let head_config = HeadConfiguration {
                enabled: output.enabled,
                mode: mode.clone(),
                position: output.position.map(|p| (p[0], p[1])),
                transform,
                scale: output.scale,
                adaptive_sync: output.adaptive_sync,
            };

            if let Err(e) = client.configure_head(&head_name, &head_config).await {
                error!("Failed to configure output '{}': {}", head_name, e);
                return Ok(false);
            }
        }

        // Apply the configuration
        match client.apply_configuration().await {
            Ok(()) => {
                info!(
                    "Successfully applied display configuration for profile '{}'",
                    profile.name
                );
                Ok(true)
            }
            Err(e) => {
                error!("Failed to apply display configuration: {}", e);
                Ok(false)
            }
        }
    }

    /// Test a profile configuration without applying
    pub async fn test_profile(&mut self, profile: &Profile) -> Result<bool> {
        info!(
            "Testing display configuration for profile '{}'",
            profile.name
        );

        // Check if client is available first
        if self.wayland_client.is_none() {
            return Err(anyhow!("Wayland client not initialized"));
        }

        if !self.is_available() {
            warn!("wlr-output-management not available - cannot test display configuration");
            return Ok(false);
        }

        let heads = self.get_heads();
        let resolved_outputs = resolve_profile_outputs(profile, &heads)
            .ok_or_else(|| anyhow!("Profile no longer matches the connected outputs"))?;

        // Pre-resolve modes and concrete head names to avoid borrow conflicts.
        let mut output_configs = Vec::new();
        for (output, head_name) in resolved_outputs {
            let mode = if output.enabled {
                output
                    .mode
                    .as_ref()
                    .map(|mode_str| {
                        self.find_mode(&head_name, mode_str).ok_or_else(|| {
                            anyhow!(
                                "No mode matching '{}' is available for output '{}'",
                                mode_str,
                                head_name
                            )
                        })
                    })
                    .transpose()?
            } else {
                None
            };

            let transform = output.transform.map(|t| match t {
                crate::config::Transform::Normal => wl_output::Transform::Normal,
                crate::config::Transform::Rotate90 => wl_output::Transform::_90,
                crate::config::Transform::Rotate180 => wl_output::Transform::_180,
                crate::config::Transform::Rotate270 => wl_output::Transform::_270,
                crate::config::Transform::Flipped => wl_output::Transform::Flipped,
                crate::config::Transform::Flipped90 => wl_output::Transform::Flipped90,
                crate::config::Transform::Flipped180 => wl_output::Transform::Flipped180,
                crate::config::Transform::Flipped270 => wl_output::Transform::Flipped270,
            });

            output_configs.push((output, head_name, mode, transform));
        }

        // Now get mutable reference to client and use it
        let client = match self.wayland_client.as_mut() {
            Some(client) => client,
            None => return Err(anyhow!("Wayland client not initialized")),
        };

        // Create new configuration
        client.create_configuration().await?;

        // Configure each output
        for (output, head_name, mode, transform) in output_configs {
            let head_config = HeadConfiguration {
                enabled: output.enabled,
                mode: mode.clone(),
                position: output.position.map(|p| (p[0], p[1])),
                transform,
                scale: output.scale,
                adaptive_sync: output.adaptive_sync,
            };

            if let Err(e) = client.configure_head(&head_name, &head_config).await {
                error!("Failed to configure output '{}': {}", head_name, e);
                return Ok(false);
            }
        }

        // Test the configuration
        match client.test_configuration().await {
            Ok(()) => {
                info!(
                    "Display configuration test passed for profile '{}'",
                    profile.name
                );
                Ok(true)
            }
            Err(e) => {
                error!("Display configuration test failed: {}", e);
                Ok(false)
            }
        }
    }

    /// Get the Wayland client for direct access
    pub fn client(&self) -> Option<&WaylandClient> {
        self.wayland_client.as_ref()
    }

    /// Get mutable reference to Wayland client
    pub fn client_mut(&mut self) -> Option<&mut WaylandClient> {
        self.wayland_client.as_mut()
    }

    /// Handles pending events and reports whether outputs may have changed.
    pub async fn handle_events(&mut self) -> bool {
        if let Some(client) = &mut self.wayland_client {
            let _ = client.handle_events().await;
            // Events were processed - outputs may have changed
            true
        } else {
            false
        }
    }

    /// Check if current display configuration matches a profile
    fn config_matches_profile(&self, profile: &Profile) -> bool {
        let heads = self.get_heads();
        let Some(resolved_outputs) = resolve_profile_outputs(profile, &heads) else {
            return false;
        };

        for (output_config, head_name) in resolved_outputs {
            let head = match self.get_head(&head_name) {
                Some(h) => h,
                None => {
                    info!("Config doesn't match: output '{}' not found", head_name);
                    return false;
                }
            };

            // Check enabled state
            if head.enabled != output_config.enabled {
                info!(
                    "Config doesn't match: enabled mismatch for '{}' (current: {}, requested: {})",
                    head_name, head.enabled, output_config.enabled
                );
                return false;
            }

            // If output is disabled, we don't need to check other properties
            if !output_config.enabled {
                continue;
            }

            // Check mode if specified
            if let Some(ref mode_str) = output_config.mode {
                let requested_mode = match self.find_mode(&head_name, mode_str) {
                    Some(m) => m,
                    None => {
                        info!(
                            "Config doesn't match: requested mode '{}' not available for '{}'",
                            mode_str, head_name
                        );
                        return false;
                    }
                };

                let current_mode = match &head.current_mode {
                    Some(m) => m,
                    None => {
                        info!(
                            "Config doesn't match: no current mode set for '{}'",
                            head_name
                        );
                        return false;
                    }
                };

                // Allow 5 Hz for hardware-reported refresh-rate variance.
                if requested_mode.width != current_mode.width
                    || requested_mode.height != current_mode.height
                    || (requested_mode.refresh_mhz - current_mode.refresh_mhz).abs() > 5000
                {
                    info!("Config doesn't match: mode mismatch for '{}' (current: {}x{}@{}Hz, requested: {}x{}@{}Hz)",
                        head_name,
                        current_mode.width, current_mode.height, current_mode.refresh_hz(),
                        requested_mode.width, requested_mode.height, requested_mode.refresh_hz());
                    return false;
                }
            }

            // Check position if specified
            if let Some(requested_pos) = output_config.position {
                let current_pos = head.position.unwrap_or_default();

                if (requested_pos[0], requested_pos[1]) != current_pos {
                    return false;
                }
            }

            // Check scale if specified (allow small tolerance for floating point)
            if let Some(requested_scale) = output_config.scale {
                if (head.scale - requested_scale).abs() > 0.01 {
                    return false;
                }
            }

            // Check adaptive sync if specified
            if let Some(requested_adaptive_sync) = output_config.adaptive_sync {
                if head.adaptive_sync != Some(requested_adaptive_sync) {
                    return false;
                }
            }
        }

        // All checks passed
        true
    }
}

impl Default for OutputManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse mode string like "1920x1080@60" into components
fn parse_mode_string(mode_str: &str) -> Result<(i32, i32, Option<i32>)> {
    let parts: Vec<&str> = mode_str.split('@').collect();
    let resolution = parts[0];

    let res_parts: Vec<&str> = resolution.split('x').collect();
    if res_parts.len() != 2 {
        return Err(anyhow!("Invalid mode format: {}", mode_str));
    }

    let width: i32 = res_parts[0].parse()?;
    let height: i32 = res_parts[1].parse()?;

    let refresh = if parts.len() > 1 {
        let rate: f64 = parts[1].parse()?;
        Some((rate * 1000.0) as i32) // Convert to millihertz
    } else {
        None
    };

    Ok((width, height, refresh))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mode_string() {
        let (width, height, refresh) = parse_mode_string("1920x1080@60").unwrap();
        assert_eq!(width, 1920);
        assert_eq!(height, 1080);
        assert_eq!(refresh, Some(60000)); // 60Hz in mHz

        let (width, height, refresh) = parse_mode_string("2560x1440").unwrap();
        assert_eq!(width, 2560);
        assert_eq!(height, 1440);
        assert_eq!(refresh, None);
    }
}
