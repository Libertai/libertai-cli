#!/bin/sh
set -eu

# LibertAI CLI - Debian / Ubuntu bootstrap.
# Usage: curl -fsSL https://apt.libertai.io/install.sh | sudo sh
#
# This intentionally installs the latest GitHub Release .deb directly. The
# signed APT repository can replace this script once apt.libertai.io has a
# valid Pages certificate, GPG key, and package index.

REPO="Libertai/libertai-cli"
API_URL="https://api.github.com/repos/${REPO}/releases/latest"

err() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

need() {
    command -v "$1" >/dev/null 2>&1 || err "missing required tool: $1"
}

[ "$(id -u)" -eq 0 ] || err "run this installer as root, for example: curl -fsSL https://apt.libertai.io/install.sh | sudo sh"

need apt-get
need curl
need mktemp
need sed
need uname

case "$(uname -m)" in
    x86_64|amd64) ;;
    *) err "only amd64 Debian/Ubuntu packages are published today. Use the universal installer or build from source." ;;
esac

tmp_json="$(mktemp -t libertaiXXXXXX.json)"
tmp_deb="$(mktemp -t libertaiXXXXXX.deb)"
trap 'rm -f "$tmp_json" "$tmp_deb"' EXIT

printf 'Resolving latest LibertAI CLI release...\n'
curl -fsSL "$API_URL" -o "$tmp_json"

deb_url="$(
    sed -n 's/.*"browser_download_url": "\(https:[^"]*libertai-cli_[^"]*_amd64\.deb\)".*/\1/p' "$tmp_json" | head -n 1
)"
[ -n "$deb_url" ] || err "could not find an amd64 .deb asset in the latest GitHub Release"

printf 'Downloading %s...\n' "$deb_url"
curl -fL --progress-bar -o "$tmp_deb" "$deb_url"

# Make the temp file readable so apt skips its "unsandboxed" notice.
chmod 0644 "$tmp_deb"

printf 'Installing LibertAI CLI...\n'
apt-get update
apt-get install -y "$tmp_deb"

printf 'LibertAI CLI installed. Run '\''libertai --help'\'' to get started.\n'
