#!/bin/bash
set -e
cd "$(dirname "$0")"
cargo build --release -p rewind-tui -p rewind-cli
cp target/release/rewind-tui ~/.local/bin/rewind-tui
cp target/release/rewind ~/.local/bin/rewind
# Ad-hoc codesign (required on macOS when launched from signed apps like Ghostty)
codesign -s - -f ~/.local/bin/rewind-tui 2>/dev/null
codesign -s - -f ~/.local/bin/rewind 2>/dev/null
echo "Installed and signed rewind-tui + rewind"
