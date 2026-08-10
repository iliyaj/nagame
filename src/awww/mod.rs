// SPDX-FileCopyrightText: 2026 iliyaj
// SPDX-License-Identifier: GPL-3.0-or-later

//! Coordinates awww wallpaper updates with persistent wallpaper-state restoration.

pub mod client;
pub mod restore;

pub use client::AwwwClient;
pub use restore::WallpaperRestore;

use anyhow::Result;
use tracing::{debug, info, warn};

/// awww integration manager
pub struct AwwwManager {
    client: AwwwClient,
    restore: WallpaperRestore,
}

impl AwwwManager {
    /// Create a new awww manager
    pub async fn new() -> Result<Self> {
        info!("Initializing awww integration");

        let client = AwwwClient::new().await?;
        let restore = WallpaperRestore::new();

        Ok(Self { client, restore })
    }

    /// Check if awww daemon is running
    pub async fn is_daemon_running(&self) -> bool {
        self.client.is_daemon_running().await
    }

    /// Start awww daemon if not running
    pub async fn ensure_daemon_running(&self) -> Result<()> {
        if !self.is_daemon_running().await {
            info!("Starting awww daemon");
            self.client.start_daemon().await?;
        }
        Ok(())
    }

    /// Sets the wallpaper and persists it to quickshell's shared state file.
    pub async fn set_wallpaper(&mut self, image_path: &str) -> Result<()> {
        info!("Setting wallpaper: {}", image_path);

        // Ensure daemon is running
        self.ensure_daemon_running().await?;

        // Set the wallpaper
        self.client.set_wallpaper(image_path).await?;

        // Save state for restoration (shared with quickshell)
        self.restore.save_wallpaper_state(image_path).await?;

        Ok(())
    }

    /// Restore wallpaper from saved state
    pub async fn restore_wallpaper(&mut self) -> Result<bool> {
        debug!("Attempting to restore wallpaper from saved state");

        if let Some(wallpaper_path) = self.restore.get_saved_wallpaper().await? {
            info!("Restoring wallpaper: {}", wallpaper_path);

            // Ensure daemon is running
            self.ensure_daemon_running().await?;

            // Restore the wallpaper
            self.client.set_wallpaper(&wallpaper_path).await?;

            return Ok(true);
        }

        warn!("No saved wallpaper state found");
        Ok(false)
    }

    /// Get current wallpaper information
    pub async fn get_current_wallpaper(&self) -> Result<Option<String>> {
        self.client.query_wallpaper().await
    }

    /// Query current wallpaper from awww and save it for restoration
    pub async fn save_current_wallpaper(&mut self) -> Result<bool> {
        info!("Querying current wallpaper from awww to save state");

        if let Some(current_wallpaper) = self.client.query_wallpaper().await? {
            info!("Found current wallpaper: {}", current_wallpaper);
            self.restore
                .save_wallpaper_state(&current_wallpaper)
                .await?;
            return Ok(true);
        }

        warn!("No wallpaper currently set in awww");
        Ok(false)
    }

    /// Clear wallpaper to solid color
    pub async fn clear_wallpaper(&self, color: &str) -> Result<()> {
        info!("Clearing wallpaper to color: {}", color);
        self.client.clear_wallpaper(color).await
    }
}

/// Helper function to check if awww is installed
pub async fn check_awww_installation() -> bool {
    use tokio::process::Command;

    let result = Command::new("which").arg("awww").output().await;

    match result {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

/// Initialize awww integration if available
pub async fn init_awww() -> Result<Option<AwwwManager>> {
    if !check_awww_installation().await {
        warn!("awww not found in PATH - wallpaper management disabled");
        return Ok(None);
    }

    info!("awww found, initializing integration");
    let manager = AwwwManager::new().await?;
    Ok(Some(manager))
}
