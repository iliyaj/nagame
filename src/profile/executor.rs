// SPDX-FileCopyrightText: 2026 iliyaj
// SPDX-License-Identifier: GPL-3.0-or-later

//! Applies display profiles, runs profile commands, and manages profile wallpapers.

use crate::awww::AwwwManager;
use crate::config::{expand_home, is_supported_image, Profile};
use crate::wayland::OutputManager;
use anyhow::Result;
use rand::seq::SliceRandom;
use std::process::Stdio;
use tokio::process::Command;
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info, warn};

// Retry for up to five seconds while awww creates surfaces for rebuilt outputs.
const RESTORE_ATTEMPTS: u32 = 20;
const RESTORE_RETRY: Duration = Duration::from_millis(250);

pub struct ProfileExecutor {
    awww_manager: Option<AwwwManager>,
}

impl ProfileExecutor {
    /// Create a new profile executor
    pub fn new() -> Self {
        Self { awww_manager: None }
    }

    /// Initialize with awww support.
    pub async fn with_awww() -> Result<Self> {
        let awww_manager = match crate::awww::init_awww().await? {
            Some(manager) => Some(manager),
            None => {
                warn!("awww not available - wallpaper functionality disabled");
                None
            }
        };

        Ok(Self { awww_manager })
    }

    /// Restores saved wallpaper state and reports whether one existed.
    pub async fn restore_wallpaper(&mut self) -> Result<bool> {
        if let Some(ref mut awww) = self.awww_manager {
            info!("Attempting to restore wallpaper from saved state");
            match awww.restore_wallpaper().await {
                Ok(true) => {
                    info!("Wallpaper restored successfully - will preserve manual changes");
                    Ok(true)
                }
                Ok(false) => {
                    debug!("No saved wallpaper state found");
                    Ok(false)
                }
                Err(e) => {
                    // Distinguish missing state from a failed restore.
                    Err(e)
                }
            }
        } else {
            debug!("awww not available, skipping wallpaper restoration");
            Ok(false)
        }
    }

    /// Retries restoration while awww creates surfaces for rebuilt outputs.
    pub async fn restore_saved_wallpaper(&mut self) -> bool {
        for attempt in 1..=RESTORE_ATTEMPTS {
            match self.restore_wallpaper().await {
                Ok(true) => return true,
                // Nothing saved - retrying cannot conjure one up
                Ok(false) => return false,
                Err(e) if attempt == RESTORE_ATTEMPTS => {
                    warn!(
                        "Gave up restoring the saved wallpaper after {} attempts: {}",
                        attempt, e
                    );
                    return false;
                }
                Err(e) => {
                    debug!(
                        "Wallpaper restore attempt {} failed ({}), retrying",
                        attempt, e
                    );
                    sleep(RESTORE_RETRY).await;
                }
            }
        }
        false
    }

    /// Save current wallpaper state for restoration after wake/restart
    pub async fn save_current_wallpaper(&mut self) -> Result<bool> {
        if let Some(ref mut awww) = self.awww_manager {
            awww.save_current_wallpaper().await
        } else {
            debug!("awww not available, skipping wallpaper state save");
            Ok(false)
        }
    }

    /// Applies a profile, optionally preserving the current wallpaper.
    pub async fn apply_profile(
        &mut self,
        profile: &Profile,
        output_manager: &mut OutputManager,
        skip_wallpaper: bool,
    ) -> Result<bool> {
        info!("Applying profile '{}'", profile.name);

        if let Err(e) = self.configure_displays(profile, output_manager).await {
            error!(
                "Failed to configure displays for profile '{}': {}",
                profile.name, e
            );
            return Ok(false);
        }

        if !skip_wallpaper {
            // `wallpaper_dir` takes precedence over `wallpaper`.
            let resolved = if let Some(ref dir) = profile.wallpaper_dir {
                match Self::pick_random_wallpaper(dir).await {
                    Ok(path) => Some(path),
                    Err(e) => {
                        error!(
                            "Failed to pick wallpaper from dir for profile '{}': {}",
                            profile.name, e
                        );
                        None
                    }
                }
            } else {
                profile.wallpaper.clone()
            };

            if let Some(ref wallpaper_path) = resolved {
                if let Err(e) = self.set_wallpaper(wallpaper_path).await {
                    error!(
                        "Failed to set wallpaper for profile '{}': {}",
                        profile.name, e
                    );
                }
            }
        } else {
            info!("Skipping profile wallpaper to preserve manual wallpaper change");
        }

        if let Err(e) = self.execute_commands(profile).await {
            error!(
                "Failed to execute commands for profile '{}': {}",
                profile.name, e
            );
            // Command failure does not undo a successful display configuration.
        }

        info!("Successfully applied profile '{}'", profile.name);
        Ok(true)
    }

    /// Configure displays according to profile settings
    async fn configure_displays(
        &self,
        profile: &Profile,
        output_manager: &mut OutputManager,
    ) -> Result<()> {
        debug!("Configuring displays for profile '{}'", profile.name);

        if !output_manager.is_available() {
            warn!("Output management not available - skipping display configuration");
            return Ok(());
        }

        // Apply the entire profile configuration atomically
        match output_manager.apply_profile(profile).await {
            Ok(true) => {
                info!(
                    "Successfully applied display configuration for profile '{}'",
                    profile.name
                );
            }
            Ok(false) => {
                warn!(
                    "Display configuration partially failed for profile '{}'",
                    profile.name
                );
                return Err(anyhow::anyhow!("Display configuration failed"));
            }
            Err(e) => {
                error!("Failed to apply display configuration: {}", e);
                return Err(e);
            }
        }

        Ok(())
    }

    /// Pick a random image file from a directory
    async fn pick_random_wallpaper(dir_path: &str) -> Result<String> {
        let expanded = expand_home(dir_path).to_string_lossy().into_owned();

        let mut images: Vec<std::path::PathBuf> = Vec::new();
        let mut entries = tokio::fs::read_dir(&expanded)
            .await
            .map_err(|e| anyhow::anyhow!("Cannot read wallpaper_dir '{}': {}", expanded, e))?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if is_supported_image(&path) {
                images.push(path);
            }
        }

        if images.is_empty() {
            anyhow::bail!("No image files found in wallpaper_dir '{}'", expanded);
        }

        let chosen = images
            .choose(&mut rand::thread_rng())
            .expect("images is non-empty");
        let path_str = chosen.to_string_lossy().to_string();
        info!("Picked random wallpaper from '{}': {}", expanded, path_str);
        Ok(path_str)
    }

    /// Set wallpaper using awww
    async fn set_wallpaper(&mut self, wallpaper_path: &str) -> Result<()> {
        if let Some(ref mut awww) = self.awww_manager {
            info!("Setting wallpaper: {}", wallpaper_path);

            let expanded_path = expand_home(wallpaper_path).to_string_lossy().into_owned();

            // Validate path exists before setting
            if !std::path::Path::new(&expanded_path).exists() {
                return Err(anyhow::anyhow!(
                    "Wallpaper file not found: {}",
                    expanded_path
                ));
            }

            awww.set_wallpaper(&expanded_path).await?;
            info!("Successfully set wallpaper: {}", expanded_path);
        } else {
            debug!("awww not available, skipping wallpaper: {}", wallpaper_path);
        }
        Ok(())
    }

    /// Execute commands specified in the profile
    async fn execute_commands(&self, profile: &Profile) -> Result<()> {
        if profile.exec.is_empty() {
            debug!("No commands to execute for profile '{}'", profile.name);
            return Ok(());
        }

        info!(
            "Executing {} commands for profile '{}'",
            profile.exec.len(),
            profile.name
        );

        for (idx, command) in profile.exec.iter().enumerate() {
            debug!("Executing command {}: {}", idx + 1, command);

            if let Err(e) = self.execute_single_command(command).await {
                error!("Command {} failed: {}", command, e);
                // Continue with other commands even if one fails
            }
        }

        Ok(())
    }

    /// Execute a single shell command
    async fn execute_single_command(&self, command: &str) -> Result<()> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = cmd.output().await?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.trim().is_empty() {
                debug!("Command output: {}", stdout.trim());
            }
            info!("Command completed successfully: {}", command);
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "Command failed (exit code: {}): {}\nStderr: {}",
                output.status.code().unwrap_or(-1),
                command,
                stderr
            );
        }

        Ok(())
    }

    /// Test if a command exists and is executable
    pub async fn test_command(&self, command: &str) -> bool {
        // Extract the first word (command name) for testing
        let cmd_name = command.split_whitespace().next().unwrap_or(command);

        let result = Command::new("which").arg(cmd_name).output().await;

        match result {
            Ok(output) => output.status.success(),
            Err(_) => false,
        }
    }

    /// Validate all commands in a profile can be executed
    pub async fn validate_profile_commands(&self, profile: &Profile) -> Vec<String> {
        let mut missing_commands = Vec::new();

        for command in &profile.exec {
            if !self.test_command(command).await {
                let cmd_name = command.split_whitespace().next().unwrap_or(command);
                missing_commands.push(cmd_name.to_string());
            }
        }

        if !missing_commands.is_empty() {
            warn!(
                "Profile '{}' has commands with missing executables: {:?}",
                profile.name, missing_commands
            );
        }

        missing_commands
    }
}

impl Default for ProfileExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{OutputConfig, Profile};
    use crate::wayland::OutputManager;

    #[tokio::test]
    async fn test_command_execution() {
        let executor = ProfileExecutor::new();

        // Test a simple command that should always work
        let result = executor.execute_single_command("echo 'test'").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_command_validation() {
        let executor = ProfileExecutor::new();

        // Test existing command
        assert!(executor.test_command("echo").await);

        // Test non-existing command
        assert!(!executor.test_command("non_existent_command_12345").await);
    }

    #[tokio::test]
    async fn test_profile_execution() {
        let mut executor = ProfileExecutor::new();
        let mut output_manager = OutputManager::new();

        let profile = Profile {
            name: "test".to_string(),
            outputs: vec![OutputConfig {
                name: "TEST".to_string(),
                enabled: true,
                mode: Some("1920x1080".to_string()),
                scale: Some(1.0),
                position: Some([0, 0]),
                transform: None,
                adaptive_sync: None,
            }],
            exec: vec!["echo 'profile applied'".to_string()],
            wallpaper: None,
            wallpaper_dir: None,
        };

        // This should complete without errors (though display config is not implemented)
        let result = executor
            .apply_profile(&profile, &mut output_manager, false)
            .await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }
}
