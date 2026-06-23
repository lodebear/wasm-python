#!/usr/bin/env bash
# Standalone import smoke for the THREADED bridge core module: prove that the
# host-backed `socket` and `ssl` shims (wired through _mcpone -> the monocp.*
# plain imports) at least LOAD inside the threaded CPython core module.
#
# We run the SELF-MEMORY variant (it defines+exports its own shared memory) and
# --preload a stub `monocp` module + a stub `wasi.thread-spawn`, because the
# wasmtime CLI cannot simultaneously host wasi-threads (-S threads=y, which
# supplies env.memory + thread-spawn) AND --preload a custom `monocp` module.
# The smoke only IMPORTS the shims (never connects), so the stubs are never
# called; threads machinery (shared memory, wasi_thread_start, atomics) is still
# all present and validated. See STAGE-C-NOTES.md.
#
# Usage:
#   WASMTIME=/path/to/wasmtime ./scripts/run-cpython-threads-bridge.sh ['python code']
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CP="$ROOT/threads-build/cpython"
WASM="${WASM:-$ROOT/threads-stage3/bridge/python-threads-bridge-selfmem.wasm}"
STUB="$ROOT/threads-stage3/monocp_net_stub.wasm"
TS="$ROOT/threads-stage3/wasi_thread_spawn_stub.wasm"
SYSCONFIG="/cross-build/wasm32-wasi/build/lib.wasi-wasm32-3.14"

: "${WASMTIME:?Set WASMTIME to a wasmtime 41+ CLI built with the threads feature}"

# Build the preload stubs if missing.
[ -f "$STUB" ] || wasm-tools parse "$ROOT/threads-stage3/monocp_net_stub.wat" -o "$STUB"
if [ ! -f "$TS" ]; then
  cat > "$ROOT/threads-stage3/wasi_thread_spawn_stub.wat" <<'EOF'
(module (func (export "thread-spawn") (param i32) (result i32) i32.const -1))
EOF
  wasm-tools parse "$ROOT/threads-stage3/wasi_thread_spawn_stub.wat" -o "$TS"
fi

CODE="${1:-import socket
import ssl
print('THREAD-IMPORT-OK', socket.__name__, ssl.__name__)}"

exec "$WASMTIME" run \
  -W max-wasm-stack=16777216 \
  -W threads=y -W shared-memory=y \
  --preload "monocp=$STUB" --preload "wasi=$TS" \
  --dir "$CP::/" \
  --env "PYTHONPATH=$SYSCONFIG" \
  --env "MCPONE_CODE=$CODE" \
  "$WASM"
