# Stage C — threaded CPython core module + the mcpone guest bridge on PLAIN core imports

## Outcome: SUCCESS

The static THREADED CPython core module now statically links the
`mcpone-python-wasm-runtime` guest bridge (the socket/TLS/execute/stdout shim),
with its host networking/TLS bound to **PLAIN CORE IMPORTS** (`monocp.tcp_*`,
`monocp.dns_*`, `monocp.tls_*` + `wasi_snapshot_preview1.*` + `wasi.thread-spawn`)
rather than the component/wit interfaces the shipped preview2 build uses. The
resulting core module:

- (a) keeps THREADS intact — shared `env.memory` (or a self-defined shared
  memory), `export "wasi_thread_start"`, `import "wasi" "thread-spawn"`,
  `+atomics`/`+bulk-memory`, and is a COMMAND (`export "_start"`); and
- (b) now imports the 14 `monocp.*` networking/TLS functions as plain core
  imports; and
- (c) loads `import socket` and `import ssl` standalone (the host-backed shims),
  with the `_mcpone` builtins (`_mcpone_tcp_connect`, `_mcpone_tls_upgrade`, …)
  present.

This was the central feasibility risk in the plan ("can the bridge be cleanly
re-bound to plain imports?"). **It can.** The Stage-3 componentization wall
(canonical-ABI shared-memory mismatch, see `STAGE3-WALL.md`) is entirely
side-stepped because Path A never componentizes: the deliverable is a CORE
module driven by the preview1 host, exactly like ffmpeg.

## Why the bridge re-bound cleanly (the load-bearing insight)

The shipped preview2 build resolves networking by string at runtime and
dispatches through a function pointer that the **wit-dylib bindings** fill in at
component-instantiation time (`Wit::get_import(...)` -> `call_import_sync` ->
the bindings' import impl). TCP/DNS go via `wasi:sockets/*`, TLS via the
`mcpone:host-sockets/tls@0.1.0` component import. None of that machinery exists
in a plain core module (no component, no wit-dylib bindings).

But the interpreter staticlib bakes in NO wasm import for networking — the
Python-facing surface is just the `_mcpone` PyO3 builtins
(`_mcpone_tcp_connect/read/write/close`, `_mcpone_tls_upgrade/read/write/close`)
calling the crate's public `do_tcp_*` / `do_tls_*`, which call the private
`call_*` layer. So re-binding is a SURGICAL swap of just the `call_*` layer.

## What changed (guest crate `crates/mcpone-python-wasm-runtime`)

1. **`src/core_net.rs`** (new, `#[cfg(feature = "core-net")]`): declares the 14
   `monocp.*` functions as `#[link(wasm_import_module = "monocp")] extern "C"`
   core imports — matching the preview1 host ABI exactly (see backend
   `internal/vm/wasi/wasm/preview1/{net_imports,net_tcp,net_dns,net_tls}.go` and
   the 32-byte LE sockaddr in `runtime/sockaddr.go`). Reimplements
   `call_tcp_connect` (net_instance -> dns_resolve -> dns_resolve_next ->
   tcp_create -> tcp_start_connect -> tcp_finish_connect), `call_tcp_read/write/
   close`, and `call_tls_upgrade/read/write/close` (TLS re-dials by hostname,
   ignoring the tcp handle, exactly like the preview1 host).

2. **`src/lib.rs`**: the existing wit-dylib `call_tcp_*`/`call_tls_*` are gated
   `#[cfg(not(feature = "core-net"))]`; thin `#[cfg(feature = "core-net")]`
   delegators forward to `core_net`. The public `do_tcp_*`/`do_tls_*` surface and
   all of `python.rs` (the `_mcpone` builtins, the socket/ssl shims) are
   UNCHANGED, so the entire Python-facing surface is reused.

3. **`src/threads_command.rs`** (new, `#[cfg(feature = "threads-command")]`):
   exports `mcpone_threads_main()`, a C-callable entry that runs the SAME
   `initialize_python()` the reactor does (registers `_mcpone`, injects the
   socket/ssl/subprocess shims), then executes a script from `MCPONE_CODE`
   (default: an `import socket`/`import ssl` smoke). This is needed because a
   wasi-threads CPython build's only entry is `_start` -> vanilla CPython
   `main()`, which never touches the bridge (so the shims and the `monocp.*`
   imports would be dead-stripped).

4. **`Cargo.toml`**: adds the `core-net` and `threads-command` features (default
   off, so the shipped preview2 component build is untouched).

## How to reproduce

```
# Stage 2 prerequisite (already built into threads-build/):
scripts/build-cpython-threads.sh all

# Stage C: build the bridge staticlib + link the threaded core module(s):
scripts/build-cpython-threads-bridge.sh
#   -> threads-stage3/bridge/python-threads-bridge.wasm          (--import-memory)
#   -> threads-stage3/bridge/python-threads-bridge-selfmem.wasm  (self memory)

# Standalone import smoke (prints THREAD-IMPORT-OK socket ssl):
WASMTIME=/path/to/wasmtime-41 scripts/run-cpython-threads-bridge.sh
```

## Verified shape (wasm-tools 1.244.0) — see `BRIDGE-WASM-SHAPE-PROOF.txt`

`python-threads-bridge.wasm` (production `--import-memory` shape):

```
(import "env" "memory" (memory (;0;) 50 4096 shared))   ; shared mem (threads)
(import "wasi" "thread-spawn" (func ...))                ; wasi-threads spawn
(export "memory" (memory 0))
(export "wasi_thread_start" (func $wasi_thread_start))   ; thread entry export
(export "_start" (func $_start))                         ; COMMAND
target_features: +atomics +bulk-memory
14 monocp.* plain core imports: net_instance, tcp_create, tcp_start_connect,
  tcp_finish_connect, tcp_read, tcp_write, tcp_close, dns_resolve,
  dns_resolve_next, dns_close, tls_upgrade, tls_read, tls_write, tls_close
wasm-tools validate --features threads => CORE MODULE VALID (threads)
head -c8 => 0061736d 01000000  (core module, NOT a component)
```

## Standalone load smoke (real output)

```
$ WASMTIME=.../wasmtime scripts/run-cpython-threads-bridge.sh
THREAD-IMPORT-OK socket ssl

$ ... 'import socket, ssl, _mcpone, threading; ...'
ssl is shim: True                 # ssl.__doc__ contains "MCP One ssl shim"
tls_upgrade present: True         # _mcpone._mcpone_tls_upgrade
threading active: 1
ALL-OK
```

## One harness caveat (NOT a module defect)

The wasmtime **CLI** cannot, in a single invocation, BOTH host wasi-threads
(`-S threads=y`, which is what supplies the shared `env.memory` + `thread-spawn`)
AND `--preload` a custom `monocp` core module: with `-S threads=y` the CLI's
wasi-threads instantiation path does not consult `--preload`, and without it the
CLI does not provide the shared `env.memory`. Reproduced minimally:

```
# with -S threads=y:        Error: unknown import: `monocp::tcp_close` ...
# without -S threads=y:     Error: unknown import: `env::memory` ...
```

So the standalone IMPORT smoke uses the **self-memory** variant
(`python-threads-bridge-selfmem.wasm`, which DEFINES+EXPORTS its own shared
memory) plus stub `monocp` + `wasi.thread-spawn` preloads. The smoke only
IMPORTS the shims (never connects), so the stubs are never called and the threads
machinery (shared memory, `wasi_thread_start`, atomics) is all still present and
validated. The production `--import-memory` module is built and validated
identically; running it with REAL host networking + REAL wasi-threads is the
job of the preview1 Go host (Stage D), which provides both `monocp.*` and the
wasi-threads host together — no CLI compromise there.

## Next (Stage D)

Run `python-threads-bridge.wasm` on the preview1 Go runner (the host that
registers `monocp.tcp_*/dns_*/tls_*` from Stage B and a wasi-threads host) with
a `threading.Thread` doing a urllib HTTPS GET, asserting THREAD-HTTPS-OK. The
guest binding is done; what remains is wiring the threaded core module into the
preview1 runner alongside the wasi-threads spawn host.
