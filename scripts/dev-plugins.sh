#!/usr/bin/env bash
# Build the bundled plugins and install them into a plugins directory so a dev
# `npm run tauri dev` can discover them.
#
# For each plugin it: builds the binary, copies it into the plugins dir, runs
# `--describe` to get the plugin's PluginInfo, and writes a `<id>.plugin.json`
# manifest ({ pluginInfo, executable }) next to it.
#
# Usage:
#   scripts/dev-plugins.sh [debug|release]
#
# Honours $POLYCLIENT_PLUGINS_DIR (defaults to <repo>/dev-plugins). Point the host at
# the same dir when running the app:
#   POLYCLIENT_PLUGINS_DIR=<repo>/dev-plugins npm run tauri dev
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROFILE="${1:-debug}"
PLUGINS_DIR="${POLYCLIENT_PLUGINS_DIR:-$ROOT/dev-plugins}"

CARGO_FLAGS=()
TARGET_SUBDIR=debug
if [ "$PROFILE" = "release" ]; then
  CARGO_FLAGS+=(--release)
  TARGET_SUBDIR=release
fi

# plugin id -> crate name
PLUGINS=(postgres mongodb rabbitmq ssh sftp snowflake mysql mssql s3 curlui)
CRATES=(polyclient-plugin-postgres polyclient-plugin-mongodb polyclient-plugin-rabbitmq polyclient-plugin-ssh polyclient-plugin-sftp polyclient-plugin-snowflake polyclient-plugin-mysql polyclient-plugin-mssql polyclient-plugin-s3 polyclient-plugin-curlui)

mkdir -p "$PLUGINS_DIR"

for i in "${!PLUGINS[@]}"; do
  id="${PLUGINS[$i]}"
  crate="${CRATES[$i]}"
  echo "==> building $crate ($PROFILE)"
  cargo build ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"} -p "$crate"

  bin_name="$crate"
  [ "$(uname)" = "Windows_NT" ] && bin_name="$crate.exe"
  src_bin="$ROOT/target/$TARGET_SUBDIR/$bin_name"
  dest_bin="$PLUGINS_DIR/$bin_name"
  # Replace the destination with a fresh inode rather than overwriting in place:
  # on macOS the kernel caches a Mach-O's code-signature hash per inode, so
  # overwriting a binary that was already executed makes the new content fail
  # signature validation and get SIGKILL'd ("Killed: 9"). Removing first gives
  # `cp` a new inode; then re-sign ad-hoc on macOS for good measure.
  rm -f "$dest_bin"
  cp "$src_bin" "$dest_bin"
  if [ "$(uname)" = "Darwin" ]; then
    codesign --force --sign - "$dest_bin" >/dev/null 2>&1 || true
  fi

  # Generate the manifest: wrap the plugin's own --describe output.
  describe_tmp="$PLUGINS_DIR/$id.describe.json"
  "$dest_bin" --describe > "$describe_tmp"
  node -e '
    const fs = require("fs");
    const info = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
    const manifest = { pluginInfo: info, executable: "./" + process.argv[3] };
    fs.writeFileSync(process.argv[2], JSON.stringify(manifest, null, 2) + "\n");
  ' "$describe_tmp" "$PLUGINS_DIR/$id.plugin.json" "$bin_name"
  rm -f "$describe_tmp"
  echo "    installed $id -> $PLUGINS_DIR/$id.plugin.json"
done

echo
echo "Plugins installed to: $PLUGINS_DIR"
echo "Run the app with:"
echo "  POLYCLIENT_PLUGINS_DIR=$PLUGINS_DIR npm run tauri dev"
