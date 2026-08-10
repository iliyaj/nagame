// SPDX-FileCopyrightText: 2026 iliyaj
// SPDX-License-Identifier: GPL-3.0-or-later

//! Parses command-line options, initializes logging, and starts the nagame daemon.

use anyhow::Result;
use clap::Parser;
use nagame::config::Config;
use nagame::NagameDaemon;
use std::path::PathBuf;
use tracing::{error, info};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Enable debug logging
    #[arg(short, long)]
    debug: bool,

    /// Test mode - validate config and profiles without applying changes
    #[arg(long)]
    test_only: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize logging
    let log_level = if args.debug { "debug" } else { "info" };
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level)))
        .init();

    info!("Starting nagame daemon");

    // Determine config file path
    let config_path = args.config.unwrap_or_else(|| {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("nagame")
            .join("config.toml")
    });

    info!("Using config file: {}", config_path.display());

    if args.test_only {
        info!("Test mode - validating configuration without starting services");
        let config = Config::load(&config_path).await?;
        config.validate_environment().await?;
        info!("✅ Configuration validation passed!");
        return Ok(());
    }

    // Create daemon only after side-effect-free validation mode has returned.
    let daemon = NagameDaemon::new(config_path).await?;

    // Run daemon normally
    if let Err(e) = daemon.run().await {
        error!("Daemon error: {}", e);
        std::process::exit(1);
    }

    Ok(())
}
