// SPDX-FileCopyrightText: 2026 iliyaj
// SPDX-License-Identifier: GPL-3.0-or-later

//! Coordinates matching and applying display profiles when outputs change.

pub mod executor;
pub mod matcher;

pub use executor::ProfileExecutor;
pub use matcher::ProfileMatcher;

use crate::config::Config;
use crate::wayland::OutputManager;
use anyhow::Result;
use tracing::{debug, info};

pub struct ProfileManager {
    matcher: ProfileMatcher,
    executor: ProfileExecutor,
    current_profile: Option<String>,
    /// Retained across transient no-match states while outputs are rebuilt.
    last_applied_profile: Option<String>,
    wallpaper_restored: bool,
}

impl ProfileManager {
    /// Create a new profile manager
    pub fn new() -> Self {
        Self {
            matcher: ProfileMatcher::new(),
            executor: ProfileExecutor::new(),
            current_profile: None,
            last_applied_profile: None,
            wallpaper_restored: false,
        }
    }

    /// Create a new profile manager with awww support
    pub async fn with_awww() -> Result<Self> {
        let mut executor = ProfileExecutor::with_awww().await?;

        let wallpaper_restored = executor.restore_wallpaper().await.unwrap_or(false);

        Ok(Self {
            matcher: ProfileMatcher::new(),
            executor,
            current_profile: None,
            last_applied_profile: None,
            wallpaper_restored,
        })
    }

    /// Find and apply the best matching profile for current outputs
    pub async fn match_and_apply(
        &mut self,
        config: &Config,
        output_manager: &mut OutputManager,
    ) -> Result<Option<String>> {
        let heads = output_manager.get_heads();
        info!("Matching profile for {} connected outputs", heads.len());

        if let Some(profile) = self.matcher.find_best_match(config, heads) {
            info!("Found matching profile: {}", profile.name);

            if let Some(ref current) = self.current_profile {
                if current == &profile.name {
                    debug!("Profile '{}' is already active", profile.name);
                    return Ok(Some(profile.name.clone()));
                }
            }

            // Rebuilt outputs need the saved wallpaper, not a new random profile wallpaper.
            let reactivating = self.last_applied_profile.as_deref() == Some(profile.name.as_str());
            if reactivating {
                self.executor.restore_saved_wallpaper().await;
            }

            // Preserve saved state even when restoration fails.
            let skip_wallpaper = self.wallpaper_restored || reactivating;

            if self
                .executor
                .apply_profile(profile, output_manager, skip_wallpaper)
                .await?
            {
                self.current_profile = Some(profile.name.clone());
                self.last_applied_profile = Some(profile.name.clone());
                self.wallpaper_restored = false;
                return Ok(Some(profile.name.clone()));
            }
        } else {
            info!("No matching profile found for current output configuration");
            self.current_profile = None;
        }

        Ok(None)
    }

    /// Get the currently active profile name
    pub fn current_profile(&self) -> Option<&String> {
        self.current_profile.as_ref()
    }

    /// Force apply a specific profile by name
    pub async fn apply_profile_by_name(
        &mut self,
        config: &Config,
        profile_name: &str,
        output_manager: &mut OutputManager,
    ) -> Result<bool> {
        if let Some(profile) = config.profiles.iter().find(|p| p.name == profile_name) {
            info!("Force applying profile: {}", profile_name);

            if self
                .executor
                .apply_profile(profile, output_manager, false)
                .await?
            {
                self.current_profile = Some(profile.name.clone());
                self.last_applied_profile = Some(profile.name.clone());
                return Ok(true);
            }
        } else {
            anyhow::bail!("Profile '{}' not found in configuration", profile_name);
        }

        Ok(false)
    }

    /// Clears the active profile while retaining restoration context for rebuilt outputs.
    pub fn invalidate_current_profile(&mut self) {
        debug!("Invalidating active profile so the next match re-applies it");
        self.current_profile = None;
    }

    /// Save current wallpaper state for restoration
    pub async fn save_current_wallpaper(&mut self) -> Result<bool> {
        self.executor.save_current_wallpaper().await
    }
}

impl Default for ProfileManager {
    fn default() -> Self {
        Self::new()
    }
}
