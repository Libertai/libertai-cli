#!/bin/bash
set -euo pipefail

# LibertAI CLI — APT repository setup (Debian / Ubuntu)
# Usage: curl -fsSL https://apt.libertai.io/install.sh | sudo bash

KEYRING_PATH="/usr/share/keyrings/libertai.gpg"
SOURCES_PATH="/etc/apt/sources.list.d/libertai.sources"

echo "Adding LibertAI APT repository..."

# Download and install the signing key
curl -fsSL https://apt.libertai.io/gpg.key | gpg --dearmor -o "$KEYRING_PATH"

# Add the repository (deb822 format, GitHub Pages as fallback)
cat > "$SOURCES_PATH" <<EOF
Types: deb
URIs: https://apt.libertai.io https://libertai.github.io/apt
Suites: stable
Components: main
Signed-By: $KEYRING_PATH
Architectures: amd64
EOF

# Update and install
apt-get update -o Dir::Etc::sourcelist="$SOURCES_PATH" -o Dir::Etc::sourceparts="-"
apt-get install -y libertai-cli

echo "LibertAI CLI installed. Run 'libertai --help' to get started."
