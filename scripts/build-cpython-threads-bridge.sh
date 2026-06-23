#!/usr/bin/env bash
# Stage C: statically link the mcpone-python-wasm-runtime guest bridge (socket/
# TLS/execute/stdout shim) INTO the static THREADED libpython core module, with
# its host imports bound to PLAIN CORE IMPORTS (wasi_snapshot_preview1 +
# monocp.tcp_*/dns_*/tls_*) rather than component/wit interfaces.
#
# This is "Path A" from the parity plan: the guest bridge resolves networking by
# direct `#[link(wasm_import_module="monocp")]` extern "C" core imports (built
# under the crate's `core-net` feature, see crates/.../src/core_net.rs), and a
# small C main() (threads-build/mcpone_threads_main.c) calls the bridge's
# `mcpone_threads_main()` entry so the threaded core module is a runnable COMMAND
# that registers the _mcpone builtins + socket/ssl shims via initialize_python().
#
# Outputs (under threads-stage3/bridge/):
#   python-threads-bridge.wasm         - --import-memory (env.memory shared);
#                                        the production preview1 shape, run under
#                                        wasmtime -S threads=y (wasi-threads host
#                                        supplies env.memory + thread-spawn).
#   python-threads-bridge-selfmem.wasm - DEFINES+EXPORTS its own shared memory;
#                                        used for the standalone import smoke
#                                        because the wasmtime CLI cannot both
#                                        host wasi-threads AND --preload a custom
#                                        `monocp` module (see STAGE-C-NOTES.md).
#
# Requires: wasi-sdk-27, wasm-tools, and the Stage-2 threaded build tree under
# threads-build/cpython/cross-build/wasm32-wasi (built by
# scripts/build-cpython-threads.sh).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CP="$ROOT/threads-build/cpython/cross-build/wasm32-wasi"
RUNTIME_CRATE="$ROOT/crates/mcpone-python-wasm-runtime"
OUT="${OUT:-$ROOT/threads-stage3/bridge}"
mkdir -p "$OUT"

: "${WASI_SDK_PATH:=$ROOT/.wasi-sdk/wasi-sdk-27.0-arm64-macos}"
SDK="$WASI_SDK_PATH"
: "${MAX_MEMORY:=268435456}"

echo "## 1) Build the guest bridge staticlib for wasm32-wasip1-threads (core-net + threads-command)"
( cd "$RUNTIME_CRATE" && rustup run nightly cargo build \
    -Z build-std=panic_abort,std \
    --target wasm32-wasip1-threads --release \
    --features threads-command )
STATICLIB="$ROOT/target/wasm32-wasip1-threads/release/libmcpone_python_wasm_runtime.a"

echo "## 2) Compile the C command entry (calls mcpone_threads_main())"
"$SDK/bin/clang" --target=wasm32-wasi-threads -pthread -O2 -c \
  "$ROOT/threads-stage3/mcpone_threads_main.c" -o "$OUT/mcpone_threads_main.o"

# Shared archive list: threaded libpython + the hacl/decimal/expat static deps.
ARCHIVES=(
  "$CP/libpython3.14.a"
  "$CP/Modules/_hacl/libHacl_Hash_BLAKE2.a" "$CP/Modules/_hacl/libHacl_Hash_SHA3.a"
  "$CP/Modules/_hacl/libHacl_Hash_MD5.a"    "$CP/Modules/_hacl/libHacl_Hash_SHA1.a"
  "$CP/Modules/_hacl/libHacl_HMAC.a"        "$CP/Modules/_hacl/libHacl_Hash_SHA2.a"
  "$CP/Modules/_decimal/libmpdec/libmpdec.a" "$CP/Modules/expat/libexpat.a"
)
LIBS=( -ldl -lwasi-emulated-signal -lwasi-emulated-getpid -lwasi-emulated-process-clocks -lpthread )

echo "## 3a) Link the production shape (--import-memory: env.memory shared)"
"$SDK/bin/clang" \
  --target=wasm32-wasi-threads -pthread \
  -Wl,--shared-memory -Wl,--export-memory -Wl,--max-memory="$MAX_MEMORY" -Wl,--import-memory \
  -o "$OUT/python-threads-bridge.wasm" \
  "$OUT/mcpone_threads_main.o" "$STATICLIB" "${ARCHIVES[@]}" "${LIBS[@]}"

echo "## 3b) Link the self-memory shape (DEFINES+EXPORTS its own shared memory)"
"$SDK/bin/clang" \
  --target=wasm32-wasi-threads -pthread \
  -Wl,--shared-memory -Wl,--export-memory -Wl,--max-memory="$MAX_MEMORY" \
  -o "$OUT/python-threads-bridge-selfmem.wasm" \
  "$OUT/mcpone_threads_main.o" "$STATICLIB" "${ARCHIVES[@]}" "${LIBS[@]}"

echo
echo "## SHAPE PROOF (wasm-tools)"
for w in python-threads-bridge.wasm python-threads-bridge-selfmem.wasm; do
  echo "### $w"
  wasm-tools print "$OUT/$w" 2>/dev/null | grep -E \
    '\(memory \(;0;\)|import "env" "memory"|export "memory"|export "wasi_thread_start"|import "wasi" "thread-spawn"|export "_start"' \
    | sed 's/^[[:space:]]*/   /'
  echo "   monocp.* imports: $(wasm-tools print "$OUT/$w" 2>/dev/null | grep -cE 'import "monocp"')"
  wasm-tools validate --features threads "$OUT/$w" && echo "   core module VALID (threads)"
  echo
done
echo "## Done. Run the import smoke with scripts/run-cpython-threads-bridge.sh"
