#!/usr/bin/env bash
# Stage 2: Build a STATIC, THREADED CPython 3.14 for wasm32-wasip1-threads.
#
# Uses CPython's own Tools/wasm/wasi build driver, configured with
# --enable-wasm-pthreads (which sets -target wasm32-wasi-threads -pthread and
# the --import-memory/--export-memory/shared-memory link flags) and WITHOUT
# --enable-shared (so we get a STATIC libpython.a + static python.wasm command
# module linked against the threaded wasi-libc static sysroot).
#
# Steps:
#   1. native build python (cross-build prerequisite)
#   2. cross-configure for wasm32-wasip1-threads + pthreads
#   3. make -> static python.wasm
#
# Env:
#   WASI_SDK_PATH must point at wasi-sdk-27 (threaded sysroot lives at
#   $WASI_SDK_PATH/share/wasi-sysroot/lib/wasm32-wasip1-threads).
#   MAX_MEMORY   max linear-memory bytes for the shared memory (default 256 MiB).
#
# NOTE on the memory cap: CPython's configure hardcodes
# `-Wl,--max-memory=10485760` (10 MiB) under --enable-wasm-pthreads. A host
# cannot grow a shared memory past the module's declared maximum, so 10 MiB
# OOMs heavy stdlib (concurrent.futures.ThreadPoolExecutor, urllib/ssl, pip).
# We append a larger `-Wl,--max-memory` to LDFLAGS_NODIST in configure-host;
# wasm-ld honours the LAST --max-memory, so this overrides the 10 MiB default
# without patching CPython's configure. 268435456 = 256 MiB = 4096 pages
# (matches ffmpeg's cap).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CP="$ROOT/threads-build/cpython"

: "${WASI_SDK_PATH:=$ROOT/.wasi-sdk/wasi-sdk-27.0-arm64-macos}"
export WASI_SDK_PATH

# Raise the shared-memory cap above CPython's 10 MiB default (see note above).
: "${MAX_MEMORY:=268435456}"
export MAX_MEMORY

echo "WASI_SDK_PATH=$WASI_SDK_PATH"
echo "CPython source: $CP"

cd "$CP"

case "${1:-all}" in
  native)
    # Native build python (host tools needed to cross-build).
    python3 Tools/wasm/wasi configure-build-python --quiet
    python3 Tools/wasm/wasi make-build-python --quiet
    ;;
  configure-host)
    # zlib deps (built by scripts/build-zlib-threads.sh) so CPython's configure
    # detects zlib (it has no zlib.pc for pkg-config, so it falls back to the
    # zlib.h header + `gzread in -lz` link check; -I/-L make both succeed). This
    # turns MODULE_ZLIB_STATE on so the `zlib` C extension is compiled into
    # libpython3.14.a — needed by pip (vendored `rich` imports zlib; ZipFile
    # ZIP_DEFLATED needs zlib). Mirrors the preview2 component build, which links
    # deps/lib/libz.a (RUNTIME-BUILD.md §3).
    DEPS="$ROOT/threads-build/deps"
    if [ ! -f "$DEPS/lib/libz.a" ]; then
      echo "ERROR: missing $DEPS/lib/libz.a; run scripts/build-zlib-threads.sh first" >&2
      exit 1
    fi
    # Append a larger --max-memory after CPython's hardcoded 10 MiB one; wasm-ld
    # uses the last --max-memory, so this raises the shared-memory cap.
    # NOTE: host-triple is wasm32-wasi (NOT wasm32-wasip1-threads): config.sub
    # does not recognize an "os=threads", and --enable-wasm-pthreads is what
    # retargets the toolchain to `-target wasm32-wasi-threads -pthread` + shared
    # memory. The cross-build output therefore lives in cross-build/wasm32-wasi.
    CFLAGS="${CFLAGS:-} -I${DEPS}/include" \
    LDFLAGS="${LDFLAGS:-} -L${DEPS}/lib" \
    LDFLAGS_NODIST="${LDFLAGS_NODIST:-} -Wl,--max-memory=${MAX_MEMORY}" \
    python3 Tools/wasm/wasi configure-host \
      --host-triple wasm32-wasi \
      -- --enable-wasm-pthreads --disable-test-modules
    ;;
  make-host)
    python3 Tools/wasm/wasi make-host --host-triple wasm32-wasi
    ;;
  all)
    "$0" native
    "$0" configure-host
    "$0" make-host
    ;;
  *)
    echo "usage: $0 [native|configure-host|make-host|all]" >&2
    exit 1
    ;;
esac
