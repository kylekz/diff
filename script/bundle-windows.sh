#!/usr/bin/env bash
# Assemble the Windows dist bundle.
#
#   bash script/bundle-windows.sh
#
# Produces/refreshes dist/ with the two-binary layout:
#
#   dv.exe       console launcher (crates/cli, console subsystem) — the
#                PATH-visible entry point: runs the headless CLI in-console
#                (shell waits, clean output) and forwards GUI-shaped
#                invocations to dv-gui.exe next to it
#   dv-gui.exe   the windowed app (crates/app, shipped shape: release,
#                --locked, automation feature OFF)
#
# The WSL sidecars (dv-host-linux-x64 / dv-linux-x64) must sit in the same
# dir for WSL support; they're built INSIDE a distro (see CLAUDE.md § WSL
# host — keep the target dir on ext4, not 9P) and this script only checks
# they're present, it can't build them.

set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release --locked -p dv --no-default-features
cargo build --release --locked -p dv-cli

mkdir -p dist
cp target/release/dv-cli.exe dist/dv.exe
cp target/release/dv.exe dist/dv-gui.exe

for sidecar in dv-host-linux-x64 dv-linux-x64; do
  if [ ! -f "dist/$sidecar" ]; then
    echo "warning: dist/$sidecar missing — WSL support will be inert" >&2
    echo "  (build in WSL: CARGO_TARGET_DIR=\$HOME/.cache/dv-target cargo build --release, then copy)" >&2
  fi
done

echo "done: dist/ (dv.exe launcher + dv-gui.exe app)"
