// SPDX-FileCopyrightText: 2026 iliyaj
// SPDX-License-Identifier: GPL-3.0-or-later

//! Starts and communicates with the awww daemon to query and set wallpapers.

use anyhow::Result;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, error, info, warn};

/// Client for communicating with awww daemon
pub struct AwwwClient {
    // Future: could add connection state, caching, etc.
}

impl AwwwClient {
    /// Create a new awww client
    pub async fn new() -> Result<Self> {
        debug!("Creating awww client");
        Ok(Self {})
    }

    /// Check if awww daemon is currently running
    pub async fn is_daemon_running(&self) -> bool {
        let result = Command::new("awww")
            .arg("query")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;

        match result {
            Ok(status) => status.success(),
            Err(_) => false,
        }
    }

    /// Start the awww daemon with retry logic
    pub async fn start_daemon(&self) -> Result<()> {
        info!("Starting awww daemon");

        let mut cmd = Command::new("awww-daemon");
        cmd.arg("--format")
            .arg("argb")
            .arg("--no-cache")
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        // Start daemon in background
        let _child = cmd.spawn()?;

        // Wait and verify with retries (compositor might still be initializing)
        let mut attempts = 0;
        let max_attempts = 5;

        while attempts < max_attempts {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            attempts += 1;

            if self.is_daemon_running().await {
                info!(
                    "awww daemon started successfully after {} attempt(s)",
                    attempts
                );
                return Ok(());
            }

            if attempts < max_attempts {
                debug!(
                    "awww daemon not ready yet, waiting... (attempt {}/{})",
                    attempts, max_attempts
                );
            }
        }

        error!("Failed to start awww daemon after {} attempts", attempts);
        anyhow::bail!("awww daemon failed to start");
    }

    /// Set wallpaper to an image file
    pub async fn set_wallpaper(&self, image_path: &str) -> Result<()> {
        debug!("Setting wallpaper via awww: {}", image_path);

        let output = Command::new("awww")
            .arg("img")
            .arg(image_path)
            .arg("--transition-type")
            .arg("fade")
            .arg("--transition-duration")
            .arg("1")
            .output()
            .await?;

        if output.status.success() {
            info!("Wallpaper set successfully: {}", image_path);
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!("Failed to set wallpaper: {}", stderr);
            anyhow::bail!("awww failed to set wallpaper: {}", stderr);
        }

        Ok(())
    }

    /// Clear wallpaper to solid color
    pub async fn clear_wallpaper(&self, color: &str) -> Result<()> {
        debug!("Clearing wallpaper to color: {}", color);

        let output = Command::new("awww")
            .arg("clear")
            .arg(color)
            .output()
            .await?;

        if output.status.success() {
            info!("Wallpaper cleared to color: {}", color);
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!("Failed to clear wallpaper: {}", stderr);
            anyhow::bail!("awww failed to clear wallpaper: {}", stderr);
        }

        Ok(())
    }

    /// Query current wallpaper status
    pub async fn query_wallpaper(&self) -> Result<Option<String>> {
        debug!("Querying current wallpaper status");

        let output = Command::new("awww").arg("query").output().await?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);

            // Parse awww query output to extract image path
            for line in stdout.lines() {
                if line.contains("currently displaying: image:") {
                    if let Some(path) = line.split("image: ").nth(1) {
                        return Ok(Some(path.trim().to_string()));
                    }
                }
            }

            // No image found (might be showing color)
            debug!("No image wallpaper found in query output");
            Ok(None)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("Failed to query wallpaper status: {}", stderr);
            Ok(None)
        }
    }

    /// Kill the awww daemon
    pub async fn kill_daemon(&self) -> Result<()> {
        info!("Killing awww daemon");

        let output = Command::new("awww").arg("kill").output().await?;

        if output.status.success() {
            info!("awww daemon killed successfully");
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("Failed to kill awww daemon: {}", stderr);
        }

        Ok(())
    }

    /// Get awww version information
    pub async fn get_version(&self) -> Result<String> {
        let output = Command::new("awww").arg("--version").output().await?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            Ok(stdout.trim().to_string())
        } else {
            anyhow::bail!("Failed to get awww version");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_client_creation() {
        let client = AwwwClient::new().await;
        assert!(client.is_ok());
    }

    #[tokio::test]
    async fn test_daemon_check() {
        let client = AwwwClient::new().await.unwrap();
        let _is_running = client.is_daemon_running().await;
    }

    #[tokio::test]
    async fn test_version_check() {
        let client = AwwwClient::new().await.unwrap();

        if tokio::process::Command::new("which")
            .arg("awww")
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            let version = client.get_version().await;
            assert!(version.is_ok());
            assert!(version.unwrap().contains("awww"));
        }
    }
}
