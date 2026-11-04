#!/bin/bash
# Writes a new Heimdall update signing key pair into the folder given (created 0700; the secret key 0600) and prints
# the public key, its id and the Rust constant to paste into crates/update/src/signing.rs. Never prints the secret
# key; refuses to overwrite an existing pair. Keep the folder out of every repository.
#
#   scripts/update_keygen.sh <folder outside the repository>
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH" CC=/usr/bin/clang CXX=/usr/bin/clang++
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target/release-candidate}"
DIR="${1:?Usage: scripts/update_keygen.sh <folder outside the repository>}"
mkdir -p "$DIR"
DIR="$(cd "$DIR" && pwd)"
case "$DIR/" in "$PWD"/*) echo "Refusing a key folder inside the repository: $DIR" >&2; exit 1 ;; esac
cargo build --release -p heimdall-update --bin heimdall-update-sign
"$CARGO_TARGET_DIR/release/heimdall-update-sign" keygen "$DIR"
