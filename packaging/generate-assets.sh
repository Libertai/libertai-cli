#!/bin/sh
# Regenerate the committed shell completions + man page under packaging/.
#
# These files are baked into the .deb (see [package.metadata.deb] assets in
# Cargo.toml) so `cargo deb` works without extra CI steps. They are kept in
# sync with the CLI surface by tests/probes_completions.rs — if that probe
# fails after you change src/cli.rs (or bump the version), run this script
# from the repo root and commit the result.
#
# Homebrew does NOT use these files: the formula template generates
# completions and the man page from the installed binary at install time.

set -eu

cd "$(dirname "$0")/.."

cargo build --quiet --bin libertai
BIN=target/debug/libertai

mkdir -p packaging/completions packaging/man

"$BIN" completions bash > packaging/completions/libertai.bash
"$BIN" completions zsh  > packaging/completions/_libertai
"$BIN" completions fish > packaging/completions/libertai.fish
"$BIN" man              > packaging/man/libertai.1

echo "Regenerated packaging/completions/* and packaging/man/libertai.1"
