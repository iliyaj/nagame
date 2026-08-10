#!/usr/bin/env sh
# SPDX-FileCopyrightText: 2026 iliyaj
# SPDX-License-Identifier: GPL-3.0-or-later
# Builds a release binary without embedding the builder's home-directory path.

set -eu

build_home=${HOME:?HOME must be set}
remap_flag="--remap-path-prefix=${build_home}=/build/home"

if [ -n "${RUSTFLAGS:-}" ]; then
    export RUSTFLAGS="${RUSTFLAGS} ${remap_flag}"
else
    export RUSTFLAGS="${remap_flag}"
fi

cargo build --release --locked
