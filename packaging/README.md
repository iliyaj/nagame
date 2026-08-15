<!-- SPDX-FileCopyrightText: 2026 iliyaj -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# Packaging

`PKGBUILD` builds an Arch Linux package from a tagged GitHub release.

## Installing from this PKGBUILD

```bash
cd packaging
makepkg -si
```

The package installs:

| Path | Contents |
| --- | --- |
| `/usr/bin/nagame` | the daemon |
| `/usr/lib/systemd/user/nagame.service` | systemd user unit |
| `/usr/share/nagame/config.toml.example` | example configuration |
| `/usr/share/doc/nagame/README.md` | documentation |
| `/usr/share/licenses/nagame/LICENSE` | license text |

After installing, initialize a config from the live outputs and enable the service:

```bash
nagame init
nagame --test-only
systemctl --user enable --now nagame.service
```

## Releasing

After tagging and publishing `vX.Y.Z`, update `pkgver`, run `updpkgsums` in
this directory, and regenerate `.SRCINFO`. Verify the tagged source with
`makepkg --verifysource` before publishing. Never publish a package with a
`SKIP` checksum.

## AUR

To publish on the AUR, clone `ssh://aur@aur.archlinux.org/nagame.git`, copy
`PKGBUILD` in, generate `.SRCINFO` with `makepkg --printsrcinfo > .SRCINFO`,
and push. A `nagame-git` variant that builds from the default branch suits
users who want unreleased changes.
