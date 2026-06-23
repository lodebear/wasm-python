/* Command entry for the THREADED preview1 CPython core module.
 *
 * Compiled by scripts/build-cpython-threads-bridge.sh.
 *
 * We do NOT link CPython's own Programs/python.o (its main() runs the vanilla
 * REPL/script path and never wires in the mcpone bridge). Instead this main()
 * calls mcpone_threads_main() from the Rust guest bridge
 * (crates/mcpone-python-wasm-runtime, built with --features threads-command),
 * which registers the _mcpone builtins + socket/ssl shims via
 * initialize_python() and then runs a script.
 *
 * Keeping a real C main() (rather than a Rust #[no_mangle] main) lets wasi-libc
 * emit the standard `_start` command shape with __wasi_args/environ wiring. */

extern int mcpone_threads_main(void);

int main(void) {
    return mcpone_threads_main();
}
