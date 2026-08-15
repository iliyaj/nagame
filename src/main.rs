// SPDX-FileCopyrightText: 2026 iliyaj
// SPDX-License-Identifier: GPL-3.0-or-later

//! Parses command-line options, initializes logging, and starts the nagame daemon.

use anyhow::Result;
use clap::{Parser, Subcommand};
use nagame::config::Config;
use nagame::ipc::{ClientRequest, ServerEvent};
use nagame::NagameDaemon;
use std::path::PathBuf;
use tracing::{error, info};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

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

#[derive(Subcommand)]
enum Command {
    /// Create the first profile from the live Wayland output configuration
    Init,
    /// Query and temporarily preview display modes through the running daemon
    Display {
        #[command(subcommand)]
        command: DisplayCommand,
    },
}

#[derive(Subcommand)]
enum DisplayCommand {
    /// Return connected outputs and exact advertised modes as JSON
    Outputs,
    /// Test and apply one advertised mode for a 15-second preview
    Preview {
        #[arg(long)]
        output: String,
        #[arg(long)]
        mode: String,
        #[arg(long)]
        profile: String,
        #[arg(long)]
        revision: String,
    },
    /// Persist a pending preview by transaction ID
    Confirm {
        #[arg(long)]
        transaction: String,
    },
    /// Revert a pending preview by transaction ID
    Revert {
        #[arg(long)]
        transaction: String,
    },
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

    if let Some(Command::Init) = args.command {
        let config_path = config_path(args.config);
        match nagame::initialize::initialize_config(&config_path).await {
            Ok(()) => println!(
                "{}",
                serde_json::json!({
                    "event": "initialized",
                    "config": config_path,
                })
            ),
            Err(error) => {
                println!(
                    "{}",
                    serde_json::to_string(&ServerEvent::error("init_failed", error.to_string()))?
                );
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    if let Some(Command::Display { command }) = args.command {
        let request = match command {
            DisplayCommand::Outputs => ClientRequest::Outputs,
            DisplayCommand::Preview {
                output,
                mode,
                profile,
                revision,
            } => ClientRequest::Preview {
                output,
                mode_id: mode,
                profile,
                revision,
            },
            DisplayCommand::Confirm { transaction } => ClientRequest::Confirm {
                transaction_id: transaction,
            },
            DisplayCommand::Revert { transaction } => ClientRequest::Revert {
                transaction_id: transaction,
            },
        };
        if let Err(error) = nagame::ipc::run_client(request).await {
            println!(
                "{}",
                serde_json::to_string(&ServerEvent::error(
                    "daemon_unavailable",
                    error.to_string()
                ))?
            );
            std::process::exit(1);
        }
        return Ok(());
    }

    info!("Starting nagame daemon");

    // Determine config file path
    let config_path = config_path(args.config);

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

fn config_path(config: Option<PathBuf>) -> PathBuf {
    config.unwrap_or_else(|| {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("nagame")
            .join("config.toml")
    })
}
