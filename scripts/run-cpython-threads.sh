#!/usr/bin/env bash
# Run the static THREADED CPython python.wasm core module via wasmtime with
# wasi-threads enabled. Proves the threaded build executes (and uses real
# OS threads through the wasi `thread-spawn` import).
#
# Requires:
#   - wasmtime 41+ built with the `threads` feature (CLI). Install with:
#       cargo install wasmtime-cli --version '^41' --features threads
#   - the threaded python.wasm at cross-build/wasm32-wasi/python.wasm
#     (built by scripts/build-cpython-threads.sh) OR decompressed from
#     threads-artifacts/python-threads.wasm.zst
#   - the CPython checkout (for the stdlib in Lib/) mapped to the guest /
#
# Usage:
#   WASMTIME=/path/to/wasmtime ./scripts/run-cpython-threads.sh 'print(1+1)'
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CP="$ROOT/threads-build/cpython"
PW="$CP/cross-build/wasm32-wasi/python.wasm"
SYSCONFIG="/cross-build/wasm32-wasi/build/lib.wasi-wasm32-3.14"

: "${WASMTIME:?Set WASMTIME to a wasmtime 41+ CLI built with the threads feature}"
PROG="${1:-print(1+1)}"

exec "$WASMTIME" run \
  -W max-wasm-stack=16777216 \
  -W threads=y -W shared-memory=y \
  -S threads=y \
  --dir "$CP::/" \
  --env "PYTHONPATH=$SYSCONFIG" \
  "$PW" -c "$PROG"
