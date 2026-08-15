<!-- SPDX-FileCopyrightText: 2026 iliyaj -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
<!-- Provides a concise overview and quick start for nagame. -->

# nagame

nagame is a Wayland display and wallpaper manager. It watches for connected monitors, applies the matching display profile, and restores wallpapers after display changes or sleep.

The project is in an early stage. It is currently tested on Arch Linux with Hyprland, and other compositors and hardware may behave differently.

## Features

- Applies display profiles when monitors are connected or disconnected.
- Configures modes, positions, scaling, rotation, and adaptive sync.
- Manages wallpapers with `awww`, including random images from a directory.
- Preserves wallpaper choices across display changes and sleep.
- Reloads its TOML configuration when the file changes.
- Can run commands after activating a profile.
- Exposes connected modes and safe 15-second previews through a local JSON command interface.

## Requirements

- A Wayland compositor that supports `wlr-output-management-unstable-v1`, such as Hyprland.
- `awww` for wallpaper management. Display management works without it.
- Rust 1.86 or later when building from source.

## Quick start

```bash
git clone https://github.com/iliyaj/nagame.git
cd nagame
./scripts/build-release.sh
sudo cp target/release/nagame /usr/local/bin/

mkdir -p ~/.config/nagame
cp config.toml.example ~/.config/nagame/config.toml
```

Edit `~/.config/nagame/config.toml` for your displays and wallpaper paths, then validate it before starting the daemon:

```bash
nagame --test-only
nagame
```

## Architecture

nagame is divided into six main parts:

- The Wayland client discovers outputs and applies configurations through the `wlr-output-management` protocol.
- The profile manager matches connected outputs to configured profiles.
- The `awww` integration applies and restores wallpapers.
- The configuration watcher reloads the TOML file after changes.
- The private Unix socket exposes structured output discovery and preview/revert transactions.
- The Tokio event loop coordinates display, signal, and configuration events.

## Documentation

See [INSTRUCTIONS.md](INSTRUCTIONS.md) for the configuration reference, systemd setup, troubleshooting, development commands, and project details.

Security issues should be reported as described in [SECURITY.md](SECURITY.md), not opened as public issues.

## Project background

The name **nagame** (眺め) is Japanese for “view” or “scenery,” reflecting the project's focus on displays and wallpapers. It was originally created to address a persistent wallpaper restoration problem with `awww`.

The project draws inspiration from
[kanshi](https://sr.ht/~emersion/kanshi/),
[shikane](https://gitlab.com/w0lff/shikane),
[awww](https://github.com/LGFae/awww),
[autorandr](https://github.com/phillipberndt/autorandr), the Rust Wayland ecosystem, and [Quickshell](https://github.com/outfoxxed/quickshell).

## License

GNU General Public License v3.0 or later. See [LICENSE](LICENSE).
