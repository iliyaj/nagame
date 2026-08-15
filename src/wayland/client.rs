// SPDX-FileCopyrightText: 2026 iliyaj
// SPDX-License-Identifier: GPL-3.0-or-later

//! Manages the Wayland connection and dispatches output-management events.

use super::protocols::{HeadConfiguration, OutputHead, WaylandState};
use anyhow::Result;
use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info, warn};
use wayland_client::{backend::WaylandError, Connection, EventQueue};

/// How long to wait for the compositor's apply/test verdict
const CONFIG_RESULT_TIMEOUT: Duration = Duration::from_secs(2);

/// Gap between checks while waiting for that verdict
const CONFIG_RESULT_POLL: Duration = Duration::from_millis(20);

/// Main Wayland client for nagame
pub struct WaylandClient {
    connection: Connection,
    event_queue: EventQueue<WaylandState>,
    state: WaylandState,
}

impl WaylandClient {
    /// Create a new Wayland client
    pub async fn new() -> Result<Self> {
        info!("Connecting to Wayland display");

        let connection = Connection::connect_to_env()
            .map_err(|e| anyhow::anyhow!("Failed to connect to Wayland display: {}", e))?;

        let display = connection.display();
        let event_queue = connection.new_event_queue();
        let qh = event_queue.handle();

        // Create initial state and bind to registry
        let mut state = WaylandState::new();
        let registry = display.get_registry(&qh, ());
        state.registry = Some(registry);

        info!("Successfully connected to Wayland display");

        let mut client = Self {
            connection,
            event_queue,
            state,
        };

        // Perform initial roundtrip to discover globals
        client.roundtrip().await?;

        if client.state.output_manager.is_none() {
            warn!("wlr-output-management protocol not available - output control disabled");
        } else {
            info!("wlr-output-management protocol available");
        }

        Ok(client)
    }

    /// Perform a roundtrip to process pending events
    async fn roundtrip(&mut self) -> Result<()> {
        self.event_queue
            .roundtrip(&mut self.state)
            .map_err(|e| anyhow::anyhow!("Wayland roundtrip failed: {}", e))?;
        Ok(())
    }

    /// Handle incoming Wayland events (non-blocking)
    pub async fn handle_events(&mut self) -> Result<()> {
        debug!("Handling Wayland events");

        // Dispatch pending events without blocking
        if let Err(e) = self.event_queue.dispatch_pending(&mut self.state) {
            error!("Failed to dispatch Wayland events: {}", e);
        }

        // Small delay to prevent busy loop
        sleep(Duration::from_millis(10)).await;

        Ok(())
    }

    /// Duplicates the connection fd for readiness polling.
    pub fn connection_fd(&self) -> Result<OwnedFd> {
        self.connection
            .as_fd()
            .try_clone_to_owned()
            .map_err(|e| anyhow::anyhow!("Failed to duplicate Wayland connection fd: {}", e))
    }

    /// Dispatches queued events that would not make the connection fd readable.
    pub fn dispatch_pending(&mut self) -> Result<usize> {
        self.event_queue
            .dispatch_pending(&mut self.state)
            .map_err(|e| anyhow::anyhow!("Failed to dispatch Wayland events: {}", e))
    }

    /// Flush outgoing requests so the compositor can act on them before we block
    pub fn flush(&mut self) -> Result<()> {
        self.event_queue
            .flush()
            .map_err(|e| anyhow::anyhow!("Failed to flush Wayland connection: {}", e))
    }

    /// Reads signalled events; zero dispatched events indicates spurious readiness.
    pub fn read_and_dispatch(&mut self) -> Result<usize> {
        let Some(guard) = self.event_queue.prepare_read() else {
            // Another reader got there first, or events are already queued
            return self.dispatch_pending();
        };

        match guard.read() {
            Ok(_) => {}
            Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                return Ok(0);
            }
            Err(e) => return Err(anyhow::anyhow!("Failed to read Wayland events: {}", e)),
        }

        self.dispatch_pending()
    }

    /// Which complete configuration we are on; bumped by every `done` event
    pub fn configuration_generation(&self) -> u64 {
        self.state.configuration_generation
    }

    /// Discovers outputs during startup or safety checks; normal updates are event-driven.
    pub async fn refresh_outputs(&mut self) -> Result<()> {
        debug!("Refreshing output information");

        if self.state.output_manager.is_none() {
            warn!("Cannot refresh outputs - wlr-output-management not available");
            return Ok(());
        }

        self.roundtrip().await?;

        // Protocol events own the head map, so refreshes must not clear it.
        self.state.finalize_pending();

        debug!("Discovered {} output heads", self.state.heads.len());
        for (name, head) in &self.state.heads {
            debug!(
                "  {}: {} (enabled: {}, modes: {})",
                name,
                head.description,
                head.enabled,
                head.modes.len()
            );
        }

        Ok(())
    }

    /// Get all available output heads
    pub fn get_outputs(&self) -> &HashMap<String, OutputHead> {
        &self.state.heads
    }

    /// Get a specific output by name
    pub fn get_output(&self, name: &str) -> Option<&OutputHead> {
        self.state.heads.get(name)
    }

    /// Check if output management is available
    pub fn has_output_management(&self) -> bool {
        self.state.output_manager.is_some()
    }

    /// Create a new output configuration
    pub async fn create_configuration(&mut self) -> Result<()> {
        let qh = self.event_queue.handle();
        self.state.create_configuration(&qh)?;
        Ok(())
    }

    /// Configure a specific output head
    pub async fn configure_head(
        &mut self,
        head_name: &str,
        config: &HeadConfiguration,
    ) -> Result<()> {
        let qh = self.event_queue.handle();
        self.state.configure_head(head_name, config, &qh)?;
        Ok(())
    }

    /// Apply the current configuration
    pub async fn apply_configuration(&mut self) -> Result<()> {
        self.state.apply_configuration()?;
        self.await_configuration_result().await
    }

    /// Adopt a compositor-confirmed configuration before its follow-up head events arrive.
    pub fn adopt_configuration(&mut self, configuration: &[(String, HeadConfiguration)]) {
        for (name, config) in configuration {
            let Some(head) = self.state.heads.get_mut(name) else {
                continue;
            };
            adopt_head_configuration(head, config);
        }
    }

    /// Test the current configuration
    pub async fn test_configuration(&mut self) -> Result<()> {
        self.state.test_configuration()?;
        self.await_configuration_result().await
    }

    /// Waits for the verdict because compositors decide well after the request roundtrip.
    async fn await_configuration_result(&mut self) -> Result<()> {
        let deadline = tokio::time::Instant::now() + CONFIG_RESULT_TIMEOUT;

        loop {
            self.roundtrip().await?;

            if self.state.has_configuration_outcome() {
                return self.state.take_configuration_result();
            }

            if tokio::time::Instant::now() >= deadline {
                warn!(
                    "Compositor gave no configuration verdict within {:?}",
                    CONFIG_RESULT_TIMEOUT
                );
                self.state.retire_active_configuration();
                return Err(anyhow::anyhow!(
                    "Compositor gave no output configuration verdict within {:?}",
                    CONFIG_RESULT_TIMEOUT
                ));
            }

            tokio::time::sleep(CONFIG_RESULT_POLL).await;
        }
    }

    /// Start the event loop (blocking)
    pub async fn run_event_loop(&mut self) -> Result<()> {
        info!("Starting Wayland event loop");

        loop {
            if let Err(e) = self.handle_events().await {
                error!("Event handling error: {}", e);
                // Continue running unless it's a fatal error
            }

            // Small delay between event processing cycles
            sleep(Duration::from_millis(50)).await;
        }
    }

    /// Get connection for low-level operations
    pub fn connection(&self) -> &Connection {
        &self.connection
    }
}

fn adopt_head_configuration(head: &mut OutputHead, config: &HeadConfiguration) {
    head.enabled = config.enabled;
    if let Some(mode) = &config.mode {
        head.current_mode = Some(mode.clone());
    }
    if let Some(position) = config.position {
        head.position = Some(position);
    }
    if let Some(transform) = config.transform {
        head.transform = transform;
    }
    if let Some(scale) = config.scale {
        head.scale = scale;
    }
    if let Some(adaptive_sync) = config.adaptive_sync {
        head.adaptive_sync = Some(adaptive_sync);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wayland::OutputMode;
    use wayland_client::protocol::wl_output;

    #[test]
    fn adopting_a_modeless_configuration_keeps_the_advertised_current_mode() {
        let current_mode = OutputMode {
            width: 2560,
            height: 1440,
            refresh_mhz: 143_973,
            preferred: true,
        };
        let mut head = OutputHead {
            id: 1,
            name: "DP-1".to_string(),
            description: String::new(),
            physical_size: None,
            position: Some((0, 0)),
            transform: wl_output::Transform::Normal,
            scale: 1.0,
            enabled: true,
            modes: vec![current_mode.clone()],
            current_mode: Some(current_mode.clone()),
            preferred_mode: Some(current_mode.clone()),
            make: String::new(),
            model: String::new(),
            serial_number: String::new(),
            adaptive_sync: None,
        };
        let config = HeadConfiguration {
            enabled: true,
            mode: None,
            position: None,
            transform: None,
            scale: None,
            adaptive_sync: None,
        };

        adopt_head_configuration(&mut head, &config);

        assert_eq!(head.current_mode, Some(current_mode));
    }
}
