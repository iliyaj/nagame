// SPDX-FileCopyrightText: 2026 iliyaj
// SPDX-License-Identifier: GPL-3.0-or-later

//! Persists and retrieves the wallpaper path used to restore wallpaper state.

use anyhow::Result;
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::{debug, info, warn};

/// Manages wallpaper state for restoration after display changes
pub struct WallpaperRestore {
    state_file: PathBuf,
}

impl WallpaperRestore {
    /// Uses quickshell's shared wallpaper state file.
    pub fn new() -> Self {
        // Share wallpaper state with quickshell.
        let state_file = dirs::state_dir()
            .unwrap_or_else(|| {
                // Fallback to XDG_STATE_HOME or ~/.local/state
                std::env::var("XDG_STATE_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| {
                        dirs::home_dir()
                            .unwrap_or_else(|| PathBuf::from("."))
                            .join(".local/state")
                    })
            })
            .join("quickshell")
            .join("user")
            .join("current-wallpaper");

        Self { state_file }
    }

    /// Create with custom state file path
    pub fn with_state_file<P: AsRef<Path>>(state_file: P) -> Self {
        Self {
            state_file: state_file.as_ref().to_path_buf(),
        }
    }

    /// Save current wallpaper path for future restoration
    pub async fn save_wallpaper_state(&self, wallpaper_path: &str) -> Result<()> {
        debug!("Saving wallpaper state: {}", wallpaper_path);

        // Ensure parent directory exists
        if let Some(parent) = self.state_file.parent() {
            fs::create_dir_all(parent).await?;
        }

        // Save the wallpaper path
        fs::write(&self.state_file, wallpaper_path).await?;

        info!("Wallpaper state saved to: {}", self.state_file.display());
        Ok(())
    }

    /// Get saved wallpaper path if it exists and the file is still available
    pub async fn get_saved_wallpaper(&self) -> Result<Option<String>> {
        if !self.state_file.exists() {
            debug!("No wallpaper state file found");
            return Ok(None);
        }

        let wallpaper_path = fs::read_to_string(&self.state_file).await?;
        let wallpaper_path = wallpaper_path.trim();

        // Verify the wallpaper file still exists
        if !Path::new(wallpaper_path).exists() {
            warn!("Saved wallpaper file no longer exists: {}", wallpaper_path);
            return Ok(None);
        }

        debug!("Found saved wallpaper: {}", wallpaper_path);
        Ok(Some(wallpaper_path.to_string()))
    }

    /// Clear saved wallpaper state
    pub async fn clear_state(&self) -> Result<()> {
        if self.state_file.exists() {
            fs::remove_file(&self.state_file).await?;
            info!("Wallpaper state cleared");
        }
        Ok(())
    }

    /// Get the path to the state file
    pub fn state_file_path(&self) -> &Path {
        &self.state_file
    }

    /// Check if wallpaper state exists
    pub fn has_saved_state(&self) -> bool {
        self.state_file.exists()
    }
}

impl Default for WallpaperRestore {
    fn default() -> Self {
        Self::new()
    }
}

/// Helper functions for wallpaper management
impl WallpaperRestore {
    /// Validate that a wallpaper file exists and is readable
    pub async fn validate_wallpaper_file<P: AsRef<Path>>(path: P) -> Result<()> {
        let path = path.as_ref();

        if !path.exists() {
            anyhow::bail!("Wallpaper file does not exist: {}", path.display());
        }

        if !path.is_file() {
            anyhow::bail!("Wallpaper path is not a file: {}", path.display());
        }

        // Try to read file metadata to verify it's accessible
        let metadata = fs::metadata(path).await?;
        if metadata.len() == 0 {
            anyhow::bail!("Wallpaper file is empty: {}", path.display());
        }

        Ok(())
    }

    /// Get common wallpaper directories to search
    pub fn get_wallpaper_search_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();

        // User's Pictures directory
        if let Some(pictures_dir) = dirs::picture_dir() {
            dirs.push(pictures_dir.join("Wallpapers"));
            dirs.push(pictures_dir);
        }

        // Home directory wallpapers
        if let Some(home_dir) = dirs::home_dir() {
            dirs.push(home_dir.join("wallpapers"));
            dirs.push(home_dir.join("Wallpapers"));
            dirs.push(home_dir.join(".local/share/wallpapers"));
        }

        // System wallpaper directories
        dirs.push(PathBuf::from("/usr/share/wallpapers"));
        dirs.push(PathBuf::from("/usr/share/backgrounds"));

        dirs
    }

    /// Find wallpaper files in common directories
    pub async fn find_wallpaper_files() -> Result<Vec<PathBuf>> {
        let mut wallpapers = Vec::new();
        let search_dirs = Self::get_wallpaper_search_dirs();

        for dir in search_dirs {
            if !dir.exists() || !dir.is_dir() {
                continue;
            }

            debug!("Searching for wallpapers in: {}", dir.display());

            let mut entries = fs::read_dir(&dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();

                if path.is_file() {
                    if let Some(extension) = path.extension() {
                        let ext = extension.to_string_lossy().to_lowercase();
                        if matches!(
                            ext.as_str(),
                            "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp"
                        ) {
                            wallpapers.push(path);
                        }
                    }
                }
            }
        }

        info!("Found {} wallpaper files", wallpapers.len());
        Ok(wallpapers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_state_save_and_load() {
        let temp_dir = tempdir().unwrap();
        let state_file = temp_dir.path().join("wallpaper-state");

        let restore = WallpaperRestore::with_state_file(&state_file);

        // Create a test wallpaper file
        let test_wallpaper = temp_dir.path().join("test.jpg");
        fs::write(&test_wallpaper, "fake image data").await.unwrap();

        let wallpaper_path = test_wallpaper.to_string_lossy();

        // Save state
        restore.save_wallpaper_state(&wallpaper_path).await.unwrap();

        // Load state
        let loaded = restore.get_saved_wallpaper().await.unwrap();
        assert_eq!(loaded, Some(wallpaper_path.to_string()));
    }

    #[tokio::test]
    async fn test_missing_wallpaper_file() {
        let temp_dir = tempdir().unwrap();
        let state_file = temp_dir.path().join("wallpaper-state");

        let restore = WallpaperRestore::with_state_file(&state_file);

        // Save state for non-existent file
        let fake_path = "/non/existent/wallpaper.jpg";
        restore.save_wallpaper_state(fake_path).await.unwrap();

        // Try to load - should return None because file doesn't exist
        let loaded = restore.get_saved_wallpaper().await.unwrap();
        assert_eq!(loaded, None);
    }

    #[tokio::test]
    async fn test_clear_state() {
        let temp_dir = tempdir().unwrap();
        let state_file = temp_dir.path().join("wallpaper-state");

        let restore = WallpaperRestore::with_state_file(&state_file);

        // Save some state
        restore.save_wallpaper_state("/some/path").await.unwrap();
        assert!(restore.has_saved_state());

        // Clear state
        restore.clear_state().await.unwrap();
        assert!(!restore.has_saved_state());
    }

    #[test]
    fn test_wallpaper_search_dirs() {
        let dirs = WallpaperRestore::get_wallpaper_search_dirs();
        assert!(!dirs.is_empty());

        // Should include some common directories
        let dir_strings: Vec<String> = dirs
            .iter()
            .map(|d| d.to_string_lossy().to_string())
            .collect();

        let has_usr_share = dir_strings.iter().any(|d| d.contains("/usr/share"));
        assert!(has_usr_share);
    }
}
