#!/bin/sh
# macOS / Linux: build from source and play (double-click on macOS, or run in a terminal).
# Put this folder inside the game folder (the one with REKO3.COM).
cd "$(dirname "$0")" || exit 1
if [ ! -f ../REKO3.COM ]; then
  echo "Game files not found: put this eiketsu-rt folder inside the game folder (the folder that has REKO3.COM)."
  exit 1
fi
command -v cargo >/dev/null || { echo "cargo not found. Install Rust: https://rustup.rs"; exit 1; }
cargo build --release && ./target/release/eiketsu-rt --game ..
