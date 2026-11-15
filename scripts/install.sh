#!/bin/sh
# Installs tonewell and signs it with the Developer ID certificate. macOS ties the
# background service's audio-recording consent to the code signature; an
# unsigned build gets a new one each time, so the consent prompt would return
# after every rebuild.
set -eu
cd "$(dirname "$0")/.."
cargo install --path .
codesign --force --sign "Developer ID Application" --identifier dev.tonewell.cli "$HOME/.cargo/bin/tonewell"
codesign --verify --strict "$HOME/.cargo/bin/tonewell"
