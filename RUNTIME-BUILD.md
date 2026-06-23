# CPython wasm runtime: baseline build + from-source recipe

This documents (1) the EXISTING re-link flow that turns the prebuilt CPython/WASI
`.so` libs into `runtime.wasm`, and (2) the from-source CPython compile recipe we
would reproduce in Stage 2 to retarget the interpreter (e.g. for real threads).

## 1. Current build/link flow (Stage 1 baseline — REPRODUCED)

`runtime.wasm` is a WASI Preview2 component produced by *re-linking prebuilt
shared libraries*; there is no in-repo CPython compile. The full flow lives in
`crates/mcpone-python-runtime/build.rs` and only runs when
`BUILD_MCPONE_PY_RUNTIME=1` is set.

Steps (build.rs):
1. `build_wasm_runtime()`:
   - `rustup run nightly cargo build -Z build-std=panic_abort,std
     --target wasm32-wasip1 --release` of the `mcpone-python-wasm-runtime`
     crate (a `staticlib`, built PIC via `RUSTFLAGS=-C relocation-model=pic`,
     into a nested `OUT_DIR/wasm-runtime-target` to avoid cargo-lock contention)
     -> `libmcpone_python_wasm_runtime.a`.
   - wasi-sdk `clang --target=wasm32-wasip1 -fPIC -c clock_stubs.c` ->
     `clock_stubs.o` (provides `_CLOCK_PROCESS_CPUTIME_ID` / `_CLOCK_THREAD_CPUTIME_ID`).
   - wasi-sdk `clang --target=wasm32-wasip1 -shared -Wl,--allow-undefined
     -Wl,--whole-archive <staticlib> -Wl,--no-whole-archive clock_stubs.o`
     -> `libmcpone_python_runtime.so` (the guest bridge .so).
2. `build_component()`:
   - decompress `libs/*.zst` -> `libs/decompressed/` (libc, libc++, libc++abi,
     libpython3.14, the four wasi-emulated-* shims, and the
     `wasi_snapshot_preview1.reactor.wasm` adapter).
   - parse `wit/` (world `sandbox`), generate wit-dylib bindings pointing at
     `libmcpone_python_runtime.so`.
   - `wit_component::Linker` links, in dependency order: the 4 wasi-emulated
     shims, libc, libc++abi, libc++, libpython3.14, our runtime, the bindings,
     and the `wasi_snapshot_preview1` reactor adapter -> encodes the component
     -> writes `crates/mcpone-python-runtime/runtime.wasm`.

### Reproduce

    ./scripts/build-runtime.sh            # produces crates/mcpone-python-runtime/runtime.wasm
    # run a trivial program:
    PYTHON_STDLIB=/path/to/cpython-3.14-wasi-stdlib \
      ./scripts/build-runtime.sh --run   # prints "2"

Prereqs: rustup nightly + `rust-src`, target `wasm32-wasip1`, wasm-tools, and
wasi-sdk-27 (clang 20.1.8-wasi-sdk). `WASI_SDK_PATH` must point at the SDK
(the script auto-detects the workspace/main-checkout `.wasi-sdk`).

The host probe (`crates/python-host-probe`) instantiates the component on
Wasmtime 41 and calls the `execute` export; it needs a CPython 3.14 wasi stdlib
tree (a dir containing `encodings/`) preopened as `/python-stdlib`.

## 2. Provenance of the prebuilt libpython3.14.so

`libs/libpython3.14.so.zst` decompresses to a 30 MB `.so` whose embedded
metadata pins its origin exactly:

- producers / clang: `Clang 20.1.8-wasi-sdk
  (llvm 87f0227cb60147a26a1eeb4fb06e3b505e9c7261)` — byte-identical to the local
  `.wasi-sdk/wasi-sdk-27.0-arm64-macos` (VERSION: 27.0, llvm-version 20.1.8,
  llvm 87f0227cb601).
- build path baked in: `/home/ben/repos/rust/componentize-py/cpython/builddir/wasi`
  and `.../install` — i.e. built via **componentize-py**'s CPython submodule
  ("ben" = dicej).
- symbol suffix `.cpython-314-wasm32-wasi.so` — CPython 3.14, wasm32-wasi.
- It is a DYNAMIC (PIC) build: has a `dylink.0` section and
  `(import "env" "memory" (memory 49))`.

### THREADLESS confirmation (key for Stage 2 retarget)

The shipped libpython memory is **non-shared** and `target_features` is:
`bulk-memory, bulk-memory-opt, extended-const, multivalue, mutable-globals,
nontrapping-fptoint, reference-types, sign-ext` — i.e. **NO `atomics`, NO shared
memory**. So this is the single-threaded build, exactly the baseline to retarget.

## 3. From-source CPython recipe (Stage 2 — NOT yet executed)

Source of truth: componentize-py's `build.rs`
(`bytecodealliance/componentize-py`, default branch `main`).

- CPython source: a `dicej/cpython` branch built for wasi-sdk. Our shipped libs
  used branch **`v3.14.0-wasi-sdk-27`** (matches local clang 20.1.8 = wasi-sdk-27).
  Branch verified to exist at sha `342deabaab74d29fad6d6aecb9a3d8d390ed26c3`.
  NOTE: componentize-py `main` HEAD has since moved on to
  `v3.14.0-wasi-sdk-30` (CPYTHON_TARBALL_URL) and `DEFAULT_SDK_VERSION = "33"`.
  To faithfully reproduce the *shipped* lib, pin CPython to
  `v3.14.0-wasi-sdk-27` and use the local wasi-sdk-27. (A from-scratch Stage 2
  may instead adopt the newer wasi-sdk-30/33 lineage.)

- Build steps (from componentize-py `maybe_make_cpython`):
  1. **Native build python first** (needed as `--with-build-python` for the
     cross build): in `cpython/builddir/build`, run `../../configure
     --prefix=.../install` then `make`.
  2. Build deps: zlib + sqlite3 static libs into `builddir/wasi/deps`.
  3. `config.guess` -> `--build` triple.
  4. Cross-configure (the load-bearing flags):

         CONFIG_SITE=../../Tools/wasm/wasi/config.site-wasm32-wasi
         CFLAGS="--target=wasm32-wasip2 -fPIC -I<wasi>/deps/include"
         LDFLAGS="--target=wasm32-wasip2 -L<wasi>/deps/lib"
         WASI_SDK_PATH=<wasi-sdk>
         ../../Tools/wasm/wasi-env ../../configure -C \
           --host=wasm32-unknown-wasip2 \
           --build=<config.guess> \
           --with-build-python=<wasi>/../build/python \
           --prefix=<wasi>/install \
           --disable-test-modules \
           --enable-ipv6

     (componentize-py `main` targets `wasm32-wasip2`. The shipped lib's symbol
     suffix is `wasm32-wasi`; the wasi-sdk-27 lineage configured `wasm32-wasi`.
     This triple is the main thing to re-derive when pinning to wasi-sdk-27.)
  5. `Modules/Setup.local` force-enables `_sqlite3`.
  6. `make build_all install`.
  7. Final shared lib link (wasi-sdk clang):

         clang --target=wasm32-wasip2 -shared -o libpython3.14.so \
           -Wl,--whole-archive libpython3.14.a -Wl,--no-whole-archive \
           Modules/_hacl/libHacl_*.a Modules/_decimal/libmpdec/libmpdec.a \
           Modules/expat/libexpat.a deps/lib/libz.a deps/lib/libsqlite3.a \
           -lwasi-emulated-signal -lwasi-emulated-getpid \
           -lwasi-emulated-process-clocks -ldl

- Estimated cost: a native CPython `configure && make`, then a wasm cross
  `configure && make build_all install` (+ zlib/sqlite static builds). On the
  order of tens of minutes to ~an hour of compile, plus network to fetch the
  CPython tarball (~25 MB) — github reachable from this environment.

### Stage 2 retarget-for-threads pointer

To get a threaded libpython, the wasm cross build above must be reconfigured for
the threads target (atomics + shared memory). With wasi-sdk that means the
`wasm32-wasip1-threads` toolchain/sysroot path (and matching `-matomics
-mbulk-memory -pthread` CFLAGS / shared memory), which is the actual Stage 2
investigation — not covered here.
