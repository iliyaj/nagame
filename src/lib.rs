// SPDX-FileCopyrightText: 2026 iliyaj
// SPDX-License-Identifier: GPL-3.0-or-later

//! Implements the nagame daemon that manages Wayland displays and wallpapers.

use anyhow::{Context, Result};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use tokio::io::unix::AsyncFd;
use tokio::signal;
use tokio::sync::mpsc;
use tracing::{info, warn};

pub mod awww;
pub mod config;
pub mod ipc;
mod preview;
pub mod profile;
pub mod wayland;

use config::{persistence, Config};
use preview::{Completion, ConfirmRequest, PendingPreview, PreviewState, RevertRequest};
use profile::matcher::resolve_profile_outputs;
use profile::ProfileManager;
use wayland::{HeadConfiguration, OutputManager, OutputMode};

/// Safety-net interval for missed Wayland events.
const HEARTBEAT_SECS: u64 = 60;

/// Retry tick while the compositor is not up yet
const RECONNECT_SECS: u64 = 1;

/// Coalescing window for the event burst a single config save produces
const CONFIG_DEBOUNCE: tokio::time::Duration = tokio::time::Duration::from_millis(300);

const DISPLAY_PREVIEW_SECS: u64 = 15;

struct PreviewPayload {
    before: Vec<(String, HeadConfiguration)>,
    config_revision: String,
    mode: String,
    output_index: usize,
    profile: String,
    responses: mpsc::UnboundedSender<ipc::ServerEvent>,
}

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
    config_revision: String,
    output_manager: OutputManager,
    profile_manager: ProfileManager,
    previews: PreviewState<PreviewPayload>,
}

impl NagameDaemon {
    /// Create a new nagame daemon instance
    pub async fn new(config_path: PathBuf) -> Result<Self> {
        info!("Initializing nagame daemon");

        // Resolve a dotfiles symlink once so both persistence and file watching follow its target.
        let config_path = tokio::fs::canonicalize(&config_path)
            .await
            .with_context(|| format!("Failed to resolve config file: {}", config_path.display()))?;
        let (config_source, config_revision) =
            persistence::read_with_revision(&config_path).await?;
        let config = Config::from_toml(&config_source)
            .with_context(|| format!("Failed to parse config file: {}", config_path.display()))?;
        info!("Loaded {} profiles from config", config.profiles.len());

        let mut output_manager = OutputManager::new();
        output_manager.initialize().await?;

        let profile_manager = ProfileManager::with_awww().await?;

        Ok(Self {
            config_path,
            config,
            config_revision,
            output_manager,
            profile_manager,
            previews: PreviewState::default(),
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
        let (mut ipc_requests, _ipc_socket) = ipc::start_server().await?;

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

            let preview_deadline = self.previews.deadline();
            let preview_timeout = async move {
                match preview_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            };

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
                            let disk_revision = persistence::read_with_revision(&self.config_path)
                                .await
                                .map(|(_, revision)| revision);
                            if disk_revision.as_ref().is_ok_and(|revision| {
                                revision == &self.config_revision
                            }) {
                                continue;
                            }
                            if self.previews.is_pending() {
                                let _ = self.revert_pending_preview("configuration_changed").await;
                            }
                            info!("Config file changed - reloading");
                            if let Err(e) = self.reload_config().await {
                                warn!("Failed to reload config: {}", e);
                            }
                        }
                    }
                }

                request = ipc_requests.recv() => {
                    if let Some(request) = request {
                        self.handle_display_request(request).await;
                    }
                }

                _ = preview_timeout => {
                    if let Some(preview) = self.previews.take_if_expired(tokio::time::Instant::now()) {
                        if let Err(error) = self.restore_preview(preview, "timeout").await {
                            warn!("Failed to revert expired display preview: {}", error);
                        }
                    }
                }

                // Handle shutdown signals
                _ = sigterm.recv() => {
                    if let Err(error) = self.revert_pending_preview("daemon_shutdown").await {
                        warn!("Failed to revert display preview before shutdown: {}", error);
                    }
                    info!("Received SIGTERM - saving current wallpaper state before shutdown");
                    if let Err(e) = self.profile_manager.save_current_wallpaper().await {
                        warn!("Failed to save wallpaper state: {}", e);
                    }
                    info!("Shutting down gracefully");
                    break;
                }
                _ = sigint.recv() => {
                    if let Err(error) = self.revert_pending_preview("daemon_shutdown").await {
                        warn!("Failed to revert display preview before shutdown: {}", error);
                    }
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

    async fn handle_display_request(&mut self, incoming: ipc::Incoming) {
        let request = match incoming {
            ipc::Incoming::Request(request) => request,
            ipc::Incoming::Disconnected(client_id) => {
                if let Some(preview) = self.previews.take_for_client(client_id) {
                    if let Err(error) = self.restore_preview(preview, "client_disconnected").await {
                        warn!("Failed to revert disconnected display preview: {}", error);
                    }
                }
                return;
            }
        };

        match request.request {
            ipc::ClientRequest::Outputs => {
                let outputs = self
                    .output_manager
                    .get_heads()
                    .into_iter()
                    .map(ipc::DisplayOutput::from)
                    .collect();
                let _ = request.responses.send(ipc::ServerEvent::Outputs {
                    outputs,
                    active_profile: self.profile_manager.current_profile().cloned(),
                    revision: self.config_revision.clone(),
                    supported: self.output_manager.is_available(),
                });
            }
            ipc::ClientRequest::Preview {
                output,
                mode_id,
                profile,
                revision,
            } => {
                if self.previews.is_pending() {
                    let _ = request.responses.send(ipc::ServerEvent::error(
                        "preview_busy",
                        "Another display preview is already pending",
                    ));
                    return;
                }

                let disk_revision = match persistence::read_with_revision(&self.config_path).await {
                    Ok((_, revision)) => revision,
                    Err(error) => {
                        let _ = request.responses.send(ipc::ServerEvent::error(
                            "config_read_failed",
                            error.to_string(),
                        ));
                        return;
                    }
                };
                if revision != self.config_revision || revision != disk_revision {
                    let _ = request.responses.send(ipc::ServerEvent::error(
                        "config_conflict",
                        "The display configuration changed elsewhere. Refresh and try again.",
                    ));
                    return;
                }
                if self.profile_manager.current_profile().map(String::as_str)
                    != Some(profile.as_str())
                {
                    let _ = request.responses.send(ipc::ServerEvent::error(
                        "profile_changed",
                        "The active display profile changed. Refresh and try again.",
                    ));
                    return;
                }

                let (output_index, mode) =
                    match self.preview_persistence_target(&profile, &output, &mode_id) {
                        Ok(target) => target,
                        Err(error) => {
                            let _ = request.responses.send(ipc::ServerEvent::error(
                                "invalid_profile_target",
                                error.to_string(),
                            ));
                            return;
                        }
                    };

                let before = match self.output_manager.snapshot_configuration() {
                    Ok(configuration) => configuration,
                    Err(error) => {
                        let _ = request.responses.send(ipc::ServerEvent::error(
                            "invalid_live_state",
                            error.to_string(),
                        ));
                        return;
                    }
                };
                let candidate = match self.output_manager.candidate_with_mode(&output, &mode_id) {
                    Ok(configuration) => configuration,
                    Err(error) => {
                        let _ = request
                            .responses
                            .send(ipc::ServerEvent::error("invalid_mode", error.to_string()));
                        return;
                    }
                };
                if let Err(error) = self.output_manager.test_configuration(&candidate).await {
                    let _ = request.responses.send(ipc::ServerEvent::error(
                        "compositor_test_failed",
                        error.to_string(),
                    ));
                    return;
                }
                if let Err(error) = self.output_manager.apply_configuration(&candidate).await {
                    let _ = request.responses.send(ipc::ServerEvent::error(
                        "compositor_apply_failed",
                        error.to_string(),
                    ));
                    return;
                }

                let deadline = tokio::time::Instant::now()
                    + tokio::time::Duration::from_secs(DISPLAY_PREVIEW_SECS);
                let transaction_id = self
                    .previews
                    .start(
                        request.client_id,
                        deadline,
                        PreviewPayload {
                            before,
                            config_revision: revision,
                            mode,
                            output_index,
                            profile,
                            responses: request.responses.clone(),
                        },
                    )
                    .expect("preview state changed while handling one request");

                if request
                    .responses
                    .send(ipc::ServerEvent::PreviewStarted {
                        transaction_id: transaction_id.clone(),
                        remaining_ms: DISPLAY_PREVIEW_SECS * 1000,
                    })
                    .is_err()
                {
                    if let RevertRequest::Restore(preview) =
                        self.previews.request_revert(&transaction_id)
                    {
                        if let Err(error) =
                            self.restore_preview(preview, "client_disconnected").await
                        {
                            warn!("Failed to recover abandoned display preview: {}", error);
                        }
                    }
                }
            }
            ipc::ClientRequest::Confirm { transaction_id } => {
                match self.previews.request_confirm(&transaction_id) {
                    ConfirmRequest::Persist(preview) => match self.persist_preview(preview).await {
                        Ok(revision) => {
                            let _ = request.responses.send(ipc::ServerEvent::ConfirmCompleted {
                                transaction_id,
                                revision,
                            });
                        }
                        Err((code, message)) => {
                            let _ = request
                                .responses
                                .send(ipc::ServerEvent::error(code, message));
                        }
                    },
                    ConfirmRequest::AlreadyConfirmed => {
                        let _ = request.responses.send(ipc::ServerEvent::ConfirmCompleted {
                            transaction_id,
                            revision: self.config_revision.clone(),
                        });
                    }
                    ConfirmRequest::NoPending => {
                        let _ = request.responses.send(ipc::ServerEvent::error(
                            "no_pending_preview",
                            "There is no pending display preview",
                        ));
                    }
                    ConfirmRequest::Mismatch => {
                        let _ = request.responses.send(ipc::ServerEvent::error(
                            "transaction_mismatch",
                            "The display preview transaction no longer matches",
                        ));
                    }
                }
            }
            ipc::ClientRequest::Revert { transaction_id } => {
                match self.previews.request_revert(&transaction_id) {
                    RevertRequest::Restore(preview) => {
                        match self.restore_preview(preview, "manual").await {
                            Ok(()) => {
                                let _ = request
                                    .responses
                                    .send(ipc::ServerEvent::RevertCompleted { transaction_id });
                            }
                            Err(error) => {
                                let _ = request.responses.send(ipc::ServerEvent::error(
                                    "revert_failed",
                                    error.to_string(),
                                ));
                            }
                        }
                    }
                    RevertRequest::AlreadyReverted => {
                        let _ = request
                            .responses
                            .send(ipc::ServerEvent::RevertCompleted { transaction_id });
                    }
                    RevertRequest::NoPending => {
                        let _ = request.responses.send(ipc::ServerEvent::error(
                            "no_pending_preview",
                            "There is no pending display preview",
                        ));
                    }
                    RevertRequest::Mismatch => {
                        let _ = request.responses.send(ipc::ServerEvent::error(
                            "transaction_mismatch",
                            "The display preview transaction no longer matches",
                        ));
                    }
                }
            }
        }
    }

    async fn revert_pending_preview(&mut self, reason: &str) -> Result<()> {
        let Some(preview) = self.previews.take_pending() else {
            return Ok(());
        };
        self.restore_preview(preview, reason).await
    }

    fn cancel_pending_preview(&mut self, reason: &str) {
        let Some(preview) = self.previews.take_pending() else {
            return;
        };
        let transaction_id = preview.id;
        let _ = preview
            .payload
            .responses
            .send(ipc::ServerEvent::PreviewReverted {
                transaction_id: transaction_id.clone(),
                reason: reason.to_string(),
            });
        self.previews.complete(transaction_id, Completion::Reverted);
    }

    fn preview_persistence_target(
        &self,
        profile_name: &str,
        output_name: &str,
        mode_id: &str,
    ) -> Result<(usize, String)> {
        let profile = self
            .config
            .profiles
            .iter()
            .find(|profile| profile.name == profile_name)
            .with_context(|| format!("Profile '{}' no longer exists", profile_name))?;
        let heads = self.output_manager.get_heads();
        let output_index = resolve_profile_outputs(profile, &heads)
            .and_then(|outputs| {
                outputs
                    .into_iter()
                    .enumerate()
                    .find_map(|(index, (_, connector))| (connector == output_name).then_some(index))
            })
            .with_context(|| {
                format!(
                    "Output '{}' is not part of active profile '{}'",
                    output_name, profile_name
                )
            })?;
        let mode = self
            .output_manager
            .get_head(output_name)
            .and_then(|head| head.modes.iter().find(|mode| ipc::mode_id(mode) == mode_id))
            .map(Self::persistent_mode)
            .with_context(|| {
                format!(
                    "Mode '{}' is not advertised by output '{}'",
                    mode_id, output_name
                )
            })?;
        Ok((output_index, mode))
    }

    fn persistent_mode(mode: &OutputMode) -> String {
        let whole = mode.refresh_mhz / 1000;
        let fraction = mode.refresh_mhz.rem_euclid(1000);
        if fraction == 0 {
            format!("{}x{}@{}", mode.width, mode.height, whole)
        } else {
            let refresh = format!("{}.{:03}", whole, fraction)
                .trim_end_matches('0')
                .to_string();
            format!("{}x{}@{}", mode.width, mode.height, refresh)
        }
    }

    async fn persist_preview(
        &mut self,
        preview: PendingPreview<PreviewPayload>,
    ) -> std::result::Result<String, (&'static str, String)> {
        let transaction_id = preview.id.clone();
        let (source, revision) = match persistence::read_with_revision(&self.config_path).await {
            Ok(result) => result,
            Err(error) => {
                let message = error.to_string();
                if let Err(revert_error) = self.restore_preview(preview, "persistence_failed").await
                {
                    return Err((
                        "revert_failed",
                        format!("{message}; the preview also failed to revert: {revert_error}"),
                    ));
                }
                return Err(("config_read_failed", message));
            }
        };

        if revision != preview.payload.config_revision {
            if let Err(error) = self.restore_preview(preview, "configuration_changed").await {
                return Err(("revert_failed", error.to_string()));
            }
            return Err((
                "config_conflict",
                "The display configuration changed elsewhere. The newer configuration won."
                    .to_string(),
            ));
        }

        let (updated_source, updated_config) = match persistence::update_output_mode(
            &source,
            &preview.payload.profile,
            preview.payload.output_index,
            &preview.payload.mode,
        ) {
            Ok(updated) => updated,
            Err(error) => {
                let message = error.to_string();
                if let Err(revert_error) = self.restore_preview(preview, "persistence_failed").await
                {
                    return Err((
                        "revert_failed",
                        format!("{message}; the preview also failed to revert: {revert_error}"),
                    ));
                }
                return Err(("config_update_failed", message));
            }
        };

        if let Err(error) = persistence::atomic_write(&self.config_path, &updated_source).await {
            let message = error.to_string();
            if let Err(revert_error) = self.restore_preview(preview, "persistence_failed").await {
                return Err((
                    "revert_failed",
                    format!("{message}; the preview also failed to revert: {revert_error}"),
                ));
            }
            return Err(("config_write_failed", message));
        }

        self.config_revision = persistence::revision(&updated_source);
        self.config = updated_config;
        let _ = preview
            .payload
            .responses
            .send(ipc::ServerEvent::PreviewConfirmed {
                transaction_id: transaction_id.clone(),
                revision: self.config_revision.clone(),
            });
        self.previews
            .complete(transaction_id, Completion::Confirmed);
        Ok(self.config_revision.clone())
    }

    async fn restore_preview(
        &mut self,
        mut preview: PendingPreview<PreviewPayload>,
        reason: &str,
    ) -> Result<()> {
        if let Err(error) = self
            .output_manager
            .apply_configuration(&preview.payload.before)
            .await
        {
            preview.deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(1);
            let _ = preview
                .payload
                .responses
                .send(ipc::ServerEvent::error("revert_failed", error.to_string()));
            self.previews.retry(preview);
            return Err(error);
        }

        let transaction_id = preview.id;
        let _ = preview
            .payload
            .responses
            .send(ipc::ServerEvent::PreviewReverted {
                transaction_id: transaction_id.clone(),
                reason: reason.to_string(),
            });
        self.previews.complete(transaction_id, Completion::Reverted);
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

        // The old snapshot cannot be replayed across a changed output topology.
        // Cancel its lease and let the durable profile matcher establish the new state.
        if self.previews.is_pending() {
            self.cancel_pending_preview("outputs_changed");
        }

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
        let (source, revision) = persistence::read_with_revision(&self.config_path).await?;
        self.config = Config::from_toml(&source).with_context(|| {
            format!(
                "Failed to parse config file: {}",
                self.config_path.display()
            )
        })?;
        self.config_revision = revision;
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
