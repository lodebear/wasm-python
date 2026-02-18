# mcpone python runtime (Option B)

This workspace builds a **custom Python runtime component** (Option B):

- Reuses prebuilt CPython/WASI base libraries from `crates/mcpone-python-runtime/libs/*.zst`
- Uses a custom Rust runtime bridge (`crates/mcpone-python-wasm-runtime`)
- Produces a WASI Preview2 component: `runtime.wasm`
- Uses networking imports:
  - `wasi:sockets/*` for TCP
  - `mcpone:host-sockets/tls@0.1.0` for TLS

## Crates

- `crates/mcpone-python-runtime` — component linker + WIT world
- `crates/mcpone-python-wasm-runtime` — guest runtime bridge (CPython FFI + shims)
- `crates/wit-dylib-ffi` — vendored wit-dylib ffi helpers
- `crates/python-host-probe` — Rust Preview2 host probe for runtime debugging

## Build prerequisites

- Rust stable + nightly with `rust-src`
- nightly target `wasm32-wasip1`
- WASI SDK (clang + wasi-sysroot)

Install WASI SDK (macOS arm64 helper):

```bash
# Run from the repository root:
./scripts/install-wasi-sdk.sh
export WASI_SDK_PATH=$(find "$PWD/.wasi-sdk" -maxdepth 1 -type d -name 'wasi-sdk-*' | head -n1)
```

## Build runtime.wasm

```bash
# Run from the repository root:
BUILD_MCPONE_PY_RUNTIME=1 cargo build -p mcpone-python-runtime --release
```

Expected output:

- `crates/mcpone-python-runtime/runtime.wasm`

## Run host probe

### Rust host probe

```bash
# Run from the repository root:
export PYTHON_STDLIB="$PWD/crates/mcpone-python-wasm-runtime/tests/python-stdlib"
cargo run -p python-host-probe --release
```

Optional env vars:

- `PY_RUNTIME_COMPONENT=/path/to/runtime.wasm`
- `PYTHON_STDLIB=/path/to/python-stdlib`
- first CLI arg = Python source to execute
