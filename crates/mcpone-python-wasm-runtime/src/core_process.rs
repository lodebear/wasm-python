//! Plain-core-import subprocess execution for the THREADED (preview1) CPython
//! core module.
//!
//! In the shipped preview2 build the guest's subprocess shim reaches the host
//! through the wit-dylib component import `monocp:host/monocp@0.2.0` (the
//! `spawn-with-context` / `wait-child` / `close-child` family in `lib.rs`,
//! dispatched by `Wit::get_import` -> a function pointer the wit-dylib bindings
//! fill in at component-instantiation time). That machinery only exists inside
//! a COMPONENT instantiated by those bindings, so on the threaded *command*
//! core module `CURRENT_WIT` is `None` and `subprocess.run(...)` fails with
//! "spawn-with-context called outside of execute context".
//!
//! A wasi-threads CPython build is a plain CORE MODULE (no component, no
//! wit-dylib bindings), exactly like networking under the `core-net` feature.
//! So here we bind subprocess to a PLAIN CORE IMPORT in module `monocp`:
//! `monocp.exec_with_context_v2`, which the preview1 host already registers
//! (see backend .../preview1/host_core.go + exec.go `monocpExecWithContextV2`).
//!
//! Unlike the component spawn/wait split, the preview1 `exec_with_context_v2`
//! is a SINGLE synchronous, blocking call that returns the child's exit code
//! directly. That is a perfect match for the bridge's blocking model (the
//! preview1 host runs the child on a goroutine and the call returns when it
//! exits). The temp-file capture machinery in `do_subprocess_run`
//! (`lib.rs`) is build-agnostic and reused verbatim; only the host-reaching
//! triple is swapped for the single v2 call here.
//!
//! ## ABI (matches backend preview1/exec.go `parseExecContextV2`)
//!
//! The context struct is six little-endian u32s (24 bytes):
//!   [0]  argv_ptr   pointer to a NUL-separated argv buffer
//!   [1]  argv_len   byte length of that buffer
//!   [2]  cwd_ptr    pointer to the cwd string (utf-8, no NUL)
//!   [3]  cwd_len    byte length of cwd
//!   [4]  fds_ptr    pointer to an array of fd-spec records
//!   [5]  fds_len    number of fd-spec records
//!
//! Each fd-spec record is four little-endian u32s (16 bytes):
//!   [0]  child_fd   the fd number inside the child (0/1/2/...)
//!   [1]  kind       0=inherit, 1=pipe, 2=wasi-fd, 3=close
//!   [2]  value      pipe handle (kind=1) or source WASI fd (kind=2)
//!   [3]  flags      must be 0
//!
//! `monocp.exec_with_context_v2(ctx_ptr, ctx_len) -> i32` returns the child
//! exit code (or -1 on a host-side dispatch error).

use std::os::fd::AsRawFd;

use crate::{ProcessFdBinding, ProcessFdTarget};

#[link(wasm_import_module = "monocp")]
unsafe extern "C" {
    // exec_with_context_v2(ctx_ptr, ctx_len) -> exit_code (-1 on host error)
    #[link_name = "exec_with_context_v2"]
    fn monocp_exec_with_context_v2(ctx_ptr: u32, ctx_len: u32) -> i32;
}

// fd-spec `kind` discriminants (must match preview1/exec.go execV2Fd* consts).
const EXEC_V2_FD_INHERIT: u32 = 0;
const EXEC_V2_FD_WASI_FD: u32 = 2;
const EXEC_V2_FD_CLOSE: u32 = 3;

/// A resolved fd-spec to hand to the host, plus any open file backing it.
///
/// The `File` is kept alive in `_open_files` for the duration of the host call
/// so its raw WASI fd stays valid; the host clones the underlying descriptor
/// for the child (see preview1/exec.go `injectedFDsForExecV2` -> the
/// `wasiFDFile` branch clones the file).
struct FdSpec {
    child_fd: u32,
    kind: u32,
    value: u32,
}

/// Spawn a child via the plain `monocp.exec_with_context_v2` core import and
/// block until it exits, returning the child's exit code.
///
/// `bindings` is the SAME `[fd0, fd1, fd2]` vector `do_subprocess_run` already
/// builds: stdin/stdout/stderr each map to one of inherit / close /
/// file-read / file-write / dup-stdout. We translate each into the host's v2
/// fd-spec, opening the temp capture/input files so the host can clone real
/// WASI descriptors into the child.
///
/// `timeout_secs` is not separately enforced here: the v2 import is a single
/// blocking call, so there is no try-wait/kill loop to honour a deadline. The
/// subprocess shim only passes a timeout when the caller sets one; we surface a
/// clear error rather than silently ignoring it.
pub fn spawn_wait_close(
    argv: &[String],
    cwd: &str,
    bindings: &[ProcessFdBinding],
    timeout_secs: Option<f64>,
) -> Result<i32, String> {
    if argv.is_empty() {
        return Err("subprocess argv must not be empty".to_string());
    }
    if timeout_secs.is_some() {
        return Err(
            "subprocess timeout is not supported on the threaded (preview1) build".to_string(),
        );
    }

    // Keep opened files alive for the duration of the host call so their raw
    // fds remain valid while the host clones them for the child.
    let mut open_files: Vec<std::fs::File> = Vec::new();

    // Resolve each binding into a host fd-spec. `child_fd` is the binding's fd.
    // We need stdout's resolved WASI fd first so a `Dup(1)` stderr (stderr ->
    // stdout, i.e. subprocess.STDOUT) can point at the very same descriptor.
    let mut specs: Vec<FdSpec> = Vec::with_capacity(bindings.len());
    let mut stdout_wasi_fd: Option<u32> = None;

    for binding in bindings {
        let spec = match &binding.target {
            ProcessFdTarget::Inherit => FdSpec {
                child_fd: binding.fd,
                kind: EXEC_V2_FD_INHERIT,
                value: 0,
            },
            ProcessFdTarget::Close => FdSpec {
                child_fd: binding.fd,
                kind: EXEC_V2_FD_CLOSE,
                value: 0,
            },
            ProcessFdTarget::FileRead(path) => {
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .open(path)
                    .map_err(|e| format!("failed to open subprocess input {path}: {e}"))?;
                let raw = file.as_raw_fd() as u32;
                open_files.push(file);
                FdSpec {
                    child_fd: binding.fd,
                    kind: EXEC_V2_FD_WASI_FD,
                    value: raw,
                }
            }
            ProcessFdTarget::FileWrite { path, append } => {
                let mut opts = std::fs::OpenOptions::new();
                opts.write(true).create(true);
                if *append {
                    opts.append(true);
                } else {
                    opts.truncate(true);
                }
                let file = opts
                    .open(path)
                    .map_err(|e| format!("failed to open subprocess capture {path}: {e}"))?;
                let raw = file.as_raw_fd() as u32;
                open_files.push(file);
                if binding.fd == 1 {
                    stdout_wasi_fd = Some(raw);
                }
                FdSpec {
                    child_fd: binding.fd,
                    kind: EXEC_V2_FD_WASI_FD,
                    value: raw,
                }
            }
            ProcessFdTarget::Dup(src_fd) => {
                // The only Dup the subprocess shim produces is stderr -> stdout
                // (subprocess.STDOUT). Point child fd 2 at whatever WASI fd we
                // resolved for child fd 1: if stdout is captured to a file, both
                // child fds share that capture file (matching the preview2 host,
                // which merges into the stdout sink); if stdout is inherited,
                // fall through to inherit so stderr also inherits.
                if *src_fd == 1 {
                    if let Some(raw) = stdout_wasi_fd {
                        FdSpec {
                            child_fd: binding.fd,
                            kind: EXEC_V2_FD_WASI_FD,
                            value: raw,
                        }
                    } else {
                        FdSpec {
                            child_fd: binding.fd,
                            kind: EXEC_V2_FD_INHERIT,
                            value: 0,
                        }
                    }
                } else {
                    return Err(format!(
                        "unsupported subprocess fd dup target: fd {src_fd}"
                    ));
                }
            }
            ProcessFdTarget::PipeRead(_) | ProcessFdTarget::PipeWrite(_) => {
                // do_subprocess_run never produces pipe targets (it uses temp
                // files for capture), so this should be unreachable.
                return Err("subprocess pipe fd targets are not supported on the threaded build"
                    .to_string());
            }
        };
        specs.push(spec);
    }

    // Build the argv NUL-separated buffer (argv[0]\0argv[1]\0...).
    let mut argv_buf: Vec<u8> = Vec::new();
    for (i, arg) in argv.iter().enumerate() {
        if i > 0 {
            argv_buf.push(0);
        }
        argv_buf.extend_from_slice(arg.as_bytes());
    }

    let cwd_bytes = cwd.as_bytes();

    // Lay out the fd-spec array: fds_len records of 16 bytes each.
    let mut fds_buf: Vec<u8> = Vec::with_capacity(specs.len() * 16);
    for spec in &specs {
        fds_buf.extend_from_slice(&spec.child_fd.to_le_bytes());
        fds_buf.extend_from_slice(&spec.kind.to_le_bytes());
        fds_buf.extend_from_slice(&spec.value.to_le_bytes());
        fds_buf.extend_from_slice(&0u32.to_le_bytes()); // flags
    }

    // The context struct references the three buffers by pointer, so they must
    // outlive the host call. They are owned here on the stack for its duration.
    let ctx: [u32; 6] = [
        argv_buf.as_ptr() as u32,
        argv_buf.len() as u32,
        cwd_bytes.as_ptr() as u32,
        cwd_bytes.len() as u32,
        fds_buf.as_ptr() as u32,
        specs.len() as u32,
    ];
    let mut ctx_buf: Vec<u8> = Vec::with_capacity(24);
    for word in ctx {
        ctx_buf.extend_from_slice(&word.to_le_bytes());
    }

    let exit_code =
        unsafe { monocp_exec_with_context_v2(ctx_buf.as_ptr() as u32, ctx_buf.len() as u32) };

    // Keep the backing files and buffers alive until after the (blocking) call.
    drop(open_files);
    drop(argv_buf);
    drop(fds_buf);
    drop(ctx_buf);

    if exit_code < 0 {
        return Err("exec_with_context_v2 host dispatch failed".to_string());
    }
    Ok(exit_code)
}
