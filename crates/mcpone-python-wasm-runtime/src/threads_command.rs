//! Standalone command entry for the THREADED preview1 CPython core module.
//!
//! The shipped preview2 runtime is a *reactor* component: the host drives it
//! through the wit-dylib `execute()` export, and `initialize_python()` (which
//! registers the `_mcpone` builtins and injects the socket/ssl shims) runs as
//! part of that flow. A wasi-threads CPython build, by contrast, is a plain
//! *command* core module whose only entry is `_start` -> CPython `main()`.
//! Vanilla `main()` never touches the bridge, so the `_mcpone` builtins and the
//! socket/ssl shims would be dead-stripped and `import ssl` would load the real
//! (unavailable) `_ssl` C extension.
//!
//! This module provides `mcpone_threads_main()`, a C-callable entry that a tiny
//! `main.c` invokes instead of CPython's `main()`. It runs the SAME
//! `initialize_python()` setup as the reactor, then executes a script. That
//! pulls the entire bridge (and therefore the `monocp.*` plain imports from
//! `core_net`) into the link and makes `import socket` / `import ssl` resolve
//! to the host-backed shims.

use std::ffi::c_int;
use std::io::Write;

use crate::python;

/// Report-output callback: stream Python stdout/stderr to the real WASI fds so a
/// standalone run prints in real time (mirrors the host's report-output sink).
fn stream_to_real_fd(stream: u32, data: &str) {
    if stream == 1 {
        let mut err = std::io::stderr();
        let _ = err.write_all(data.as_bytes());
        let _ = err.flush();
    } else {
        let mut out = std::io::stdout();
        let _ = out.write_all(data.as_bytes());
        let _ = out.flush();
    }
}

/// C-callable entry. Initializes the interpreter exactly like the reactor
/// (registering `_mcpone` + the socket/ssl/subprocess shims), then runs the
/// program from the `MCPONE_CODE` env var (or a default import smoke).
///
/// Returns 0 on success, non-zero if Python execution reported an error.
///
/// # Safety
/// Must be called once, from the module's single `main()` thread, before any
/// other interpreter use.
#[unsafe(no_mangle)]
pub extern "C" fn mcpone_threads_main() -> c_int {
    // Stream Python output to the real fds.
    python::set_report_output_callback(Some(stream_to_real_fd));

    // Same setup the reactor performs: registers _mcpone, initializes CPython,
    // injects the _mcpone_async / socket / ssl / subprocess shims.
    python::initialize_python();

    let code = std::env::var("MCPONE_CODE").unwrap_or_else(|_| {
        // Default smoke: prove the host-backed socket/ssl shims at least LOAD in
        // the threaded core module.
        "import socket\nimport ssl\nprint('THREAD-IMPORT-OK', socket.__name__, ssl.__name__)\n"
            .to_string()
    });

    match python::execute_python(&code) {
        python::ExecuteResult::Complete(_output) => {
            // Output was already streamed to the real fds via the report-output
            // callback (the streaming writer flushes on each write), so we do not
            // re-print _output.stdout/_output.stderr here.
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            0
        }
        python::ExecuteResult::Error(err) => {
            eprintln!("mcpone_threads_main: python error: {err}");
            1
        }
        python::ExecuteResult::Pending(_) => {
            // A standalone smoke script should not be async; treat as success but
            // signal that the async runtime would need driving.
            eprintln!("mcpone_threads_main: unexpected pending result");
            2
        }
    }
}
