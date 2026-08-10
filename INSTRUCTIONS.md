<!-- SPDX-FileCopyrightText: 2026 iliyaj -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
<!-- Contains installation, configuration, operation, and development guidance. -->

# nagame instructions

This guide covers installation, configuration, operation, troubleshooting, and development.

## Installation

### Build from source

```bash
git clone https://github.com/iliyaj/nagame.git
cd nagame
./scripts/build-release.sh
sudo cp target/release/nagame /usr/local/bin/
mkdir -p ~/.config/nagame
cp config.toml.example ~/.config/nagame/config.toml
```

The build requires Rust 1.86 or later. At runtime, nagame requires a Wayland compositor with `wlr-output-management-unstable-v1` support. Wallpaper features also require `awww`.

## Configuration

The default configuration path is `~/.config/nagame/config.toml`. The included [`config.toml.example`](config.toml.example) contains additional profile examples.

Each `[[profile]]` describes a monitor arrangement. A profile matches when its configured outputs match the outputs reported by the compositor.

```toml
# Laptop display only
[[profile]]
name = "laptop"
wallpaper = "~/Pictures/Wallpapers/laptop.jpg"
exec = ["hyprctl notify -1 2000 'rgb(00ff00)' 'Laptop mode activated'"]

  [[profile.output]]
  name = "eDP-1"
  mode = "1920x1080@60"
  scale = 1.5
  position = [0, 0]

# External display with the laptop panel disabled
[[profile]]
name = "docked"
wallpaper_dir = "~/Pictures/Wallpapers/desktop"

  [[profile.output]]
  name = "eDP-1"
  enabled = false

  [[profile.output]]
  name = "HDMI-A-2"
  mode = "2560x1440@144"
  scale = 1.0
  position = [0, 0]
  adaptive_sync = true

# Laptop and external display
[[profile]]
name = "dual-monitors"
wallpaper = "~/Pictures/Wallpapers/ultrawide.jpg"

  [[profile.output]]
  name = "eDP-1"
  mode = "1920x1080@60"
  scale = 1.0
  position = [0, 0]

  [[profile.output]]
  name = "HDMI-A-2"
  mode = "2560x1440@144"
  scale = 1.0
  position = [1920, 0]
```

### Profile options

- `name`: A unique profile name.
- `wallpaper`: An optional image path. `~` expands to the home directory.
- `wallpaper_dir`: An optional directory from which nagame chooses a random supported image. It takes precedence over `wallpaper` when both are set.
- `exec`: A list of commands to run after the profile is activated. A command failure does not roll back a display configuration that was already applied.

### Output options

- `name`: The compositor's output name, such as `eDP-1` or `HDMI-A-2`. Use `*` to match any output.
- `enabled`: Whether the output is enabled. The default is `true`.
- `mode`: Resolution and refresh rate, such as `1920x1080@60`.
- `scale`: Scaling factor, such as `1.5`.
- `position`: Output location in the shared coordinate space, written as `[x, y]`.
- `transform`: Rotation or reflection, such as `90`, `180`, `270`, or `flipped-90`.
- `adaptive_sync`: Whether to enable adaptive sync, when supported.

## Running nagame

Start nagame with its default configuration:

```bash
nagame
```

Useful options are:

```bash
# Use another configuration file
nagame --config /path/to/config.toml

# Validate the configuration and referenced wallpaper paths without applying it
nagame --test-only

# Show debug logs
nagame --debug
```

`--test-only` verifies the TOML configuration and checks that each configured `wallpaper` and `wallpaper_dir` exists. Update the paths copied from the example before running it.

## Run as a systemd user service

Create `~/.config/systemd/user/nagame.service`:

```ini
[Unit]
Description=nagame Wayland display and wallpaper manager
Documentation=https://github.com/iliyaj/nagame
After=graphical-session.target
Wants=graphical-session.target
PartOf=graphical-session.target

[Service]
Type=exec
ExecStart=/usr/local/bin/nagame --config %h/.config/nagame/config.toml
Restart=always
RestartSec=5
StandardOutput=journal
StandardError=journal
KillMode=mixed
TimeoutStopSec=30
PrivateNetwork=false
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=%h/.local/state
NoNewPrivileges=true

[Install]
WantedBy=graphical-session.target
```

Enable and start the service:

```bash
systemctl --user enable --now nagame
```

If nagame cannot connect to Wayland, import the session variables from your compositor's startup configuration before starting the service:

```bash
systemctl --user import-environment WAYLAND_DISPLAY XDG_RUNTIME_DIR
```

View service logs with:

```bash
journalctl --user -u nagame -f
```

## Wallpaper behavior

nagame uses `awww` to apply wallpapers. It shares the current wallpaper path through `~/.local/state/quickshell/user/current-wallpaper`, allowing compatible Quickshell setups and nagame to keep the same selection. This prevents profile changes from needlessly replacing a wallpaper selected elsewhere and lets nagame restore the image when a display reconnects.

Display profile management remains available when `awww` is not installed, but wallpaper changes and restoration are skipped.

## Troubleshooting

### A wallpaper is not restored after sleep

Confirm that `awww` is installed and available:

```bash
which awww
awww --version
```

Check the saved wallpaper path and follow the service logs:

```bash
cat ~/.local/state/quickshell/user/current-wallpaper
journalctl --user -u nagame -f
```

### A profile does not match

Validate the configuration, then run nagame with debug logging to see its profile-matching decisions:

```bash
nagame --test-only
RUST_LOG=debug nagame --debug
```

Check that output names and modes match those reported by your compositor.

## Development

Build a debug binary with:

```bash
cargo build
```

Use `./scripts/build-release.sh` for distributable binaries. The script remaps the builder's home directory so local paths are not embedded in the executable.

Before submitting changes, run:

```bash
cargo test
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

To run from source with debug output:

```bash
RUST_LOG=debug cargo run -- --debug
```
