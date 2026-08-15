// SPDX-FileCopyrightText: 2026 iliyaj
// SPDX-License-Identifier: GPL-3.0-or-later

//! Exposes nagame's Wayland client, output manager, and protocol state.

pub mod client;
pub mod output;
pub mod protocols;

pub use client::WaylandClient;
pub use output::OutputManager;
pub use protocols::{HeadConfiguration, OutputHead, OutputMode, WaylandState};

use anyhow::Result;

/// Initialize Wayland protocols and client
pub async fn init_wayland() -> Result<WaylandClient> {
    WaylandClient::new().await
}
