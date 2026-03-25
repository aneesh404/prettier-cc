#!/bin/bash
set -e
cd "$(dirname "$0")"
cargo build --release -p continuum-tui -p continuum-cli
cp target/release/continuum-tui ~/.local/bin/continuum-tui
cp target/release/continuum ~/.local/bin/continuum
# Ad-hoc codesign (required on macOS when launched from signed apps like Ghostty)
codesign -s - -f ~/.local/bin/continuum-tui 2>/dev/null
codesign -s - -f ~/.local/bin/continuum 2>/dev/null
echo "Installed and signed continuum-tui + continuum"
