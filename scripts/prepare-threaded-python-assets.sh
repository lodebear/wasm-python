#!/usr/bin/env bash
# Stage 2 (reproducible install): produce the THREADED preview1 Python WASI
# command assets — the bridge core module + its merged stdlib — from the
# committed build scripts, in a single stable output directory the backend's
# `make wasm-tools PYTHON_FLAVOR=threaded` can consume directly.
#
# This replaces the old hand-assembled, untracked ~52 MiB
# `.threaded-python-assets/` install under the backend worktree: every output
# here is produced from committed inputs (build-cpython-threads-bridge.sh + the
# cross-build stdlib tree), so a clean checkout + the documented prereqs can
# rebuild it.
#
# OUTPUTS (under threads-stage3/assets/, gitignored — derived):
#   python-threads-bridge.wasm   the threaded bridge CORE MODULE (import-memory
#                                shape; runs under the preview1 thread runner).
#   stdlib/                       the merged threaded stdlib:
#                                   cpython/Lib/*            (minus __pycache__)
#                                   + _sysconfigdata__wasi_wasm32-wasi.py
#                                   + _sysconfig_vars__wasi_wasm32-wasi.json
#                                     (the two cross-build sysconfig files)
#                                   + site-packages/README.txt (empty pkg dir,
#                                     mirrors the preview2 component layout)
#
# These map 1:1 onto the backend profile's python `path` + `stdlib` asset.
#
# PREREQS (NOT produced here; see threads-artifacts/README.md + STAGE-C-NOTES.md):
#   - wasi-sdk-27 at $WASI_SDK_PATH (default below). Threaded sysroot lives at
#     $WASI_SDK_PATH/share/wasi-sysroot/lib/wasm32-wasip1-threads.
#   - the Stage-2 threaded CPython build tree under threads-build/ (~378 MiB,
#     gitignored). Produced by scripts/build-cpython-threads.sh; the cross-build
#     output dir cross-build/wasm32-wasi/ must contain libpython3.14.a and
#     build/lib.wasi-wasm32-3.14/_sysconfig*.
#   - wasm-tools + a nightly rust toolchain with the wasm32-wasip1-threads target
#     (build-cpython-threads-bridge.sh builds the guest bridge staticlib).
#
# USAGE:
#   scripts/prepare-threaded-python-assets.sh            # build bridge + stdlib
#   SKIP_BRIDGE_BUILD=1 scripts/prepare-threaded-python-assets.sh
#                                                        # reuse an existing
#                                                        # threads-stage3/bridge
#                                                        # build (just (re)merge
#                                                        # the stdlib + copy)
#   USE_COMMITTED_ZST=1 scripts/prepare-threaded-python-assets.sh
#                                                        # decompress the COMMITTED
#                                                        # threads-stage3/python-
#                                                        # threads-bridge.wasm.zst
#                                                        # instead of relinking the
#                                                        # bridge (no rust/clang
#                                                        # needed for the .wasm; the
#                                                        # stdlib still comes from
#                                                        # the build tree)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CP="$ROOT/threads-build/cpython"
CROSS="$CP/cross-build/wasm32-wasi"
SYSCONFIG_DIR="$CROSS/build/lib.wasi-wasm32-3.14"
BRIDGE_DIR="$ROOT/threads-stage3/bridge"
OUT="${OUT:-$ROOT/threads-stage3/assets}"

: "${WASI_SDK_PATH:=$ROOT/.wasi-sdk/wasi-sdk-27.0-arm64-macos}"
export WASI_SDK_PATH

echo "ROOT=$ROOT"
echo "OUT=$OUT"

# ---------------------------------------------------------------------------
# 1) Build (or recover) the bridge core module.
# ---------------------------------------------------------------------------
COMMITTED_ZST="$ROOT/threads-stage3/python-threads-bridge.wasm.zst"
if [ "${USE_COMMITTED_ZST:-0}" = "1" ]; then
  echo "## USE_COMMITTED_ZST=1 — decompressing committed $COMMITTED_ZST"
  if [ ! -f "$COMMITTED_ZST" ]; then
    echo "ERROR: missing committed bridge $COMMITTED_ZST" >&2
    exit 1
  fi
  mkdir -p "$BRIDGE_DIR"
  zstd -dqf "$COMMITTED_ZST" -o "$BRIDGE_DIR/python-threads-bridge.wasm"
elif [ "${SKIP_BRIDGE_BUILD:-0}" = "1" ]; then
  echo "## SKIP_BRIDGE_BUILD=1 — reusing existing $BRIDGE_DIR"
else
  echo "## 1) Building the threaded bridge core module"
  "$SCRIPT_DIR/build-cpython-threads-bridge.sh"
fi

BRIDGE_WASM="$BRIDGE_DIR/python-threads-bridge.wasm"
if [ ! -f "$BRIDGE_WASM" ]; then
  echo "ERROR: missing bridge core module at $BRIDGE_WASM" >&2
  echo "       run without SKIP_BRIDGE_BUILD=1, or build it via" >&2
  echo "       scripts/build-cpython-threads-bridge.sh" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# 2) Sanity-check the prereq stdlib inputs.
# ---------------------------------------------------------------------------
if [ ! -d "$CP/Lib" ]; then
  echo "ERROR: missing CPython stdlib source at $CP/Lib" >&2
  echo "       build the Stage-2 threaded tree first: scripts/build-cpython-threads.sh" >&2
  exit 1
fi
SYSCONFIG_PY="$SYSCONFIG_DIR/_sysconfigdata__wasi_wasm32-wasi.py"
SYSCONFIG_JSON="$SYSCONFIG_DIR/_sysconfig_vars__wasi_wasm32-wasi.json"
for f in "$SYSCONFIG_PY" "$SYSCONFIG_JSON"; do
  if [ ! -f "$f" ]; then
    echo "ERROR: missing cross-build sysconfig file $f" >&2
    echo "       run scripts/build-cpython-threads.sh make-host" >&2
    exit 1
  fi
done

# ---------------------------------------------------------------------------
# 3) Assemble the install dir.
# ---------------------------------------------------------------------------
echo "## 2) Assembling threaded python WASI command assets into $OUT"
rm -rf "$OUT"
mkdir -p "$OUT/stdlib"

cp -f "$BRIDGE_WASM" "$OUT/python-threads-bridge.wasm"

# Merged stdlib: Lib/* + cross-build sysconfig, __pycache__ stripped.
# (-a preserves the tree; we strip __pycache__ afterwards rather than excluding
# during copy so this stays portable across cp implementations.)
cp -a "$CP/Lib/." "$OUT/stdlib/"
cp -f "$SYSCONFIG_PY" "$OUT/stdlib/"
cp -f "$SYSCONFIG_JSON" "$OUT/stdlib/"
find "$OUT/stdlib" -type d -name __pycache__ -prune -exec rm -rf {} +
find "$OUT/stdlib" -type f -name '*.pyc' -delete

# Empty writable-package target so site.py is happy (mirrors preview2 layout).
mkdir -p "$OUT/stdlib/site-packages"
cat >"$OUT/stdlib/site-packages/README.txt" <<'TXT'
This directory exists so that 3rd party packages can be installed
here.  Read the source for site.py for more details.
TXT

# ---------------------------------------------------------------------------
# 4) Proof.
# ---------------------------------------------------------------------------
echo
echo "## PROOF"
echo "bridge: $OUT/python-threads-bridge.wasm"
ls -la "$OUT/python-threads-bridge.wasm"
if command -v wasm-tools >/dev/null 2>&1; then
  echo "  monocp.* imports: $(wasm-tools print "$OUT/python-threads-bridge.wasm" 2>/dev/null | grep -cE 'import "monocp"')"
  wasm-tools validate --features threads "$OUT/python-threads-bridge.wasm" \
    && echo "  core module VALID (threads)"
fi
echo "stdlib markers:"
for marker in encodings/__init__.py _sysconfigdata__wasi_wasm32-wasi.py \
              _sysconfig_vars__wasi_wasm32-wasi.json site-packages/README.txt; do
  if [ -e "$OUT/stdlib/$marker" ]; then
    echo "  OK  $marker"
  else
    echo "  MISSING  $marker" >&2
    exit 1
  fi
done
echo "  no __pycache__: $(find "$OUT/stdlib" -name __pycache__ | wc -l | tr -d ' ') dirs"
echo
echo "## Done. Point the backend at these via:"
echo "   make -C backend wasm-tools PYTHON_FLAVOR=threaded \\"
echo "     MONOCP_THREADED_PYTHON_WASM=$OUT/python-threads-bridge.wasm \\"
echo "     MONOCP_THREADED_PYTHON_STDLIB=$OUT/stdlib"
