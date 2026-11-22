#!/usr/bin/env bash
# Reproducible baseline build of the single-threaded CPython 3.14 wasm runtime.
#
# This re-links the PREBUILT CPython/WASI base libraries
# (crates/mcpone-python-runtime/libs/*.zst) together with our Rust guest
# bridge into the final WASI Preview2 component `runtime.wasm`.
#
# It does NOT recompile libpython from source — see RUNTIME-BUILD.md for the
# from-source CPython recipe (Stage 2). This is the Stage 1 baseline: a
# reproducible re-link of the shipped libs.
#
# Verified working on macOS arm64 with:
#   - wasi-sdk-27.0-arm64-macos (clang 20.1.8-wasi-sdk)
#   - rustup nightly with rust-src + target wasm32-wasip1
#   - wasm-tools 1.244.0
#
# Usage:
#   ./scripts/build-runtime.sh            # build runtime.wasm
#   ./scripts/build-runtime.sh --run      # build, then run print(1+1) via host probe
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Locate WASI SDK. build.rs looks at WASI_SDK_PATH first, then mise, then
# <workspace>/.wasi-sdk. In a worktree the workspace .wasi-sdk lives in the
# MAIN checkout, so we point WASI_SDK_PATH at it explicitly.
if [[ -z "${WASI_SDK_PATH:-}" ]]; then
  for cand in \
    "$ROOT/.wasi-sdk"/wasi-sdk-*-arm64-macos \
    "$ROOT/.wasi-sdk"/wasi-sdk-*-x86_64-linux \
    "$ROOT/.wasi-sdk/wasi-sdk-27.0-arm64-macos"
  do
    if [[ -x "$cand/bin/clang" ]]; then
      export WASI_SDK_PATH="$cand"
      break
    fi
  done
fi
if [[ -z "${WASI_SDK_PATH:-}" || ! -x "$WASI_SDK_PATH/bin/clang" ]]; then
  echo "ERROR: WASI SDK not found. Set WASI_SDK_PATH or run scripts/install-wasi-sdk.sh" >&2
  exit 1
fi
echo "Using WASI_SDK_PATH=$WASI_SDK_PATH"
"$WASI_SDK_PATH/bin/clang" --version | head -1

# The build.rs only runs the full link when BUILD_MCPONE_PY_RUNTIME is set.
# It: (1) builds the guest bridge staticlib (nightly build-std, wasm32-wasip1),
# (2) compiles clock_stubs.c with wasi-sdk clang, (3) links libmcpone_python_runtime.so,
# (4) generates wit-dylib bindings, (5) decompresses libs/*.zst and links everything
# into crates/mcpone-python-runtime/runtime.wasm via wit_component::Linker.
BUILD_MCPONE_PY_RUNTIME=1 cargo build -p mcpone-python-runtime --release

OUT="$ROOT/crates/mcpone-python-runtime/runtime.wasm"
echo "Built: $OUT"
ls -l "$OUT"

if [[ "${1:-}" == "--run" ]]; then
  : "${PYTHON_STDLIB:?Set PYTHON_STDLIB to a CPython 3.14 wasi stdlib dir (has encodings/)}"
  cargo build -p python-host-probe --release
  PY_RUNTIME_COMPONENT="$OUT" "$ROOT/target/release/python-host-probe" 'print(1+1)'
fi
