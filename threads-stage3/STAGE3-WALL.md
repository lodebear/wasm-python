# Stage 3 — Componentize the threaded CPython core into a preview2 component

Goal: take the Stage-2 STATIC THREADED CPython core module and componentize it
(wasm-tools + reactor adapter) so the inner core module EXPORTS its shared memory
(P4 shared-everything-linking: provider exports shared memory, worker imports it);
verify the component validates, the shared memory is reachable, and a trivial guest
runs through the `execute()` path.

OUTCOME: **BLOCKED at a hard component-model wall.** The P4 *memory topology*
(a core module that EXPORTS a shared memory, surviving into a component) is
**achieved and verified**. But componentizing the THREADED CPython *reactor*
(with the `execute()` canonical-ABI export) is **architecturally impossible** with
today's component model / wasm-tools 1.244.0 / wasmparser 0.244, because the
canonical ABI memory must be NON-shared and a wasi-threads module has exactly one
memory which MUST be shared.

Reproduce all of the below with: `scripts/componentize-cpython-threads.sh`
Toolchain: wasm-tools 1.244.0 (wasmparser 0.244 validator), wasi-sdk-27 (clang 20.1.8).

## The Stage-2 threaded module shape (input)

`threads-build/cpython/cross-build/wasm32-wasi/python.wasm` (the Stage-2 output):

    import "env" "memory" (memory 48 160 shared)   ; wasi-threads --import-memory
    import "wasi" "thread-spawn" (func ...)         ; the ffmpeg/wasi-threads spawn import
    export "memory" (memory 0)                      ; re-exported
    export "_start"                                 ; it is a COMMAND, not a reactor
    export "wasi_thread_start"

Two structural facts matter: (a) it IMPORTS its (shared) memory from `env`
(forced by `--enable-wasm-pthreads` -> `-Wl,--import-memory`), and (b) it is a
*command* with no `execute` export and no guest bridge linked.

## The wall chain (each step surfaces the next blocker; verbatim errors)

### WALL 1 — `wasi:thread-spawn` is not a WIT interface
`wasm-tools component new python.wasm --adapt wasi_snapshot_preview1=<reactor>`:

    2: failed to resolve import `wasi::thread-spawn`
    3: module requires an import interface named `wasi`

The component model can only bind imports that name a WIT interface
(`ns:pkg/iface`). `wasi.thread-spawn` is a bare core import that Wasmtime's
wasi-threads host satisfies at *core* instantiation — it has no component-model
representation. Worked around by supplying a trivial `--adapt wasi=<stub>`
adapter exporting `thread-spawn: (param i32)->(result i32)`.

### WALL 2 — a main module may not IMPORT its memory
With thread-spawn stubbed:

    0: failed to satisfy import `env::memory`
    1: main module cannot import memory

`wasm-tools component new`'s single-module reactor flow assumes the main module
DEFINES and EXPORTS its own memory (see wit-component validation.rs:539 special
case + encoding.rs:1717 `main module cannot import memory`). The wasi-threads
`--import-memory` convention violates this.

FIX (verified): RELINK the Stage-2 threaded `libpython3.14.a` + `python.o`
WITHOUT `-Wl,--import-memory` (keep `-Wl,--shared-memory -Wl,--export-memory`).
The relinked module then DEFINES + EXPORTS its own shared memory — exactly the
**P4 "provider exports shared memory"** shape:

    (memory (;0;) 48 160 shared)     ; defined here, no env.memory import
    (export "memory" (memory 0))
    core module VALID (threads)

Artifact committed: `threads-stage3/python-threads-selfmem.wasm.zst`
(26.7 MiB raw; relinked from the real threaded archive in ~0.5s).

### WALL 3 — the stock reactor adapter imports a NON-shared `env.memory`
Componentizing the self-mem module with the shipped reactor adapter:

    1: type mismatch for export `memory` of module instantiation argument `env`
       mismatch in the shared flag for memories

`wasi_snapshot_preview1.reactor.wasm` declares `(import "env" "memory" (memory 0))`
— a non-shared memory. The threaded provider exports a shared one. Worked around
by patching the adapter's import to `(memory 48 160 shared)`.

### WALL 4 — THE HARD WALL: canonical-ABI memory must be NON-shared
With the shared-memory-patched adapter:

    1: canonical ABI memory is not a 32-bit linear memory
       mismatch in the shared flag for memories

Source: wasmparser-0.244 `validator/component.rs::cabi_memory_at` requires the
lifting/lowering memory to match `MemoryType { shared: false, memory64: false, .. }`.
The doc-comment is explicit:

    /// At this time this requires that the memory is a plain 32-bit linear
    /// memory. Notably this disallows shared memory and 64-bit linear memories.

A wasi-threads CPython core module has exactly ONE linear memory, and it MUST be
shared (every atomic op operates on it). The component model's canonical ABI
needs a NON-shared memory to lift/lower `execute(code: string)` arguments. There
is no second memory to offer it. => the threaded reactor cannot be componentized.

## CONTROL (proves the spec DOES allow exported shared memory in a component)

A trivial module that DEFINES+EXPORTS a shared memory and a plain function
componentizes and validates cleanly:

    (component
      (core module ... (memory (;0;) 1 2 shared) (export "memory" (memory 0)) ...)
      ...)
    shared-memory-exporting reactor COMPONENT VALID

So shared memory per se is representable in a component; the blocker is strictly
the *canonical ABI* memory. (When the module exports NO canonical-ABI function
that lifts/lowers via memory, the shared-memory check at WALL 4 is never reached.)

## What this means / the genuine path forward

- **P4 memory topology delivered at the core level:** the threaded CPython core
  can be made to OWN+EXPORT its shared memory (no `env.memory` import). A worker
  thread spawned by `wasi:thread-spawn` imports that same shared memory at the
  core layer — this is exactly the provider/worker topology. Verified valid.

- **The outer componentization is blocked by the canonical ABI**, not by shared
  memory itself, and not by anything in the Stage-2 build. To export `execute()`
  from a threaded component you need EITHER:
    1. component-model / canonical-ABI support for a shared cABI memory (a spec
       change to wasmparser/wasmtime — `cabi_memory_at` currently forbids it), OR
    2. a TWO-MEMORY core module: a non-shared memory dedicated to the canonical
       ABI (lift/lower) plus a separate shared memory for threads. wasm-ld /
       CPython produce a single-memory module today, so this needs a custom link
       topology and a thread runtime that targets the second (shared) memory — a
       substantial change, likely requiring multi-memory + a bespoke adapter, OR
    3. run threaded CPython as a plain wasi-threads CORE module under Wasmtime's
       wasi-threads host (the ffmpeg/preview1 path) WITHOUT the component model —
       i.e. accept that real-threads Python lives outside the preview2 component
       boundary for now.

- The existing `wit_component::Linker` (dynamic/PIC shared-everything path used by
  `src/linker.rs`) is doubly unusable for this: it requires `dylink.0` metadata
  (the static threaded module has none) and its synthesized memory provider is
  hardcoded `shared: false` (wit-component linking.rs:535) — it cannot host a
  module that needs atomics on shared memory.

## Files (this stage)

- `scripts/componentize-cpython-threads.sh` — full reproducible wall chain.
- `threads-stage3/python-threads-selfmem.wasm.zst` — relinked threaded core that
  OWNS+EXPORTS its shared memory (P4 provider shape; core-valid with `--features
  threads`).
- `threads-stage3/wasi_thread_stub.wat` — stub satisfying `wasi.thread-spawn`.
- `threads-stage3/stage3-run.log` — captured end-to-end run of the script.
