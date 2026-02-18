# mcpone-python-wasm-runtime

Custom WASM guest runtime bridge for MCP One.

This crate implements the wit-dylib interpreter interface and bridges:

- WIT exports (`execute`, `snapshot-state`, `restore-state`, `clear-state`)
- host imports (callbacks, trace/output, custom sockets)
- CPython FFI execution (`libpython3.14.so`)

Built as:

- staticlib (`libmcpone_python_wasm_runtime.a`)

Then linked by `mcpone-python-runtime` into:

- `libmcpone_python_runtime.so`
- final component `runtime.wasm`
