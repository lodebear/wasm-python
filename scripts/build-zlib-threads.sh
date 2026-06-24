#!/usr/bin/env bash
# Stage 2 dependency: cross-compile zlib (static libz.a) for the THREADED
# wasm32-wasip1-threads target with wasi-sdk-27, so CPython's `zlib` C extension
# can be enabled in the threaded cross-build.
#
# Why: the preview2 COMPONENT build links zlib in (CPython `zlib` builtin); see
# RUNTIME-BUILD.md §3 step 2 — componentize-py builds a static zlib into
# builddir/wasi/deps and links deps/lib/libz.a into libpython. The threaded
# static relink dropped it (configure reported MODULE_ZLIB_STATE=missing because
# no libz/zlib.h was on the cross toolchain search path), which breaks `pip`
# (its vendored `rich` imports `zlib`, and ZipFile(ZIP_DEFLATED) needs zlib).
#
# This script reproduces zlib for the threads target the same emconfigure-free
# way: zlib's hand-written ./configure honours CC/AR/RANLIB/CFLAGS, so we point
# it at the wasi-sdk threaded clang and the wasm32-wasi-threads target flags and
# build just the static archive. The resulting libz.a carries +atomics +shared
# target_features, matching the threaded libpython + bridge.
#
# OUTPUT (under threads-build/deps/, gitignored — derived):
#   include/zlib.h, include/zconf.h
#   lib/libz.a            (wasm32-wasip1-threads static archive)
#
# Consumed by scripts/build-cpython-threads.sh configure-host (which adds
# -I<deps>/include / -L<deps>/lib so CPython's configure detects zlib) and by
# scripts/build-cpython-threads-bridge.sh (which links lib/libz.a into the
# threaded bridge core module).
#
# PREREQS: wasi-sdk-27 at $WASI_SDK_PATH; network to fetch the pinned zlib
# tarball (cached under threads-build/deps/src once downloaded).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

: "${WASI_SDK_PATH:=$ROOT/.wasi-sdk/wasi-sdk-27.0-arm64-macos}"
SDK="$WASI_SDK_PATH"

ZLIB_VERSION="1.3.1"
ZLIB_SHA256="9a93b2b7dfdac77ceba5a558a580e74667dd6fede4585b91eefb60f03b72df23"
ZLIB_URL="https://github.com/madler/zlib/releases/download/v${ZLIB_VERSION}/zlib-${ZLIB_VERSION}.tar.gz"

DEPS="$ROOT/threads-build/deps"
SRC="$DEPS/src"
PREFIX="$DEPS"            # installs into $DEPS/{include,lib}
BUILD="$DEPS/build/zlib-${ZLIB_VERSION}"

mkdir -p "$SRC"
TARBALL="$SRC/zlib-${ZLIB_VERSION}.tar.gz"

echo "WASI_SDK_PATH=$SDK"
echo "zlib version: $ZLIB_VERSION"

# ---------------------------------------------------------------------------
# 1) Fetch + verify the pinned tarball (cached).
# ---------------------------------------------------------------------------
if [ ! -f "$TARBALL" ]; then
  echo "## Fetching $ZLIB_URL"
  curl -fsSL --retry 3 -o "$TARBALL" "$ZLIB_URL"
fi
echo "## Verifying sha256"
echo "${ZLIB_SHA256}  ${TARBALL}" | shasum -a 256 -c -

# ---------------------------------------------------------------------------
# 2) Unpack + cross-configure for wasm32-wasip1-threads.
# ---------------------------------------------------------------------------
rm -rf "$BUILD"
mkdir -p "$BUILD"
tar xzf "$TARBALL" -C "$(dirname "$BUILD")"

cd "$BUILD"
export CC="$SDK/bin/clang"
export AR="$SDK/bin/llvm-ar"
export RANLIB="$SDK/bin/llvm-ranlib"
# Match the threaded bridge link flags (atomics + shared memory toolchain).
export CFLAGS="--target=wasm32-wasi-threads -pthread -O2"
export CHOST="wasm32-wasi-threads"

echo "## configure --static --prefix=$PREFIX"
./configure --static --prefix="$PREFIX"

# ---------------------------------------------------------------------------
# 3) Build just the static archive + install headers/lib.
# ---------------------------------------------------------------------------
echo "## make libz.a"
make libz.a
echo "## make install (static)"
make install

# ---------------------------------------------------------------------------
# 4) Proof: archive is wasm + carries +atomics (threads target features).
# ---------------------------------------------------------------------------
echo
echo "## PROOF"
ls -la "$PREFIX/lib/libz.a" "$PREFIX/include/zlib.h"
TMPDIR_PROOF="$(mktemp -d)"
( cd "$TMPDIR_PROOF" && "$AR" x "$PREFIX/lib/libz.a" deflate.o )
if command -v wasm-tools >/dev/null 2>&1; then
  echo -n "  deflate.o target_features: "
  wasm-tools print "$TMPDIR_PROOF/deflate.o" 2>/dev/null \
    | grep -o "target_features.*atomics" | head -1 \
    || echo "(could not read target_features)"
fi
rm -rf "$TMPDIR_PROOF"
echo "## Done. zlib deps at $PREFIX/{include,lib}"
