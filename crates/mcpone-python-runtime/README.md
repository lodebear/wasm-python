# mcpone-python-runtime

Python WASM runtime component builder for MCP One.

This crate links:

- `libmcpone_python_runtime.so` (custom guest runtime bridge)
- `libpython3.14.so` and supporting WASI libraries
- WIT-generated bindings (`libmcpone_python_bindings.so`)

to produce:

- `runtime.wasm` (WASI Preview2 component)

## Build

```bash
# Run from the repository root:
BUILD_MCPONE_PY_RUNTIME=1 cargo build -p mcpone-python-runtime --release
```

## WIT world

Source of truth:

- `wit/runtime.wit`

Imports include:

- `mcpone:host-sockets/tcp@0.1.0`
- `mcpone:host-sockets/tls@0.1.0`
- callback/trace/output imports
