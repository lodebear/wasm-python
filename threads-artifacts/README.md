# Stage 2 — STATIC THREADED CPython for wasm32-wasip1-threads (DELIVERED)

This directory holds the **static, threaded** CPython 3.14 wasm artifacts produced
in Stage 2, plus the wasm-tools shape proof. It is the retarget of the Stage 1
single-threaded baseline (which was a *dynamic/PIC* re-link of prebuilt `.so`s).

## What was built

A **static** CPython core module (`python.wasm`) and its **static** archive
(`libpython3.14.a`), compiled `-target wasm32-wasi-threads -pthread` against the
**threaded static wasi-libc sysroot** that ships in wasi-sdk-27 at
`share/wasi-sysroot/lib/wasm32-wasip1-threads` (`libc.a`, `libc++.a`, etc.).

- `python-threads.wasm.zst`     — zstd of `cross-build/wasm32-wasi/python.wasm` (26.9 MiB raw)
- `libpython3.14-threads.a.zst` — zstd of the static threaded `libpython3.14.a` (37.8 MiB raw)
- `WASM-SHAPE-PROOF.txt`        — wasm-tools dump/validate output (the shape evidence)

## How it was built (reproducible)

```
scripts/build-cpython-threads.sh native          # native build python (cross prereq)
scripts/build-cpython-threads.sh configure-host   # ./configure --enable-wasm-pthreads, STATIC (no --enable-shared)
scripts/build-cpython-threads.sh make-host         # -> cross-build/wasm32-wasi/python.wasm + libpython3.14.a
```

Source: `dicej/cpython` branch `v3.14.0-wasi-sdk-27` (the same lineage as the
shipped single-threaded baseline lib). Toolchain: wasi-sdk-27 (clang 20.1.8).

The load-bearing knob is CPython's own `--enable-wasm-pthreads`, which makes
`configure` append (verified in the generated Makefile):

```
CONFIGURE_CFLAGS=        -target wasm32-wasi-threads -pthread
CONFIGURE_LDFLAGS_NODIST= -target wasm32-wasi-threads -pthread \
    -Wl,--import-memory -Wl,--export-memory -Wl,--max-memory=268435456
LDLIBRARY = libpython3.14.a      PY_ENABLE_SHARED = 0   (STATIC)
```

NOTE on `--max-memory`: CPython's `configure` hardcodes a 10 MiB cap
(`-Wl,--max-memory=10485760`) under `--enable-wasm-pthreads`. A host cannot grow
a shared memory past the module's declared maximum, so 10 MiB OOMs heavy stdlib
(`concurrent.futures.ThreadPoolExecutor`, `urllib`/`ssl`, `pip`). The build
script now appends `-Wl,--max-memory=268435456` (256 MiB = 4096 pages, matching
ffmpeg's cap) via `LDFLAGS_NODIST` in `configure-host`; wasm-ld honours the LAST
`--max-memory`, so this overrides the default without patching CPython. Override
with `MAX_MEMORY=<bytes>`.

NOTE on the `--host` triple: `config.sub` rejects `wasm32-wasip1-threads`
("OS 'threads' not recognized"). The accepted host triple is `wasm32-wasi`;
`--enable-wasm-pthreads` is what injects the `wasm32-wasi-threads` target and
the shared-memory link flags. (This is how CPython upstream builds WASI threads.)

## Confirmed shape (wasm-tools 1.244.0) — see WASM-SHAPE-PROOF.txt

- `import "env" "memory" ... shared: true, maximum 4096`  — SHARED linear memory, imported (wasi-threads), 256 MiB cap
- `export "memory"`                         — memory EXPORTED (a worker can import it)
- `export "wasi_thread_start"`              — thread entry export
- `import "wasi" "thread-spawn"`            — wasi thread-spawn import (the ffmpeg shape)
- `target_features` contains `+atomics` (+bulk-memory ...)
- `wasm-tools validate --features threads` => VALID

## Confirmed execution (wasmtime 41.0.4, threads enabled)

```
scripts/run-cpython-threads.sh 'print(1+1)'   # => 2
```

Real OS threads verified:
- `threading.Thread` start/join => `OK [1]`
- `concurrent.futures.ThreadPoolExecutor(max_workers=4)` => `POOL OK 30`
  (`sum(ex.map(lambda x: x*x, range(5)))` == 30; raised `MemoryError` under the old 10 MiB cap)

## Installing the threaded build as the backend `python` WASI command

Stage C wires the threaded `libpython3.14.a` into the mcpone guest bridge as a
runnable **core module** (`threads-stage3/`, see `threads-stage3/STAGE-C-NOTES.md`).
To produce the backend-consumable assets (bridge `.wasm` + merged stdlib) in one
stable directory, run:

```bash
scripts/prepare-threaded-python-assets.sh                 # full rebuild
USE_COMMITTED_ZST=1 scripts/prepare-threaded-python-assets.sh  # reuse committed bridge .zst
```

Outputs land in `threads-stage3/assets/`:
- `python-threads-bridge.wasm` — the threaded bridge core module
- `stdlib/`                     — `cpython/Lib/*` (minus `__pycache__`) +
  the two cross-build `_sysconfig*` files + an empty `site-packages/`

The backend selects this flavor with the committed switch (no hand-editing the
gitignored `wasm-tools.sources.yaml`):

```bash
make -C backend wasm-tools PYTHON_FLAVOR=threaded
```

whose `MONOCP_THREADED_PYTHON_WASM`/`MONOCP_THREADED_PYTHON_STDLIB` defaults
point at `threads-stage3/assets/`. The committed
`threads-stage3/python-threads-bridge.wasm.zst` is kept in sync with the bridge
that the current guest crate produces, so `USE_COMMITTED_ZST=1` reproduces the
same `.wasm` without a rust/clang relink.

## Not (yet) done — the follow-on for the mcpone reactor

This delivers a static threaded CPython **command** core module (the Stage-2 gate).
Wiring the threaded static `libpython3.14.a` into the mcpone **reactor** component
(linking the pyo3 guest bridge, exporting `execute`, and threading the
`wasi:thread-spawn` import out through the component model) is the next stage:
the existing `src/linker.rs` path uses `wit_component::Linker` for *dynamic/PIC*
shared-everything linking, which is the dead end for threads — a threaded reactor
must instead be statically linked, then componentized with a thread-aware adapter.
