#!/bin/sh
# Builds the in-browser engine into web/dist (or the directory given):
#   tape.wasm        the engine, compiled to WebAssembly
#   tape-engine.js   the loader the page imports
#   data/*.bin       one file per month of BTC/USDT one-minute bars
#   check.html       a bare page that runs every strategy (serve with web/serve.sh)
# Market data must already be fetched with `tape data fetch`.
set -eu
cd "$(dirname "$0")/.."
OUT="${1:-web/dist}"
export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"

cargo build --quiet --release -p tape-web --target wasm32-unknown-unknown
cargo build --quiet --release -p tape-cli
mkdir -p "$OUT/data"
cp target/wasm32-unknown-unknown/release/tape_web.wasm "$OUT/tape.wasm"
cp web/tape-engine.js "$OUT/tape-engine.js"
cp web/check.html "$OUT/check.html"
./target/release/tape web export --from 2024-01 --to 2024-12 --out "$OUT/data" > /dev/null
echo "built $OUT: $(du -sh "$OUT" | cut -f1)"
