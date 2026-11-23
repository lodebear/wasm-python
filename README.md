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

Run these commands from the repository root. Install WASI SDK (macOS arm64 helper):

```bash
./scripts/install-wasi-sdk.sh
export WASI_SDK_PATH=$(find "$PWD/.wasi-sdk" -maxdepth 1 -type d -name 'wasi-sdk-*' | head -n1)
```

## Build runtime.wasm

```bash
BUILD_MCPONE_PY_RUNTIME=1 cargo build -p mcpone-python-runtime --release
```

The nested WASI build isolates host Rust/Cargo flags. To supply guest compiler
flags explicitly, use `MCPONE_PY_RUNTIME_ENCODED_RUSTFLAGS`. Multiple flags use
the same unit-separator encoding as Cargo's `CARGO_ENCODED_RUSTFLAGS`; the build
always adds position-independent code. For example, a release build can remap
machine-specific source paths without adding them to project configuration:

```sh
MCPONE_PY_RUNTIME_ENCODED_RUSTFLAGS="--remap-path-prefix=$HOME=/build" \
  BUILD_MCPONE_PY_RUNTIME=1 cargo build -p mcpone-python-runtime --release
```

Expected output:

- `crates/mcpone-python-runtime/runtime.wasm`

The command export supports `python -c CODE ARGS...`, `python FILE ARGS...`,
`python -m MODULE ARGS...`, and piped input through bare `python` or
`python - ARGS...`. Arguments are passed as Unicode data in `sys.argv`, including
empty arguments and arguments with spaces. Module execution uses `runpy` with
the located module filename in `sys.argv[0]`. Commands prepend their script or
working directory to a copy of the configured search path. The original
`sys.argv` and `sys.path` are restored after success or failure, preserving the
separate `execute(code)` export's argument and import context. Failed commands
retain captured stdout and stderr; the WASI command result distinguishes success
from failure rather than carrying a numeric process exit status.
The command's initial directory comes from the standard WASI environment
interface. Relative script paths and module lookup use that directory; Python's
working directory is scoped to the command and restored afterwards.

### Default text encoding

The embedded interpreter enables UTF-8 mode with `PyPreConfig.utf8_mode = 1`
and `Py_PreInitialize` before `Py_InitializeEx(0)`. Default `open()` and `Path`
text I/O therefore use UTF-8, including under `LANG=C LC_ALL=C`. An explicit
`encoding='ascii'` still selects ASCII. `PYTHONPATH` and `PYTHONHOME` continue
to select the mounted Python resources, and signal handlers remain disabled.

The small initialization FFI declarations match the bundled CPython 3.14
library; PyO3's selected limited ABI does not expose these configuration types.
No CPython source patch is needed.

After building, run the runtime integration tests with a matching Python stdlib:

```bash
MCPONE_PYTHON_STDLIB=/path/to/python-stdlib \
  cargo test --locked -p mcpone-python-wasm-runtime --test runtime_test test_default_text_encoding
```

`test_default_text_encoding` checks UTF-8 startup, Unicode reads and writes,
later C-locale selection, and explicit ASCII behavior in a fresh WASI instance.


## Run host probe

### Rust host probe

```bash
export PYTHON_STDLIB=/path/to/python-stdlib
cargo run -p python-host-probe --release
```

Optional env vars:

- `PY_RUNTIME_COMPONENT=/path/to/runtime.wasm`
- `PYTHON_STDLIB=/path/to/python-stdlib`
- first CLI arg = Python source to execute

## Generated artifacts

Build `runtime.wasm` locally with `BUILD_MCPONE_PY_RUNTIME=1` as shown above.
The runtime component and generated late-linking libraries are not tracked.
The base CPython/WASI libraries in `crates/mcpone-python-runtime/libs/` are
retained as build inputs.

The experimental threaded-runtime branch also builds its runtime and static
archive locally; see `RUNTIME-BUILD.md` and the scripts on that branch.
