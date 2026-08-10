// SPDX-FileCopyrightText: 2026 iliyaj
// SPDX-License-Identifier: GPL-3.0-or-later

//! Implements the nagame daemon that manages Wayland displays and wallpapers.

use anyhow::Result;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use tokio::io::unix::AsyncFd;
use tokio::signal;
use tracing::{info, warn};

pub mod awww;
pub mod config;
pub mod profile;
pub mod wayland;

use config::Config;
use profile::ProfileManager;
use wayland::OutputManager;

/// Safety-net interval for missed Wayland events.
const HEARTBEAT_SECS: u64 = 60;

/// Retry tick while the compositor is not up yet
const RECONNECT_SECS: u64 = 1;

/// Coalescing window for the event burst a single config save produces
const CONFIG_DEBOUNCE: tokio::time::Duration = tokio::time::Duration::from_millis(300);

/// The last complete output configuration the daemon acted on
struct OutputsSeen {
    /// Configuration generation, bumped by every `done` and every head removal
    generation: u64,
    /// Sorted (name, head id) pairs
    outputs: Vec<(String, u32)>,
}

/// Main nagame daemon
pub struct NagameDaemon {
    config_path: PathBuf,
    config: Config,
    output_manager: OutputManager,
    profile_manager: ProfileManager,
}

impl NagameDaemon {
    /// Create a new nagame daemon instance
    pub async fn new(config_path: PathBuf) -> Result<Self> {
        info!("Initializing nagame daemon");

        let config = Config::load(&config_path).await?;
        info!("Loaded {} profiles from config", config.profiles.len());

        let mut output_manager = OutputManager::new();
        output_manager.initialize().await?;

        let profile_manager = ProfileManager::with_awww().await?;

        Ok(Self {
            config_path,
            config,
            output_manager,
            profile_manager,
        })
    }

    /// Run the daemon main loop
    pub async fn run(mut self) -> Result<()> {
        info!("Starting daemon main loop");

        info!("Discovering connected outputs");
        self.output_manager.refresh_outputs().await?;

        info!("Matching and applying initial profile");
        match self
            .profile_manager
            .match_and_apply(&self.config, &mut self.output_manager)
            .await
        {
            Ok(Some(profile_name)) => {
                info!("Successfully applied initial profile: {}", profile_name);
            }
            Ok(None) => {
                warn!("No profile applied for current output configuration");
            }
            Err(e) => {
                warn!("Failed to apply initial profile: {}", e);
            }
        }

        let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())?;
        let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt())?;

        // The watcher binding must outlive the loop because dropping it ends the file watch.
        let (mut config_watcher, _watcher) = self.setup_config_watcher().await?;

        // Drive output updates from complete configurations announced by Wayland.
        let mut readable = Self::watch_connection(&self.output_manager);

        // The heartbeat recovers missed events or a compositor that starts late.
        let mut heartbeat = Self::heartbeat(if readable.is_some() {
            HEARTBEAT_SECS
        } else {
            RECONNECT_SECS
        });

        // Head IDs reveal same-name outputs rebuilt after suspend or DPMS.
        let mut seen = OutputsSeen {
            generation: self.output_manager.configuration_generation(),
            outputs: Self::output_identities(&self.output_manager),
        };

        loop {
            // The compositor may not have been up when we started; adopt its fd as soon as it is
            if readable.is_none() {
                readable = Self::watch_connection(&self.output_manager);
                if readable.is_some() {
                    info!("Wayland connection established - tracking outputs by event");
                    heartbeat = Self::heartbeat(HEARTBEAT_SECS);

                    match self
                        .profile_manager
                        .match_and_apply(&self.config, &mut self.output_manager)
                        .await
                    {
                        Ok(Some(name)) => {
                            info!("Applied profile after Wayland initialization: {}", name)
                        }
                        Ok(None) => warn!("No profile applied after Wayland initialization"),
                        Err(e) => warn!(
                            "Failed to apply profile after Wayland initialization: {}",
                            e
                        ),
                    }
                }
            }

            // Drain queued events before waiting because they do not make the fd readable.
            match self.output_manager.flush_before_wait() {
                Ok(dispatched) if dispatched > 0 => {
                    if self.apply_output_changes(&mut seen).await {
                        break;
                    }
                    continue;
                }
                Ok(_) => {}
                Err(e) => {
                    if self.wayland_connection_lost(&e).await {
                        break;
                    }
                }
            }

            tokio::select! {
                guard = readable.as_ref().unwrap().readable(), if readable.is_some() => {
                    match guard {
                        Ok(mut guard) => {
                            guard.clear_ready();
                            match self.output_manager.pump_events() {
                                Ok(dispatched) => {
                                    if dispatched > 0 && self.apply_output_changes(&mut seen).await {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    if self.wayland_connection_lost(&e).await {
                                        break;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Wayland connection fd is no longer pollable: {}", e);
                            break;
                        }
                    }
                }

                _ = heartbeat.tick() => {
                    if let Err(e) = self.output_manager.refresh_outputs().await {
                        if self.wayland_connection_lost(&e).await {
                            break;
                        }
                        continue;
                    }

                    if self.apply_output_changes(&mut seen).await {
                        break;
                    }
                }

                // Handle config file changes
                event = config_watcher.recv() => {
                    if let Some(event) = event {
                        // Only reload if the event affects our specific config file
                        let mut should_reload = event.paths.iter().any(|path| {
                            path == &self.config_path
                        });

                        // One save emits several events, so collapse the burst into one reload.
                        while let Ok(Some(next)) =
                            tokio::time::timeout(CONFIG_DEBOUNCE, config_watcher.recv()).await
                        {
                            should_reload |= next.paths.iter().any(|path| {
                                path == &self.config_path
                            });
                        }

                        if should_reload {
                            info!("Config file changed - reloading");
                            if let Err(e) = self.reload_config().await {
                                warn!("Failed to reload config: {}", e);
                            }
                        }
                    }
                }

                // Handle shutdown signals
                _ = sigterm.recv() => {
                    info!("Received SIGTERM - saving current wallpaper state before shutdown");
                    if let Err(e) = self.profile_manager.save_current_wallpaper().await {
                        warn!("Failed to save wallpaper state: {}", e);
                    }
                    info!("Shutting down gracefully");
                    break;
                }
                _ = sigint.recv() => {
                    info!("Received SIGINT - saving current wallpaper state before shutdown");
                    if let Err(e) = self.profile_manager.save_current_wallpaper().await {
                        warn!("Failed to save wallpaper state: {}", e);
                    }
                    info!("Shutting down gracefully");
                    break;
                }
            }
        }

        info!("Daemon shutdown complete");
        Ok(())
    }

    /// Timer used only as a safety net, never as the change-detection path
    fn heartbeat(secs: u64) -> tokio::time::Interval {
        let mut timer = tokio::time::interval(tokio::time::Duration::from_secs(secs));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        timer
    }

    /// Start watching the Wayland connection fd, if the client is up yet
    fn watch_connection(output_manager: &OutputManager) -> Option<AsyncFd<OwnedFd>> {
        match output_manager.connection_fd()? {
            Ok(fd) => match AsyncFd::new(fd) {
                Ok(async_fd) => Some(async_fd),
                Err(e) => {
                    warn!("Failed to watch Wayland connection fd: {}", e);
                    None
                }
            },
            Err(e) => {
                warn!("Failed to duplicate Wayland connection fd: {}", e);
                None
            }
        }
    }

    /// True if the daemon should exit so systemd can restart it on a fresh connection
    async fn wayland_connection_lost(&mut self, error: &anyhow::Error) -> bool {
        let message = error.to_string();
        if !message.contains("Broken pipe") && !message.contains("Connection reset") {
            warn!("Wayland error: {}", message);
            return false;
        }

        info!(
            "Wayland connection lost ({}), saving wallpaper and exiting for clean restart",
            message
        );
        if let Err(e) = self.profile_manager.save_current_wallpaper().await {
            warn!("Failed to save wallpaper state before restart: {}", e);
        }
        true
    }

    /// Applies complete output updates and reports whether the daemon should exit.
    async fn apply_output_changes(&mut self, seen: &mut OutputsSeen) -> bool {
        let generation = self.output_manager.configuration_generation();
        if generation == seen.generation {
            return false;
        }
        seen.generation = generation;

        // Restart rather than attempting to repair inconsistent protocol state.
        if self
            .output_manager
            .get_heads()
            .iter()
            .any(|h| h.enabled && h.modes.is_empty())
        {
            warn!("Head has 0 modes - protocol state corrupted, restarting for fresh Wayland connection");
            if let Err(e) = self.profile_manager.save_current_wallpaper().await {
                warn!("Failed to save wallpaper state before restart: {}", e);
            }
            info!("Exiting for clean restart");
            return true;
        }

        let current_outputs = Self::output_identities(&self.output_manager);
        if current_outputs == seen.outputs {
            return false;
        }

        let old_count = seen.outputs.len();
        let new_count = current_outputs.len();

        // New IDs with unchanged names indicate outputs rebuilt after suspend or DPMS.
        let rebuilt = new_count > 0
            && Self::output_names(&current_outputs) == Self::output_names(&seen.outputs);

        if new_count == 0 {
            info!("All outputs disconnected");
        } else if old_count == 0 {
            info!(
                "Output connected: {:?} - applying profile",
                Self::output_names(&current_outputs)
            );
        } else if rebuilt {
            info!(
                "Outputs re-created under the same names {:?} - re-applying profile",
                Self::output_names(&current_outputs)
            );
        } else {
            info!(
                "Output configuration changed: {:?} -> {:?}",
                Self::output_names(&seen.outputs),
                Self::output_names(&current_outputs)
            );
        }

        seen.outputs = current_outputs;

        if new_count == 0 {
            return false;
        }

        // Reapply even the same profile because the old outputs no longer exist.
        self.profile_manager.invalidate_current_profile();

        match self
            .profile_manager
            .match_and_apply(&self.config, &mut self.output_manager)
            .await
        {
            Ok(Some(profile_name)) => {
                info!("Applied profile '{}' after output change", profile_name)
            }
            Ok(None) => warn!("No profile applied for connected outputs"),
            Err(e) => warn!("Failed to apply profile after output change: {}", e),
        }

        false
    }

    /// Sorted (name, head id) pairs identifying the currently announced outputs
    fn output_identities(output_manager: &OutputManager) -> Vec<(String, u32)> {
        let mut identities: Vec<(String, u32)> = output_manager
            .get_heads()
            .iter()
            .map(|h| (h.name.clone(), h.id))
            .collect();
        identities.sort();
        identities
    }

    /// Drop the head ids so two output sets can be compared by name alone
    fn output_names(identities: &[(String, u32)]) -> Vec<&str> {
        identities.iter().map(|(name, _)| name.as_str()).collect()
    }

    /// Reload configuration from file
    async fn reload_config(&mut self) -> Result<()> {
        info!("Reloading configuration");
        self.config = Config::load(&self.config_path).await?;
        info!("Reloaded {} profiles", self.config.profiles.len());

        // Rematch existing outputs without refreshing physical display state.
        self.profile_manager.invalidate_current_profile();

        match self
            .profile_manager
            .match_and_apply(&self.config, &mut self.output_manager)
            .await
        {
            Ok(Some(profile_name)) => {
                info!("Applied profile after config reload: {}", profile_name);
            }
            Ok(None) => {
                warn!("No profile applied after config reload");
            }
            Err(e) => {
                warn!("Failed to apply profile after config reload: {}", e);
            }
        }

        Ok(())
    }

    /// Validate configuration without applying changes
    pub async fn validate_config(&self) -> Result<()> {
        info!(
            "Validating configuration with {} profiles",
            self.config.profiles.len()
        );

        self.config.validate_environment().await?;
        info!("✅ Configuration validation completed");
        Ok(())
    }

    /// Set up file system watcher for config changes
    async fn setup_config_watcher(
        &self,
    ) -> Result<(
        tokio::sync::mpsc::Receiver<notify::Event>,
        notify::RecommendedWatcher,
    )> {
        use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
        use tokio::sync::mpsc;

        let (tx, rx) = mpsc::channel(100);

        let config_path = self.config_path.clone();
        let mut watcher = RecommendedWatcher::new(
            move |res: notify::Result<notify::Event>| {
                if let Ok(event) = res {
                    if let Err(e) = tx.try_send(event) {
                        warn!("Failed to send config change event: {}", e);
                    }
                }
            },
            Config::default(),
        )?;

        // Watch the config file's parent directory
        if let Some(parent) = config_path.parent() {
            watcher.watch(parent, RecursiveMode::NonRecursive)?;
        }

        // The caller owns the watcher because dropping it stops the file watch.
        Ok((rx, watcher))
    }
}
