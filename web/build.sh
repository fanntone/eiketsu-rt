#!/bin/sh
# Build the browser version into web/www (then serve that folder, e.g.
# `python -m http.server 8000 --directory web/www`).
# Needs: rustup target add wasm32-wasip1
set -e
cd "$(dirname "$0")/.."
cargo build --profile web -p eiketsu-web --target wasm32-wasip1
cp target/wasm32-wasip1/web/eiketsu_web.wasm web/www/
echo "built web/www/eiketsu_web.wasm"
