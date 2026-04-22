#!/bin/sh
# LibertAI CLI — universal installer (Linux, macOS, WSL)
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Libertai/libertai-cli/main/packaging/install.sh | sh
#
# Environment overrides:
#   LIBERTAI_VERSION      — pin to a specific tag (e.g. v0.2.0). Default: latest.
#   LIBERTAI_INSTALL_DIR  — where to drop the binary. Default: $HOME/.local/bin.

set -eu

REPO="Libertai/libertai-cli"
BIN="libertai"
DEST="${LIBERTAI_INSTALL_DIR:-$HOME/.local/bin}"

err() { printf 'error: %s\n' "$*" >&2; exit 1; }
info() { printf '%s\n' "$*"; }

need() {
    command -v "$1" >/dev/null 2>&1 || err "missing required tool: $1"
}

need curl
need uname
need mktemp
need install

case "$(uname -s)" in
    Linux)  OS="linux"  ;;
    Darwin) OS="macos"  ;;
    *) err "unsupported OS: $(uname -s). Supported: Linux, macOS." ;;
esac

case "$(uname -m)" in
    x86_64|amd64) ARCH="x86_64" ;;
    arm64|aarch64)
        if [ "$OS" = "macos" ]; then
            ARCH="aarch64"
        else
            err "linux-aarch64 builds are not published yet. Install with 'cargo install libertai-cli' instead."
        fi
        ;;
    *) err "unsupported architecture: $(uname -m)." ;;
esac

ASSET="${BIN}-${OS}-${ARCH}"

if [ -n "${LIBERTAI_VERSION:-}" ]; then
    VERSION="$LIBERTAI_VERSION"
    case "$VERSION" in v*) ;; *) VERSION="v$VERSION" ;; esac
else
    info "Resolving latest release..."
    VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | sed -n 's/.*"tag_name": *"\(v[^"]*\)".*/\1/p' | head -n1)"
    [ -n "$VERSION" ] || err "could not resolve latest release from GitHub."
fi

URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET}"
SHA_URL="${URL}.sha256"

mkdir -p "$DEST" || err "could not create $DEST"

TMP="$(mktemp)"
TMP_SHA="$(mktemp)"
trap 'rm -f "$TMP" "$TMP_SHA"' EXIT

info "Downloading ${BIN} ${VERSION} (${OS}-${ARCH})..."
curl -fL --progress-bar -o "$TMP" "$URL" \
    || err "download failed: $URL"

if curl -fsSL -o "$TMP_SHA" "$SHA_URL" 2>/dev/null; then
    EXPECTED="$(awk '{print $1}' "$TMP_SHA")"
    if command -v sha256sum >/dev/null 2>&1; then
        ACTUAL="$(sha256sum "$TMP" | awk '{print $1}')"
    elif command -v shasum >/dev/null 2>&1; then
        ACTUAL="$(shasum -a 256 "$TMP" | awk '{print $1}')"
    else
        err "no sha256sum or shasum available to verify the download."
    fi
    if [ "$EXPECTED" != "$ACTUAL" ]; then
        err "checksum mismatch: expected $EXPECTED, got $ACTUAL"
    fi
    info "Checksum verified."
else
    info "Warning: no .sha256 published for $VERSION — skipping checksum verification."
fi

install -m 0755 "$TMP" "$DEST/$BIN" \
    || err "could not install binary to $DEST/$BIN"

info ""
info "Installed ${BIN} ${VERSION} to $DEST/$BIN"

case ":$PATH:" in
    *":$DEST:"*) ;;
    *)
        info ""
        info "NOTE: $DEST is not in your PATH. Add the following to your shell rc:"
        info "    export PATH=\"$DEST:\$PATH\""
        ;;
esac

info ""
info "Run '${BIN} --help' to get started, or '${BIN} login' to authenticate."
