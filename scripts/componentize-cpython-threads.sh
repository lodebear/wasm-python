#!/usr/bin/env bash
# Stage 3: Attempt to componentize the STATIC THREADED CPython core module
# (from Stage 2) into a WASI preview2 COMPONENT whose inner core module
# EXPORTS its shared memory (the P4 shared-everything-linking topology:
# provider exports shared memory; a worker thread would import it).
#
# This script reproduces the full Stage-3 wall chain with wasm-tools 1.244.0.
# It is INTENTIONALLY a sequence of attempts, each surfacing the next blocker,
# culminating in the hard component-model wall.
#
#   wasm-tools 1.244.0 (wasmparser 0.244 validator)
#   wasi-sdk-27 (clang 20.1.8); reactor adapter from libs/decompressed/
#
# Inputs:
#   threads-build/cpython/cross-build/wasm32-wasi/        (Stage 2 build tree)
#     - Programs/python.o, libpython3.14.a, Modules/_hacl/*, _decimal, expat
#   crates/mcpone-python-runtime/libs/decompressed/wasi_snapshot_preview1.reactor.wasm
#
# Outputs (under threads-stage3/, scratch under $OUT):
#   python-threads-selfmem.wasm   - threaded core module that DEFINES+EXPORTS
#                                   its own shared memory (no env.memory import)
#   wasi_thread_stub.wasm         - stub satisfying `wasi.thread-spawn`
#   adapter-shared.wasm           - reactor adapter patched to import SHARED env.memory
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CP="$ROOT/threads-build/cpython/cross-build/wasm32-wasi"
ADAPTER="$ROOT/crates/mcpone-python-runtime/libs/decompressed/wasi_snapshot_preview1.reactor.wasm"
OUT="${OUT:-$ROOT/threads-stage3/scratch}"
mkdir -p "$OUT"

: "${WASI_SDK_PATH:=$ROOT/.wasi-sdk/wasi-sdk-27.0-arm64-macos}"
SDK="$WASI_SDK_PATH"

echo "## STEP 0: baseline shape of the Stage-2 threaded module (IMPORTS env.memory shared)"
wasm-tools print "$CP/python.wasm" 2>/dev/null \
  | grep -E 'import "env" "memory"|export "memory"|import "wasi" "thread-spawn"|export "_start"' || true

echo
echo "## WALL 1: component new on the as-built module fails on wasi.thread-spawn"
echo "## (wasi:thread-spawn is NOT a WIT interface; the component model can't bind it)"
wasm-tools component new "$CP/python.wasm" --adapt "wasi_snapshot_preview1=$ADAPTER" -o /dev/null \
  2>&1 | sed 's/^/   /' || true

echo
echo "## Build a stub adapter that satisfies wasi.thread-spawn so we can get past WALL 1"
cat > "$OUT/wasi_thread_stub.wat" <<'EOF'
(module
  (func (export "thread-spawn") (param i32) (result i32)
    i32.const -1))
EOF
wasm-tools parse "$OUT/wasi_thread_stub.wat" -o "$OUT/wasi_thread_stub.wasm"

echo
echo "## WALL 2: with thread-spawn stubbed, the module still IMPORTS env.memory"
echo "## => 'main module cannot import memory' (single-module reactor flow assumes the"
echo "##    main module DEFINES+EXPORTS its own memory; wasi-threads --import-memory breaks this)"
wasm-tools component new "$CP/python.wasm" \
  --adapt "wasi_snapshot_preview1=$ADAPTER" \
  --adapt "wasi=$OUT/wasi_thread_stub.wasm" -o /dev/null \
  2>&1 | sed 's/^/   /' || true

echo
echo "## FIX for WALL 2: RELINK the threaded libpython WITHOUT -Wl,--import-memory"
echo "## (keep --shared-memory --export-memory). The module then DEFINES+EXPORTS its"
echo "## own shared memory: the P4 'provider exports shared memory' shape."
( cd "$CP" && "$SDK/bin/clang" \
    --target=wasm32-wasi-threads -pthread \
    -Wl,--shared-memory -Wl,--export-memory -Wl,--max-memory=10485760 -Wl,--stack-first \
    -o "$OUT/python-threads-selfmem.wasm" \
    Programs/python.o libpython3.14.a \
    Modules/_hacl/libHacl_Hash_BLAKE2.a Modules/_hacl/libHacl_Hash_SHA3.a \
    Modules/_hacl/libHacl_Hash_MD5.a Modules/_hacl/libHacl_Hash_SHA1.a \
    Modules/_hacl/libHacl_HMAC.a Modules/_hacl/libHacl_Hash_SHA2.a \
    Modules/_decimal/libmpdec/libmpdec.a Modules/expat/libexpat.a \
    -ldl -lwasi-emulated-signal -lwasi-emulated-getpid \
    -lwasi-emulated-process-clocks -lpthread )
echo "## relinked module memory shape (NO env.memory import; DEFINES+EXPORTS shared memory):"
wasm-tools print "$OUT/python-threads-selfmem.wasm" 2>/dev/null \
  | grep -E '\(memory \(|import "env" "memory"|export "memory"' | sed 's/^/   /'
wasm-tools validate --features threads "$OUT/python-threads-selfmem.wasm" && echo "   core module VALID (threads)"

echo
echo "## WALL 3: componentize the self-mem module with the STOCK reactor adapter"
echo "## => the adapter imports a NON-shared env.memory; provider exports SHARED."
echo "##    'mismatch in the shared flag for memories'"
wasm-tools component new "$OUT/python-threads-selfmem.wasm" \
  --adapt "wasi_snapshot_preview1=$ADAPTER" \
  --adapt "wasi=$OUT/wasi_thread_stub.wasm" -o /dev/null \
  2>&1 | sed 's/^/   /' || true

echo
echo "## Patch the reactor adapter's env.memory import to be SHARED (match 48 160 shared)"
wasm-tools print "$ADAPTER" -o "$OUT/adapter.wat" 2>/dev/null
sed -i.bak 's/(import "env" "memory" (memory (;0;) 0))/(import "env" "memory" (memory (;0;) 48 160 shared))/' "$OUT/adapter.wat"
wasm-tools parse "$OUT/adapter.wat" -o "$OUT/adapter-shared.wasm"

echo
echo "## WALL 4 (THE HARD ONE): componentize with the SHARED-memory adapter."
echo "## => 'canonical ABI memory is not a 32-bit linear memory /"
echo "##    mismatch in the shared flag for memories'"
echo "## The component-model canonical ABI (wasmparser cabi_memory_at) HARD-REQUIRES"
echo "## shared:false for the lifting/lowering memory. A wasi-threads module has only"
echo "## ONE memory and it MUST be shared. => architecturally cannot componentize today."
wasm-tools component new "$OUT/python-threads-selfmem.wasm" \
  --adapt "wasi_snapshot_preview1=$OUT/adapter-shared.wasm" \
  --adapt "wasi=$OUT/wasi_thread_stub.wasm" -o /dev/null \
  2>&1 | sed 's/^/   /' || true

echo
echo "## CONTROL: a trivial module that DEFINES+EXPORTS a shared memory DOES componentize+validate,"
echo "## proving the spec supports a component whose inner core module exports shared memory --"
echo "## the wall is specifically the canonical-ABI memory, not shared memory per se."
cat > "$OUT/shared_mem_reactor.wat" <<'EOF'
(module
  (memory (export "memory") 1 2 shared)
  (func (export "ping") (result i32) i32.const 42))
EOF
wasm-tools parse "$OUT/shared_mem_reactor.wat" -o "$OUT/shared_mem_reactor.wasm"
wasm-tools component new "$OUT/shared_mem_reactor.wasm" -o "$OUT/shared_mem_reactor.component.wasm"
wasm-tools validate --features all "$OUT/shared_mem_reactor.component.wasm" \
  && echo "   shared-memory-exporting reactor COMPONENT VALID"
wasm-tools print "$OUT/shared_mem_reactor.component.wasm" 2>/dev/null \
  | grep -E '\(memory|export "memory"' | sed 's/^/   /'

echo
echo "## SUMMARY: P4 memory topology (provider exports shared memory) is reachable at the"
echo "## CORE level and survives into a component. The componentization of the THREADED"
echo "## CPython REACTOR is blocked because the component canonical ABI forbids using the"
echo "## (only, shared) memory for argument lifting/lowering. See STAGE3-WALL.md."
