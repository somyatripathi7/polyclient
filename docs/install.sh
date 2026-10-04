#!/usr/bin/env bash
# Install the polyclient desktop app from its GitHub release.
#
# Downloads and installs the platform-matched app bundle:
#   - macOS: mounts the .dmg and copies polyclient.app into /Applications
#   - Linux: places the .AppImage at ~/.local/bin/polyclient (executable)
#
# Plugins (Postgres, MySQL, MongoDB, …) are NOT installed here — add them from
# the app's sidebar via the "⤓ Install plugin" dialog once polyclient is running.
#
# Usage:
#   docs/install.sh [--nightly] [--tag <tag>] [--dir <path>]
#
#   curl -fsSL https://somyatripathi7.github.io/polyclient/install.sh | bash
#   curl -fsSL https://somyatripathi7.github.io/polyclient/install.sh | bash -s -- --nightly
#
# Flags:
#   --nightly     Install the rolling prerelease (app tag `latest`) instead of
#                 the newest stable release. Ignored if --tag is given.
#   --tag <tag>   Install a specific release tag (e.g. v0.1.2).
#   --dir <path>  Install location. macOS: the directory polyclient.app is copied into
#                 (default /Applications). Linux: the directory the AppImage is
#                 placed in as `polyclient` (default ~/.local/bin).
#   --help        Show this help.
#
# Honours $GITHUB_TOKEN (lifts the unauthenticated GitHub API rate limit).
set -euo pipefail

REPO="somyatripathi7/polyclient"
RELEASES_URL="https://github.com/${REPO}/releases"

# ---- pretty output -------------------------------------------------------
info()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()   { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'EOF'
Install the polyclient desktop app from its GitHub release.

Usage:
  install.sh [--nightly] [--tag <tag>] [--dir <path>]

  curl -fsSL https://somyatripathi7.github.io/polyclient/install.sh | bash
  curl -fsSL https://somyatripathi7.github.io/polyclient/install.sh | bash -s -- --nightly

Flags:
  --nightly     Install the rolling prerelease (app tag `latest`) instead of the
                newest stable release. Ignored if --tag is given.
  --tag <tag>   Install a specific release tag (e.g. v0.1.2).
  --dir <path>  Install location. macOS: the directory polyclient.app is copied into
                (default /Applications). Linux: the directory the AppImage is
                placed in as `polyclient` (default ~/.local/bin).
  --help        Show this help.

Only the desktop app is installed. Add database/queue plugins from the app's
sidebar via the "Install plugin" dialog once polyclient is running.

Supported by this script: macOS (arm64/x64) and Linux (x64). On Windows, or
Linux arm64, download an installer from:
  https://github.com/somyatripathi7/polyclient/releases

Honours $GITHUB_TOKEN (lifts the unauthenticated GitHub API rate limit).
EOF
  exit "${1:-0}"
}

# ---- args ----------------------------------------------------------------
CHANNEL="stable"   # stable | nightly
TAG=""
INSTALL_DIR=""
while [ $# -gt 0 ]; do
  case "$1" in
    --nightly) CHANNEL="nightly"; shift ;;
    --tag)     [ $# -ge 2 ] || die "--tag needs a value"; TAG="$2"; shift 2 ;;
    --tag=*)   TAG="${1#--tag=}"; shift ;;
    --dir)     [ $# -ge 2 ] || die "--dir needs a value"; INSTALL_DIR="$2"; shift 2 ;;
    --dir=*)   INSTALL_DIR="${1#--dir=}"; shift ;;
    -h|--help) usage 0 ;;
    *)         die "unknown argument: $1 (try --help)" ;;
  esac
done

# ---- tools ---------------------------------------------------------------
have() { command -v "$1" >/dev/null 2>&1; }

if have curl; then
  DL()   { curl -fSL --retry 3 -o "$2" "$1"; }        # url dest
  FETCH(){ curl -fsSL "$1"; }                          # url -> stdout
elif have wget; then
  DL()   { wget -q -O "$2" "$1"; }
  FETCH(){ wget -qO- "$1"; }
else
  die "need curl or wget on PATH"
fi

# ---- platform detection --------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$arch" in
  arm64|aarch64) arch="arm64" ;;
  x86_64|amd64)  arch="x64" ;;
  *) die "unsupported CPU architecture: $arch" ;;
esac

# Per-platform: the substring that identifies the right asset name, and a
# human label. The app is only built for macOS (arm64/x64) and Linux x64.
case "$os" in
  Darwin)
    PLATFORM="macos"
    OS_LABEL="macOS"
    case "$arch" in
      arm64) ASSET_MATCH="_aarch64.dmg" ;;
      x64)   ASSET_MATCH="_x64.dmg" ;;
    esac
    ;;
  Linux)
    PLATFORM="linux"
    OS_LABEL="Linux"
    case "$arch" in
      x64) ASSET_MATCH="_amd64.AppImage" ;;
      *)   die "no Linux $arch app build. Download from ${RELEASES_URL}" ;;
    esac
    ;;
  *)
    die "unsupported OS '$os'. On Windows use the .msi/.exe from ${RELEASES_URL}"
    ;;
esac

info "Detected ${OS_LABEL} ${arch} — will fetch the '*${ASSET_MATCH}' asset"

# ---- resolve release -----------------------------------------------------
gh_get() { # path -> release JSON on stdout
  local url="https://api.github.com/repos/${REPO}/$1"
  if [ -n "${GITHUB_TOKEN:-}" ]; then
    if have curl; then
      curl -fsSL -H "Authorization: Bearer ${GITHUB_TOKEN}" "$url"
    else
      wget -qO- --header="Authorization: Bearer ${GITHUB_TOKEN}" "$url"
    fi
  else
    FETCH "$url"
  fi
}

if [ -n "$TAG" ]; then
  CHANNEL_LABEL="tagged"
  info "Resolving release tag '$TAG'…"
  RELEASE_JSON="$(gh_get "releases/tags/${TAG}")" || die "no release tagged '$TAG' in $REPO"
elif [ "$CHANNEL" = "nightly" ]; then
  CHANNEL_LABEL="nightly"
  info "Resolving nightly release (tag 'latest')…"
  RELEASE_JSON="$(gh_get "releases/tags/latest")" || die "could not fetch nightly release"
else
  CHANNEL_LABEL="stable"
  info "Resolving latest stable release…"
  RELEASE_JSON="$(gh_get "releases/latest")" || die "could not fetch latest stable release"
fi

# ---- extract fields ------------------------------------------------------
# Prefer jq; fall back to grep/sed so the script has no hard dependency on it.
if have jq; then
  RELEASE_TAG="$(printf '%s' "$RELEASE_JSON" | jq -r '.tag_name // empty')"
  ASSET_URL="$(printf '%s' "$RELEASE_JSON" \
    | jq -r --arg m "$ASSET_MATCH" \
        '.assets[] | select(.name | endswith($m)) | .browser_download_url' \
    | head -n1)"
else
  RELEASE_TAG="$(printf '%s' "$RELEASE_JSON" \
    | grep -o '"tag_name"[[:space:]]*:[[:space:]]*"[^"]*"' | head -n1 \
    | sed 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/')"
  # Every asset download URL, one per line; keep the one ending in ASSET_MATCH.
  ASSET_URL="$(printf '%s' "$RELEASE_JSON" \
    | grep -o '"browser_download_url"[[:space:]]*:[[:space:]]*"[^"]*"' \
    | sed 's/.*"browser_download_url"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/' \
    | grep -F "$ASSET_MATCH" | head -n1)"
fi

[ -n "$ASSET_URL" ] || die "no '*${ASSET_MATCH}' asset in release ${RELEASE_TAG:-?}. Browse ${RELEASES_URL}"

ASSET_NAME="${ASSET_URL##*/}"
info "Resolved ${CHANNEL_LABEL} release: ${RELEASE_TAG:-?} — asset ${ASSET_NAME}"

# ---- download ------------------------------------------------------------
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
info "Downloading…"
DL "$ASSET_URL" "$TMP/$ASSET_NAME"

# App assets are signed with Tauri's minisign (.sig) updater key, not SHA-256
# sums, so there is nothing to checksum-verify here without the embedded public
# key. The download is over HTTPS.

# ---- install -------------------------------------------------------------
install_macos() {
  local dmg="$TMP/$ASSET_NAME" dir="${INSTALL_DIR:-/Applications}"
  # Fall back to a per-user location if the target isn't writable.
  if [ ! -w "$dir" ]; then
    if [ -z "$INSTALL_DIR" ] && mkdir -p "$HOME/Applications" 2>/dev/null; then
      warn "$dir is not writable; installing to ~/Applications instead"
      dir="$HOME/Applications"
    else
      die "$dir is not writable (re-run with --dir <path> or sudo)"
    fi
  fi

  local mnt="$TMP/mnt"
  mkdir -p "$mnt"
  info "Mounting ${ASSET_NAME}…"
  hdiutil attach -nobrowse -quiet "$dmg" -mountpoint "$mnt"
  # shellcheck disable=SC2064
  trap "hdiutil detach '$mnt' -quiet >/dev/null 2>&1 || true; rm -rf '$TMP'" EXIT

  local app
  app="$(/bin/ls -d "$mnt"/*.app 2>/dev/null | head -n1)"
  [ -n "$app" ] || die "no .app found inside the dmg"

  info "Installing $(basename "$app") to ${dir}…"
  rm -rf "$dir/$(basename "$app")"
  cp -R "$app" "$dir/"
  hdiutil detach "$mnt" -quiet >/dev/null 2>&1 || true
  trap 'rm -rf "$TMP"' EXIT

  # Clear the quarantine flag so Gatekeeper doesn't block the unsigned build.
  xattr -dr com.apple.quarantine "$dir/$(basename "$app")" >/dev/null 2>&1 || true

  APP_PATH="$dir/$(basename "$app")"
  info "Installed: $APP_PATH"
  echo
  echo "Launch it with:  open \"$APP_PATH\""
}

install_linux() {
  local dir="${INSTALL_DIR:-$HOME/.local/bin}"
  mkdir -p "$dir"
  local dest="$dir/polyclient"
  info "Installing AppImage to ${dest}…"
  cp "$TMP/$ASSET_NAME" "$dest"
  chmod +x "$dest"
  info "Installed: $dest"
  case ":$PATH:" in
    *":$dir:"*) : ;;
    *) warn "$dir is not on your PATH — add it, or run the app with: $dest" ;;
  esac
  echo
  echo "Launch it with:  polyclient   (or: $dest)"
}

case "$PLATFORM" in
  macos) install_macos ;;
  linux) install_linux ;;
esac

# ---- next steps ----------------------------------------------------------
echo
info "Done. Next: open polyclient, then add database/queue plugins from the sidebar's"
echo "    \"⤓ Install plugin\" dialog — no separate download needed."
