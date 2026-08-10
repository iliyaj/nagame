// SPDX-FileCopyrightText: 2026 iliyaj
// SPDX-License-Identifier: GPL-3.0-or-later

//! Implements wlr-output-management protocol state, events, and configuration requests.

use anyhow::{anyhow, Result};
use std::collections::HashMap;
use tracing::{debug, error, info, warn};
use wayland_client::{
    backend::ObjectId,
    protocol::{wl_output, wl_registry},
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};

/// Represents a display output with its properties and available modes
#[derive(Debug, Clone)]
pub struct OutputHead {
    /// Monotonic ID that changes when a same-name output is rebuilt.
    pub id: u32,
    /// Output name (e.g., "eDP-1", "HDMI-A-2")
    pub name: String,
    /// Human-readable description
    pub description: String,
    /// Physical dimensions in millimeters
    pub physical_size: Option<(i32, i32)>,
    /// Position in global coordinate space
    pub position: Option<(i32, i32)>,
    /// Current transform/rotation
    pub transform: wl_output::Transform,
    /// Scaling factor
    pub scale: f64,
    /// Whether output is enabled
    pub enabled: bool,
    /// Available display modes
    pub modes: Vec<OutputMode>,
    /// Current active mode
    pub current_mode: Option<OutputMode>,
    /// Preferred mode (usually highest resolution)
    pub preferred_mode: Option<OutputMode>,
    /// Make and model information
    pub make: String,
    pub model: String,
    /// Serial number
    pub serial_number: String,
    /// Adaptive sync capability
    pub adaptive_sync: Option<bool>,
}

/// Represents a display mode (resolution + refresh rate)
#[derive(Debug, Clone, PartialEq)]
pub struct OutputMode {
    /// Width in pixels
    pub width: i32,
    /// Height in pixels
    pub height: i32,
    /// Refresh rate in millihertz
    pub refresh_mhz: i32,
    /// Whether this is the preferred mode
    pub preferred: bool,
}

impl OutputMode {
    /// Get refresh rate in Hz
    pub fn refresh_hz(&self) -> f64 {
        self.refresh_mhz as f64 / 1000.0
    }

    /// Format mode as string (e.g., "1920x1080@60")
    pub fn format(&self) -> String {
        format!("{}x{}@{:.0}", self.width, self.height, self.refresh_hz())
    }
}

/// Configuration for a head (output)
#[derive(Debug, Clone)]
pub struct HeadConfiguration {
    pub enabled: bool,
    pub mode: Option<OutputMode>,
    pub position: Option<(i32, i32)>,
    pub transform: Option<wl_output::Transform>,
    pub scale: Option<f64>,
    pub adaptive_sync: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigurationOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

/// State for managing Wayland protocol interactions
pub struct WaylandState {
    /// Output manager global
    pub output_manager: Option<ZwlrOutputManagerV1>,
    /// Map of output heads
    pub heads: HashMap<String, OutputHead>,
    /// Registry for global discovery
    pub registry: Option<wl_registry::WlRegistry>,
    /// Active configuration object
    pub active_config: Option<ZwlrOutputConfigurationV1>,
    /// Last serial number from done event
    pub last_serial: Option<u32>,
    /// Result reported for the most recently submitted configuration.
    configuration_outcome: Option<ConfigurationOutcome>,
    /// Temporary storage for heads being built (keyed by object ID)
    pub pending_heads: HashMap<u32, OutputHead>,
    /// Temporary storage for modes being built (keyed by object ID)
    pub pending_modes: HashMap<u32, OutputMode>,
    /// Map mode ObjectId to the head they belong to (mode ObjectId → head_id)
    pub mode_to_head: HashMap<ObjectId, u32>,
    /// Map head_id to head name (for looking up heads after mode completion)
    pub head_id_to_name: HashMap<u32, String>,
    /// Wayland protocol proxies for heads (needed for configuration)
    pub head_proxies: HashMap<String, ZwlrOutputHeadV1>,
    /// Wayland protocol proxies for modes (needed for mode selection)
    pub mode_proxies: HashMap<u32, ZwlrOutputModeV1>,
    /// Mode metadata indexed by ID (for matching modes during configuration)
    pub mode_metadata: HashMap<u32, OutputMode>,
    /// Map mode ObjectId to mode data (for CurrentMode event)
    pub mode_obj_to_data: HashMap<ObjectId, OutputMode>,
    /// Pending current modes (head_id → mode proxy) - resolved after modes are finalized
    pub pending_current_modes: HashMap<u32, ZwlrOutputModeV1>,
    /// Incremented for each complete configuration announced by `done`.
    pub configuration_generation: u64,
}

impl WaylandState {
    pub fn new() -> Self {
        Self {
            output_manager: None,
            heads: HashMap::new(),
            registry: None,
            active_config: None,
            last_serial: None,
            configuration_outcome: None,
            pending_heads: HashMap::new(),
            pending_modes: HashMap::new(),
            mode_to_head: HashMap::new(),
            head_id_to_name: HashMap::new(),
            head_proxies: HashMap::new(),
            mode_proxies: HashMap::new(),
            mode_metadata: HashMap::new(),
            mode_obj_to_data: HashMap::new(),
            pending_current_modes: HashMap::new(),
            configuration_generation: 0,
        }
    }

    /// Promotes pending protocol state after `done` marks a configuration complete.
    pub fn finalize_pending(&mut self) {
        // Modes first: they attach to heads that are still pending
        let pending_modes: Vec<_> = self.pending_modes.drain().collect();
        for (mode_id, mode) in pending_modes {
            self.mode_metadata.insert(mode_id, mode.clone());

            let Some(mode_proxy) = self.mode_proxies.get(&mode_id) else {
                continue;
            };
            self.mode_obj_to_data.insert(mode_proxy.id(), mode.clone());

            let Some(&head_id) = self.mode_to_head.get(&mode_proxy.id()) else {
                continue;
            };
            if let Some(head) = self.head_mut(head_id) {
                debug!("Finalizing mode {} for head '{}'", mode.format(), head.name);
                head.modes.push(mode.clone());
                if mode.preferred {
                    head.preferred_mode = Some(mode);
                }
            }
        }

        // Then the current-mode pointers, now that the mode data they name exists
        let pending_current: Vec<_> = self.pending_current_modes.drain().collect();
        for (head_id, mode_proxy) in pending_current {
            let Some(mode_data) = self.mode_obj_to_data.get(&mode_proxy.id()).cloned() else {
                continue;
            };
            if let Some(head) = self.head_mut(head_id) {
                debug!(
                    "Resolved current mode for head '{}': {}",
                    head.name,
                    mode_data.format()
                );
                head.current_mode = Some(mode_data);
            }
        }

        // Finally promote the heads themselves
        let pending_heads: Vec<_> = self.pending_heads.drain().collect();
        for (head_id, head) in pending_heads {
            if head.name.is_empty() {
                continue;
            }
            debug!(
                "Finalizing head '{}' (id: {}) with {} modes",
                head.name,
                head_id,
                head.modes.len()
            );
            self.head_id_to_name.insert(head_id, head.name.clone());
            self.heads.insert(head.name.clone(), head);
        }
    }

    /// A head by id, whether it is still being built or already promoted
    fn head_mut(&mut self, head_id: u32) -> Option<&mut OutputHead> {
        if self.pending_heads.contains_key(&head_id) {
            return self.pending_heads.get_mut(&head_id);
        }
        let name = self.head_id_to_name.get(&head_id)?.clone();
        self.heads.get_mut(&name)
    }

    /// Create a new output configuration
    pub fn create_configuration(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        let manager = self
            .output_manager
            .as_ref()
            .ok_or_else(|| anyhow!("Output manager not available"))?;
        let serial = self
            .last_serial
            .ok_or_else(|| anyhow!("No completed output configuration serial is available"))?;

        let config = manager.create_configuration(serial, qh, ());
        self.active_config = Some(config);
        self.configuration_outcome = None;
        debug!("Created output configuration for serial {}", serial);
        Ok(())
    }

    /// Configure a specific output head
    pub fn configure_head(
        &self,
        head_name: &str,
        head_config: &HeadConfiguration,
        qh: &QueueHandle<Self>,
    ) -> Result<()> {
        let config = self
            .active_config
            .as_ref()
            .ok_or_else(|| anyhow!("No active configuration"))?;

        let _head = self
            .heads
            .get(head_name)
            .ok_or_else(|| anyhow!("Head '{}' not found", head_name))?;

        let head_proxy = self
            .head_proxies
            .get(head_name)
            .ok_or_else(|| anyhow!("Head proxy for '{}' not found", head_name))?;

        info!(
            "Configuring head '{}': enabled={}, mode={:?}, position={:?}",
            head_name, head_config.enabled, head_config.mode, head_config.position
        );

        if head_config.enabled {
            // Enable the head and get configuration_head object
            let config_head = config.enable_head(head_proxy, qh, ());

            // Set mode if specified
            if let Some(ref desired_mode) = head_config.mode {
                // Find the matching mode proxy
                if let Some((mode_id, mode_proxy)) = self.find_mode_proxy(head_name, desired_mode) {
                    info!(
                        "Setting mode for '{}': {} (id: {})",
                        head_name,
                        desired_mode.format(),
                        mode_id
                    );
                    config_head.set_mode(mode_proxy);
                } else {
                    warn!(
                        "Could not find matching mode proxy for {}",
                        desired_mode.format()
                    );
                }
            }

            // Set position if specified
            if let Some((x, y)) = head_config.position {
                debug!("Setting position for '{}': ({}, {})", head_name, x, y);
                config_head.set_position(x, y);
            }

            // Set transform if specified
            if let Some(t) = head_config.transform {
                debug!("Setting transform for '{}': {:?}", head_name, t);
                config_head.set_transform(t);
            }

            // Set scale if specified
            if let Some(s) = head_config.scale {
                debug!("Setting scale for '{}': {}", head_name, s);
                config_head.set_scale(s);
            }

            // Set adaptive sync if specified
            if let Some(sync) = head_config.adaptive_sync {
                debug!("Setting adaptive sync for '{}': {}", head_name, sync);
                use wayland_protocols_wlr::output_management::v1::client::zwlr_output_head_v1::AdaptiveSyncState;
                let sync_state = if sync {
                    AdaptiveSyncState::Enabled
                } else {
                    AdaptiveSyncState::Disabled
                };
                config_head.set_adaptive_sync(sync_state);
            }
        } else {
            // Disable the head
            info!("Disabling head '{}'", head_name);
            config.disable_head(head_proxy);
        }

        Ok(())
    }

    /// Find mode proxy that matches the desired mode
    fn find_mode_proxy(
        &self,
        head_name: &str,
        desired_mode: &OutputMode,
    ) -> Option<(u32, &ZwlrOutputModeV1)> {
        let head_id = self.heads.get(head_name)?.id;

        for (mode_id, metadata) in &self.mode_metadata {
            if metadata.width == desired_mode.width
                && metadata.height == desired_mode.height
                && metadata.refresh_mhz == desired_mode.refresh_mhz
            {
                if let Some(proxy) = self.mode_proxies.get(mode_id) {
                    if self.mode_to_head.get(&proxy.id()) == Some(&head_id) {
                        return Some((*mode_id, proxy));
                    }
                }
            }
        }
        None
    }

    /// Apply the current configuration
    pub fn apply_configuration(&mut self) -> Result<()> {
        if let Some(config) = &self.active_config {
            config.apply();
            debug!("Applied output configuration");
            Ok(())
        } else {
            Err(anyhow!("No active configuration to apply"))
        }
    }

    /// Test the current configuration
    pub fn test_configuration(&mut self) -> Result<()> {
        if let Some(config) = &self.active_config {
            config.test();
            debug!("Testing output configuration");
            Ok(())
        } else {
            Err(anyhow!("No active configuration to test"))
        }
    }

    /// True once the compositor has reported a verdict for the last apply/test request.
    pub fn has_configuration_outcome(&self) -> bool {
        self.configuration_outcome.is_some()
    }

    /// Return the compositor's response to the last apply/test request.
    pub fn take_configuration_result(&mut self) -> Result<()> {
        match self.configuration_outcome.take() {
            Some(ConfigurationOutcome::Succeeded) => Ok(()),
            Some(ConfigurationOutcome::Failed) => Err(anyhow!("Output configuration failed")),
            Some(ConfigurationOutcome::Cancelled) => {
                Err(anyhow!("Output configuration was cancelled"))
            }
            None => Err(anyhow!(
                "Compositor did not report an output configuration result"
            )),
        }
    }
}

impl Default for WaylandState {
    fn default() -> Self {
        Self::new()
    }
}

// Dispatch implementations for handling protocol events

impl Dispatch<wl_registry::WlRegistry, ()> for WaylandState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            debug!("Global: {} {} v{}", name, interface, version);

            if interface == ZwlrOutputManagerV1::interface().name {
                info!("Binding to wlr-output-management v{}", version);
                let manager = registry.bind::<ZwlrOutputManagerV1, _, _>(name, version, qh, ());
                state.output_manager = Some(manager);
            }
        }
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _manager: &ZwlrOutputManagerV1,
        event: zwlr_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_manager_v1::Event::Head { head } => {
                debug!("New output head announced: {:?}", head.id());
            }
            zwlr_output_manager_v1::Event::Done { serial } => {
                debug!(
                    "Output manager done - all heads announced (serial: {})",
                    serial
                );
                state.last_serial = Some(serial);
                state.finalize_pending();
                state.configuration_generation = state.configuration_generation.wrapping_add(1);
            }
            zwlr_output_manager_v1::Event::Finished => {
                warn!("Output manager finished - compositor no longer supports protocol");
                state.output_manager = None;
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(WaylandState, ZwlrOutputManagerV1, [
        0 => (ZwlrOutputHeadV1, {
            // Generate unique ID for this head
            use std::sync::atomic::{AtomicU32, Ordering};
            static COUNTER: AtomicU32 = AtomicU32::new(1);
            COUNTER.fetch_add(1, Ordering::Relaxed)
        })
    ]);
}

impl Dispatch<ZwlrOutputHeadV1, u32> for WaylandState {
    fn event(
        state: &mut Self,
        head_proxy: &ZwlrOutputHeadV1,
        event: zwlr_output_head_v1::Event,
        head_id: &u32,
        _: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Handle Finished event - head disappeared (monitor unplugged)
        if matches!(event, zwlr_output_head_v1::Event::Finished) {
            info!(
                "Head finished (disconnected) - removing from state (id: {})",
                head_id
            );

            // Remove from pending heads
            state.pending_heads.remove(head_id);

            // Capture associated mode objects before removing them.
            let mode_objects: Vec<_> = state
                .mode_to_head
                .iter()
                .filter(|(_, &hid)| hid == *head_id)
                .map(|(obj_id, _)| obj_id.clone())
                .collect();

            // Remove mode-to-head associations
            for obj_id in &mode_objects {
                state.mode_to_head.remove(obj_id);
                // Also remove mode object data
                state.mode_obj_to_data.remove(obj_id);
            }

            // Remove pending current mode for this head
            state.pending_current_modes.remove(head_id);

            // Resolve numeric mode IDs from their protocol object IDs.
            let mode_ids: Vec<_> = state
                .mode_proxies
                .iter()
                .filter(|(_, proxy)| mode_objects.contains(&proxy.id()))
                .map(|(id, _)| *id)
                .collect();

            for mode_id in mode_ids {
                state.pending_modes.remove(&mode_id);
                state.mode_metadata.remove(&mode_id);
                state.mode_proxies.remove(&mode_id);
            }

            // Match by stable head ID because name mappings may already be cleared.
            state.head_id_to_name.remove(head_id);
            let disconnected = state
                .heads
                .iter()
                .find(|(_, head)| head.id == *head_id)
                .map(|(name, _)| name.clone());

            match disconnected {
                Some(name) => {
                    state.heads.remove(&name);
                    info!(
                        "Removed disconnected head '{}' and cleaned up {} associated modes",
                        name,
                        mode_objects.len()
                    );
                }
                None => {
                    info!(
                        "Cleaned up disconnected head (id: {}) and {} associated modes",
                        head_id,
                        mode_objects.len()
                    );
                }
            }

            // Head removal changes the configuration even without `done`.
            state.configuration_generation = state.configuration_generation.wrapping_add(1);

            return;
        }

        match event {
            zwlr_output_head_v1::Event::Name { name } => {
                debug!("Head name: {}", name);

                // Get the head from pending or create new
                let head = state
                    .pending_heads
                    .entry(*head_id)
                    .or_insert_with(|| OutputHead {
                        id: *head_id,
                        name: String::new(),
                        description: String::new(),
                        physical_size: None,
                        position: None,
                        transform: wl_output::Transform::Normal,
                        scale: 1.0,
                        enabled: false,
                        modes: Vec::new(),
                        current_mode: None,
                        preferred_mode: None,
                        make: String::new(),
                        model: String::new(),
                        serial_number: String::new(),
                        adaptive_sync: None,
                    });

                // Update the name
                head.name = name.clone();

                // Store the proxy for later use in configuration
                state.head_proxies.insert(name.clone(), head_proxy.clone());

                // Store head_id → name mapping for mode lookup later
                state.head_id_to_name.insert(*head_id, name);

                // NOTE: Don't move to final heads map yet - wait for all events including modes
            }
            zwlr_output_head_v1::Event::Finished => {
                // Already handled above
            }
            _ => {
                // For all other events, update pending head or find by name in final heads
                let head = state
                    .pending_heads
                    .entry(*head_id)
                    .or_insert_with(|| OutputHead {
                        id: *head_id,
                        name: String::new(),
                        description: String::new(),
                        physical_size: None,
                        position: None,
                        transform: wl_output::Transform::Normal,
                        scale: 1.0,
                        enabled: false,
                        modes: Vec::new(),
                        current_mode: None,
                        preferred_mode: None,
                        make: String::new(),
                        model: String::new(),
                        serial_number: String::new(),
                        adaptive_sync: None,
                    });

                match event {
                    zwlr_output_head_v1::Event::Description { description } => {
                        head.description = description;
                        debug!("Head description: {}", head.description);
                    }
                    zwlr_output_head_v1::Event::PhysicalSize { width, height } => {
                        head.physical_size = Some((width, height));
                        debug!("Head physical size: {}x{}mm", width, height);
                    }
                    zwlr_output_head_v1::Event::Mode { mode } => {
                        debug!("Head mode announced for head {}: {:?}", head_id, mode.id());
                        // Store which head this mode belongs to (this is the ONLY place we have this association!)
                        state.mode_to_head.insert(mode.id(), *head_id);
                        debug!("Stored mode {:?} → head {} association", mode.id(), head_id);
                    }
                    zwlr_output_head_v1::Event::Enabled { enabled } => {
                        head.enabled = enabled != 0;
                        debug!("Head enabled: {}", head.enabled);
                    }
                    zwlr_output_head_v1::Event::CurrentMode { mode } => {
                        // Store mode proxy to resolve later (after modes are finalized)
                        state.pending_current_modes.insert(*head_id, mode);
                        debug!("Head current mode proxy stored for later resolution");
                    }
                    zwlr_output_head_v1::Event::Position { x, y } => {
                        head.position = Some((x, y));
                        debug!("Head position: {},{}", x, y);
                    }
                    zwlr_output_head_v1::Event::Transform { transform } => {
                        head.transform = match transform {
                            wayland_client::WEnum::Value(t) => t,
                            wayland_client::WEnum::Unknown(_) => wl_output::Transform::Normal,
                        };
                        debug!("Head transform: {:?}", head.transform);
                    }
                    zwlr_output_head_v1::Event::Scale { scale } => {
                        head.scale = scale;
                        debug!("Head scale: {}", head.scale);
                    }
                    zwlr_output_head_v1::Event::Make { make } => {
                        head.make = make;
                        debug!("Head make: {}", head.make);
                    }
                    zwlr_output_head_v1::Event::Model { model } => {
                        head.model = model;
                        debug!("Head model: {}", head.model);
                    }
                    zwlr_output_head_v1::Event::SerialNumber { serial_number } => {
                        head.serial_number = serial_number;
                        debug!("Head serial: {}", head.serial_number);
                    }
                    zwlr_output_head_v1::Event::AdaptiveSync { state: sync_state } => {
                        head.adaptive_sync = Some(matches!(sync_state, wayland_client::WEnum::Value(
                                wayland_protocols_wlr::output_management::v1::client::zwlr_output_head_v1::AdaptiveSyncState::Enabled
                            )));
                        debug!("Head adaptive sync: {:?}", head.adaptive_sync);
                    }
                    _ => {}
                }
            }
        }
    }

    wayland_client::event_created_child!(WaylandState, ZwlrOutputHeadV1, [
        3 => (ZwlrOutputModeV1, {
            // Generate unique ID for this mode
            use std::sync::atomic::{AtomicU32, Ordering};
            static MODE_COUNTER: AtomicU32 = AtomicU32::new(1000);
            MODE_COUNTER.fetch_add(1, Ordering::Relaxed)
        })
    ]);
}

impl Dispatch<ZwlrOutputModeV1, u32> for WaylandState {
    fn event(
        state: &mut Self,
        mode_proxy: &ZwlrOutputModeV1,
        event: zwlr_output_mode_v1::Event,
        mode_id: &u32,
        _: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Get or create pending mode
        let mode = state
            .pending_modes
            .entry(*mode_id)
            .or_insert_with(|| OutputMode {
                width: 0,
                height: 0,
                refresh_mhz: 0,
                preferred: false,
            });

        match event {
            zwlr_output_mode_v1::Event::Size { width, height } => {
                mode.width = width;
                mode.height = height;
                debug!("Mode size: {}x{} (id: {})", width, height, mode_id);

                // Store the mode proxy for later use in configuration
                state.mode_proxies.insert(*mode_id, mode_proxy.clone());
            }
            zwlr_output_mode_v1::Event::Refresh { refresh } => {
                mode.refresh_mhz = refresh;
                debug!("Mode refresh: {}mHz (id: {})", refresh, mode_id);
            }
            zwlr_output_mode_v1::Event::Preferred => {
                mode.preferred = true;
                debug!("Mode is preferred (id: {})", mode_id);
            }
            zwlr_output_mode_v1::Event::Finished => {
                debug!(
                    "Mode finished (id: {}) - mode removed/no longer available",
                    mode_id
                );
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _config: &ZwlrOutputConfigurationV1,
        event: zwlr_output_configuration_v1::Event,
        _: &(),
        _: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_configuration_v1::Event::Succeeded => {
                info!("Output configuration succeeded");
                state.configuration_outcome = Some(ConfigurationOutcome::Succeeded);
                state.active_config = None;
            }
            zwlr_output_configuration_v1::Event::Failed => {
                error!("Output configuration failed");
                state.configuration_outcome = Some(ConfigurationOutcome::Failed);
                state.active_config = None;
            }
            zwlr_output_configuration_v1::Event::Cancelled => {
                warn!("Output configuration cancelled");
                state.configuration_outcome = Some(ConfigurationOutcome::Cancelled);
                state.active_config = None;
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _config_head: &ZwlrOutputConfigurationHeadV1,
        event: zwlr_output_configuration_head_v1::Event,
        _: &(),
        _: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        {
            // Configuration head events would be handled here
            debug!("Configuration head event: {:?}", event);
        }
    }
}
