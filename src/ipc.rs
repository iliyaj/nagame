// SPDX-FileCopyrightText: 2026 iliyaj
// SPDX-License-Identifier: GPL-3.0-or-later

//! Provides the private display-control socket and its JSON command client.

use crate::wayland::{OutputHead, OutputMode};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::env;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);
const MAX_REQUEST_BYTES: usize = 64 * 1024;

#[derive(Debug, PartialEq)]
enum RequestLine {
    Empty,
    Complete(String),
    TooLarge,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum ClientRequest {
    Outputs,
    Preview {
        output: String,
        mode_id: String,
        profile: String,
        revision: String,
    },
    Confirm {
        transaction_id: String,
    },
    Revert {
        transaction_id: String,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ServerEvent {
    Outputs {
        outputs: Vec<DisplayOutput>,
        active_profile: Option<String>,
        revision: String,
        supported: bool,
    },
    PreviewStarted {
        transaction_id: String,
        remaining_ms: u64,
    },
    PreviewReverted {
        transaction_id: String,
        reason: String,
    },
    PreviewConfirmed {
        transaction_id: String,
        revision: String,
    },
    ConfirmCompleted {
        transaction_id: String,
        revision: String,
    },
    RevertCompleted {
        transaction_id: String,
    },
    Error {
        code: String,
        message: String,
    },
}

impl ServerEvent {
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Error {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DisplayOutput {
    pub identity: String,
    pub connector: String,
    pub name: String,
    pub make: String,
    pub model: String,
    pub serial_number: String,
    pub enabled: bool,
    pub current_mode_id: Option<String>,
    pub preferred_mode_id: Option<String>,
    pub modes: Vec<DisplayMode>,
}

impl From<&OutputHead> for DisplayOutput {
    fn from(head: &OutputHead) -> Self {
        let make_model = [head.make.as_str(), head.model.as_str()]
            .into_iter()
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let name = if !make_model.is_empty() {
            make_model
        } else if !head.description.trim().is_empty() && head.description != head.name {
            head.description.clone()
        } else {
            head.name.clone()
        };
        let identity = if head.serial_number.trim().is_empty() {
            head.name.clone()
        } else {
            format!("{}:{}:{}", head.make, head.model, head.serial_number)
        };

        let mut modes: Vec<_> = head.modes.iter().map(DisplayMode::from).collect();
        modes.sort_by(|left, right| {
            right
                .preferred
                .cmp(&left.preferred)
                .then_with(|| (right.width * right.height).cmp(&(left.width * left.height)))
                .then_with(|| right.width.cmp(&left.width))
                .then_with(|| right.refresh_mhz.cmp(&left.refresh_mhz))
        });
        let mut seen = HashSet::new();
        modes.retain(|mode| seen.insert(mode.id.clone()));

        Self {
            identity,
            connector: head.name.clone(),
            name,
            make: head.make.clone(),
            model: head.model.clone(),
            serial_number: head.serial_number.clone(),
            enabled: head.enabled,
            current_mode_id: head.current_mode.as_ref().map(mode_id),
            preferred_mode_id: head.preferred_mode.as_ref().map(mode_id),
            modes,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DisplayMode {
    pub id: String,
    pub width: i32,
    pub height: i32,
    pub refresh_mhz: i32,
    pub preferred: bool,
}

impl From<&OutputMode> for DisplayMode {
    fn from(mode: &OutputMode) -> Self {
        Self {
            id: mode_id(mode),
            width: mode.width,
            height: mode.height,
            refresh_mhz: mode.refresh_mhz,
            preferred: mode.preferred,
        }
    }
}

pub fn mode_id(mode: &OutputMode) -> String {
    format!("{}x{}@{}mHz", mode.width, mode.height, mode.refresh_mhz)
}

pub struct IpcRequest {
    pub client_id: u64,
    pub request: ClientRequest,
    pub responses: mpsc::UnboundedSender<ServerEvent>,
}

pub enum Incoming {
    Request(IpcRequest),
    Disconnected(u64),
}

pub struct SocketGuard {
    path: PathBuf,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn socket_path() -> Result<PathBuf> {
    let runtime_dir = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("XDG_RUNTIME_DIR is not set"))?;
    Ok(runtime_dir.join("nagame").join("display.sock"))
}

pub async fn start_server() -> Result<(mpsc::UnboundedReceiver<Incoming>, SocketGuard)> {
    let path = socket_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Nagame display socket has no parent directory"))?;
    std::fs::create_dir_all(parent)?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    if path.exists() {
        match UnixStream::connect(&path).await {
            Ok(_) => return Err(anyhow!("another Nagame display socket is already active")),
            Err(_) => {
                let metadata = std::fs::symlink_metadata(&path)?;
                if !metadata.file_type().is_socket() {
                    return Err(anyhow!(
                        "refusing to replace non-socket path {}",
                        path.display()
                    ));
                }
                std::fs::remove_file(&path)?;
            }
        }
    }

    let listener =
        UnixListener::bind(&path).with_context(|| format!("failed to bind {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let tx = incoming_tx.clone();
            tokio::spawn(async move {
                if let Err(error) = serve_client(stream, tx).await {
                    tracing::warn!("Display IPC client failed: {}", error);
                }
            });
        }
    });

    Ok((incoming_rx, SocketGuard { path }))
}

async fn serve_client(stream: UnixStream, incoming: mpsc::UnboundedSender<Incoming>) -> Result<()> {
    let client_id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let line = match read_request_line(&mut reader).await? {
        RequestLine::Empty => return Ok(()),
        RequestLine::Complete(line) => line,
        RequestLine::TooLarge => {
            write_event(
                &mut write_half,
                &ServerEvent::error(
                    "request_too_large",
                    format!("Display IPC requests are limited to {MAX_REQUEST_BYTES} bytes"),
                ),
            )
            .await?;
            return Ok(());
        }
    };

    let request = match serde_json::from_str::<ClientRequest>(&line) {
        Ok(request) => request,
        Err(error) => {
            write_event(
                &mut write_half,
                &ServerEvent::error("invalid_request", error.to_string()),
            )
            .await?;
            return Ok(());
        }
    };
    let (responses, mut response_rx) = mpsc::unbounded_channel();
    incoming
        .send(Incoming::Request(IpcRequest {
            client_id,
            request,
            responses,
        }))
        .map_err(|_| anyhow!("Nagame daemon stopped accepting display commands"))?;

    let mut discarded = [0_u8; 64];
    loop {
        tokio::select! {
            response = response_rx.recv() => {
                let Some(response) = response else { break };
                write_event(&mut write_half, &response).await?;
            }
            read = reader.read(&mut discarded) => {
                if read? == 0 {
                    let _ = incoming.send(Incoming::Disconnected(client_id));
                    break;
                }
            }
        }
    }
    Ok(())
}

async fn read_request_line<R>(reader: &mut R) -> Result<RequestLine>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut line = String::new();
    let bytes_read = reader
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_line(&mut line)
        .await?;
    if bytes_read == 0 {
        Ok(RequestLine::Empty)
    } else if line.len() > MAX_REQUEST_BYTES {
        Ok(RequestLine::TooLarge)
    } else {
        Ok(RequestLine::Complete(line))
    }
}

async fn write_event(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    event: &ServerEvent,
) -> Result<()> {
    writer.write_all(&serde_json::to_vec(event)?).await?;
    writer.write_all(b"\n").await?;
    Ok(())
}

pub async fn run_client(request: ClientRequest) -> Result<()> {
    let path = socket_path()?;
    let stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("cannot connect to Nagame at {}", path.display()))?;
    let (read_half, mut write_half) = stream.into_split();
    write_half.write_all(&serde_json::to_vec(&request)?).await?;
    write_half.write_all(b"\n").await?;

    let mut lines = BufReader::new(read_half).lines();
    while let Some(line) = lines.next_line().await? {
        println!("{line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_id_keeps_exact_fractional_refresh() {
        let mode = OutputMode {
            width: 1920,
            height: 1080,
            refresh_mhz: 59_940,
            preferred: false,
        };

        assert_eq!(mode_id(&mode), "1920x1080@59940mHz");
    }

    #[tokio::test]
    async fn request_lines_are_capped_before_unbounded_allocation() {
        let oversized = vec![b'x'; MAX_REQUEST_BYTES + 1];
        let mut reader = BufReader::new(oversized.as_slice());

        assert_eq!(
            read_request_line(&mut reader).await.unwrap(),
            RequestLine::TooLarge
        );

        let mut reader = BufReader::new(b"{\"command\":\"outputs\"}\n".as_slice());
        assert!(matches!(
            read_request_line(&mut reader).await.unwrap(),
            RequestLine::Complete(_)
        ));
    }
}
