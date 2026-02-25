// Tests use expect/unwrap for simplicity
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Integration tests for the Python subprocess shim.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use wasmtime::component::{Component, Linker, ResourceTable, Val};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wit_component::{StringEncoding, embed_component_metadata};

fn decompress_zstd(data: &[u8]) -> Vec<u8> {
    zstd::decode_all(Cursor::new(data)).expect("failed to decompress")
}

#[derive(Debug, Clone, wasmtime::component::ComponentType, wasmtime::component::Lift)]
#[component(record)]
struct ExecuteOutput {
    #[component(name = "stdout")]
    stdout: String,
    #[component(name = "stderr")]
    stderr: String,
}

#[derive(Debug, Default)]
struct FakeProcesses {
    next_child: u64,
    exits: HashMap<u64, i32>,
}

impl FakeProcesses {
    fn alloc_child(&mut self) -> u64 {
        let child = if self.next_child == 0 {
            1
        } else {
            self.next_child
        };
        self.next_child = child.wrapping_add(1);
        if self.next_child == 0 {
            self.next_child = 1;
        }
        child
    }
}

struct State {
    wasi: WasiCtx,
    table: ResourceTable,
    processes: Arc<Mutex<FakeProcesses>>,
    workspace_host: PathBuf,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

fn load_lib(libs_dir: &Path, name: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let decompressed = libs_dir.join("decompressed").join(name);
    if decompressed.exists() {
        return Ok(std::fs::read(&decompressed)?);
    }

    let compressed = libs_dir.join(format!("{name}.zst"));
    if compressed.exists() {
        return Ok(decompress_zstd(&std::fs::read(&compressed)?));
    }

    Err(format!("Library not found: {name}").into())
}

fn build_component() -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let project_root = manifest_dir.parent().unwrap().parent().unwrap();
    let libs_dir = project_root.join("crates/mcpone-python-runtime/libs");

    let runtime_path = manifest_dir.join("target/libmcpone_python_runtime.so");
    if !runtime_path.exists() {
        panic!(
            "Runtime not found at {}. Run `BUILD_MCPONE_PY_RUNTIME=1 cargo build -p mcpone-python-runtime` first",
            runtime_path.display()
        );
    }

    let runtime = std::fs::read(&runtime_path)?;

    let libc = load_lib(&libs_dir, "libc.so")?;
    let libcxx = load_lib(&libs_dir, "libc++.so")?;
    let libcxxabi = load_lib(&libs_dir, "libc++abi.so")?;
    let wasi_clocks = load_lib(&libs_dir, "libwasi-emulated-process-clocks.so")?;
    let wasi_signal = load_lib(&libs_dir, "libwasi-emulated-signal.so")?;
    let wasi_mman = load_lib(&libs_dir, "libwasi-emulated-mman.so")?;
    let wasi_getpid = load_lib(&libs_dir, "libwasi-emulated-getpid.so")?;
    let libpython = load_lib(&libs_dir, "libpython3.14.so")?;
    let adapter = load_lib(&libs_dir, "wasi_snapshot_preview1.reactor.wasm")?;

    let wit_path = project_root.join("crates/mcpone-python-runtime/wit");
    let mut resolve = wit_parser::Resolve::default();
    let (pkg_id, _) = resolve.push_dir(&wit_path)?;
    let world_id = resolve.select_world(&[pkg_id], Some("sandbox"))?;

    let mut opts = wit_dylib::DylibOpts {
        interpreter: Some("libmcpone_python_runtime.so".to_string()),
        async_: wit_dylib::AsyncFilterSet::default(),
    };

    let mut bindings = wit_dylib::create(&resolve, world_id, Some(&mut opts));
    embed_component_metadata(&mut bindings, &resolve, world_id, StringEncoding::UTF8)?;

    let linker = wit_component::Linker::default()
        .validate(true)
        .use_built_in_libdl(true)
        .library("libwasi-emulated-process-clocks.so", &wasi_clocks, false)?
        .library("libwasi-emulated-signal.so", &wasi_signal, false)?
        .library("libwasi-emulated-mman.so", &wasi_mman, false)?
        .library("libwasi-emulated-getpid.so", &wasi_getpid, false)?
        .library("libc.so", &libc, false)?
        .library("libc++abi.so", &libcxxabi, false)?
        .library("libc++.so", &libcxx, false)?
        .library("libpython3.14.so", &libpython, false)?
        .library("libmcpone_python_runtime.so", &runtime, false)?
        .library("libmcpone_python_bindings.so", &bindings, false)?
        .adapter("wasi_snapshot_preview1", &adapter)?;

    Ok(linker.encode()?)
}

#[derive(
    Clone,
    Debug,
    wasmtime::component::ComponentType,
    wasmtime::component::Lift,
    wasmtime::component::Lower,
)]
#[component(record)]
struct FileWriteTarget {
    #[component(name = "path")]
    path: String,
    #[component(name = "append")]
    append: bool,
}

#[derive(
    Clone,
    Debug,
    wasmtime::component::ComponentType,
    wasmtime::component::Lift,
    wasmtime::component::Lower,
)]
#[component(variant)]
enum FdTarget {
    #[component(name = "inherit")]
    Inherit,
    #[component(name = "close")]
    Close,
    #[component(name = "pipe-read")]
    PipeRead(u32),
    #[component(name = "pipe-write")]
    PipeWrite(u32),
    #[component(name = "file-read")]
    FileRead(String),
    #[component(name = "file-write")]
    FileWrite(FileWriteTarget),
    #[component(name = "dup")]
    Dup(u32),
}

#[derive(
    Clone,
    Debug,
    wasmtime::component::ComponentType,
    wasmtime::component::Lift,
    wasmtime::component::Lower,
)]
#[component(record)]
struct FdBinding {
    #[component(name = "fd")]
    fd: u32,
    #[component(name = "target")]
    target: FdTarget,
}

fn resolve_path(cwd: &str, path: &str, workspace_host: &Path) -> PathBuf {
    let p = Path::new(path);
    let guest_path = if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(cwd).join(p)
    };

    if let Ok(stripped) = guest_path.strip_prefix("/workspace") {
        if stripped.as_os_str().is_empty() {
            workspace_host.to_path_buf()
        } else {
            workspace_host.join(stripped)
        }
    } else {
        guest_path
    }
}

fn write_target(
    target: &FdTarget,
    cwd: &str,
    workspace_host: &Path,
    data: &[u8],
) -> Result<(), String> {
    match target {
        FdTarget::FileWrite(t) => {
            let path = resolve_path(cwd, &t.path, workspace_host);
            let mut opts = std::fs::OpenOptions::new();
            opts.create(true).write(true);
            if t.append {
                opts.append(true);
            } else {
                opts.truncate(true);
            }
            let mut file = opts
                .open(&path)
                .map_err(|e| format!("failed to open {}: {e}", path.display()))?;
            std::io::Write::write_all(&mut file, data)
                .map_err(|e| format!("failed to write {}: {e}", path.display()))
        }
        _ => Ok(()),
    }
}

fn read_stdin(
    target: Option<&FdTarget>,
    cwd: &str,
    workspace_host: &Path,
) -> Result<Vec<u8>, String> {
    match target {
        Some(FdTarget::FileRead(path)) => {
            let path = resolve_path(cwd, path, workspace_host);
            std::fs::read(&path).map_err(|e| format!("failed to read {}: {e}", path.display()))
        }
        _ => Ok(Vec::new()),
    }
}

fn binding_target<'a>(bindings: &'a [FdBinding], fd: u32) -> Option<&'a FdTarget> {
    bindings.iter().find(|b| b.fd == fd).map(|b| &b.target)
}

fn simulate_command(
    argv: &[String],
    cwd: &str,
    workspace_host: &Path,
    fds: &[FdBinding],
) -> Result<i32, String> {
    let cmd = argv
        .first()
        .map(String::as_str)
        .ok_or_else(|| "missing argv[0]".to_string())?;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit = 0_i32;

    match cmd {
        "mcpone-test-echo" => stdout = format!("{}\n", argv[1..].join(" ")).into_bytes(),
        "mcpone-test-cat" => stdout = read_stdin(binding_target(fds, 0), cwd, workspace_host)?,
        "mcpone-test-stderr" => stderr = b"from-stderr\n".to_vec(),
        "mcpone-test-fail" => {
            exit = argv.get(1).and_then(|s| s.parse::<i32>().ok()).unwrap_or(1);
            stderr = format!("failed {exit}\n").into_bytes();
        }
        _ => return Err(format!("unknown test command: {cmd}")),
    }

    let stdout_target = binding_target(fds, 1).unwrap_or(&FdTarget::Inherit);
    let stderr_target = binding_target(fds, 2).unwrap_or(&FdTarget::Inherit);

    if let FdTarget::Dup(fd) = stderr_target {
        if *fd == 1 {
            stdout.extend_from_slice(&stderr);
            stderr.clear();
        }
    }

    write_target(stdout_target, cwd, workspace_host, &stdout)?;
    write_target(stderr_target, cwd, workspace_host, &stderr)?;

    Ok(exit)
}

fn add_callback_imports(linker: &mut Linker<State>) -> Result<(), Box<dyn std::error::Error>> {
    use wasmtime::component::Accessor;

    linker.root().func_wrap_concurrent(
        "invoke",
        |_accessor: &Accessor<State>, (_name, _args): (String, String)| {
            Box::pin(async move {
                Ok((Result::<String, String>::Err(
                    "callbacks disabled in subprocess test".to_string(),
                ),))
            })
        },
    )?;

    linker.root().func_new(
        "list-callbacks",
        |_ctx: wasmtime::StoreContextMut<'_, State>,
         _func_ty: wasmtime::component::types::ComponentFunc,
         _params: &[Val],
         results: &mut [Val]| {
            results[0] = Val::List(vec![]);
            Ok(())
        },
    )?;

    linker.root().func_wrap(
        "report-trace",
        |_ctx: wasmtime::StoreContextMut<'_, State>,
         (_lineno, _event_json, _context_json): (u32, String, String)| { Ok(()) },
    )?;

    linker.root().func_wrap(
        "report-output",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_stream, _data): (u32, String)| Ok(()),
    )?;

    Ok(())
}

#[derive(
    Clone,
    Debug,
    wasmtime::component::ComponentType,
    wasmtime::component::Lift,
    wasmtime::component::Lower,
)]
#[component(variant)]
enum TcpError {
    #[component(name = "connection-refused")]
    ConnectionRefused,
    #[component(name = "connection-reset")]
    ConnectionReset,
    #[component(name = "timed-out")]
    TimedOut,
    #[component(name = "host-not-found")]
    HostNotFound,
    #[component(name = "io-error")]
    IoError(String),
    #[component(name = "not-permitted")]
    NotPermitted(String),
    #[component(name = "invalid-handle")]
    InvalidHandle,
}

#[derive(
    Clone,
    Debug,
    wasmtime::component::ComponentType,
    wasmtime::component::Lift,
    wasmtime::component::Lower,
)]
#[component(variant)]
enum TlsError {
    #[component(name = "tcp")]
    Tcp(TcpError),
    #[component(name = "handshake-failed")]
    HandshakeFailed(String),
    #[component(name = "certificate-error")]
    CertificateError(String),
    #[component(name = "invalid-handle")]
    InvalidHandle,
}

fn add_network_stubs(linker: &mut Linker<State>) -> Result<(), Box<dyn std::error::Error>> {
    let mut tcp = linker.instance("mcpone:host-sockets/tcp@0.1.0")?;
    tcp.func_wrap_async(
        "connect",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_host, _port): (String, u16)| {
            Box::new(async move {
                Ok((Result::<u32, TcpError>::Err(TcpError::NotPermitted(
                    "networking disabled".into(),
                )),))
            })
        },
    )?;
    tcp.func_wrap_async(
        "read",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_handle, _len): (u32, u32)| {
            Box::new(async move {
                Ok((Result::<Vec<u8>, TcpError>::Err(TcpError::NotPermitted(
                    "networking disabled".into(),
                )),))
            })
        },
    )?;
    tcp.func_wrap_async(
        "write",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_handle, _data): (u32, Vec<u8>)| {
            Box::new(async move {
                Ok((Result::<u32, TcpError>::Err(TcpError::NotPermitted(
                    "networking disabled".into(),
                )),))
            })
        },
    )?;
    tcp.func_wrap(
        "close",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_handle,): (u32,)| Ok(()),
    )?;

    let mut tls = linker.instance("mcpone:host-sockets/tls@0.1.0")?;
    tls.func_wrap_async(
        "upgrade",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_tcp, _host): (u32, String)| {
            Box::new(async move {
                Ok((Result::<u32, TlsError>::Err(TlsError::HandshakeFailed(
                    "networking disabled".into(),
                )),))
            })
        },
    )?;
    tls.func_wrap_async(
        "read",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_handle, _len): (u32, u32)| {
            Box::new(async move {
                Ok((Result::<Vec<u8>, TlsError>::Err(TlsError::HandshakeFailed(
                    "networking disabled".into(),
                )),))
            })
        },
    )?;
    tls.func_wrap_async(
        "write",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_handle, _data): (u32, Vec<u8>)| {
            Box::new(async move {
                Ok((Result::<u32, TlsError>::Err(TlsError::HandshakeFailed(
                    "networking disabled".into(),
                )),))
            })
        },
    )?;
    tls.func_wrap(
        "close",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_handle,): (u32,)| Ok(()),
    )?;

    Ok(())
}

fn add_process_stubs(linker: &mut Linker<State>) -> Result<(), Box<dyn std::error::Error>> {
    #[derive(
        Clone,
        Debug,
        wasmtime::component::ComponentType,
        wasmtime::component::Lift,
        wasmtime::component::Lower,
    )]
    #[component(record)]
    struct PipeHandles {
        #[component(name = "read")]
        read: u32,
        #[component(name = "write")]
        write: u32,
    }

    let mut process = linker.instance("monocp:host/monocp@0.2.0")?;

    process.func_wrap(
        "pipe-create",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (): ()| {
            Ok((PipeHandles { read: 0, write: 0 },))
        },
    )?;
    process.func_wrap(
        "pipe-clone",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_h,): (u32,)| Ok((0_u32,)),
    )?;
    process.func_wrap(
        "pipe-read",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_h, _max): (u32, u32)| {
            Ok((Vec::<u8>::new(),))
        },
    )?;
    process.func_wrap(
        "pipe-write",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_h, data): (u32, Vec<u8>)| {
            Ok((u32::try_from(data.len()).unwrap_or(u32::MAX),))
        },
    )?;
    process.func_wrap(
        "pipe-close",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_h,): (u32,)| Ok(()),
    )?;
    process.func_wrap(
        "process-id",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (): ()| Ok((99_u32,)),
    )?;

    process.func_wrap(
        "spawn-with-context",
        |ctx: wasmtime::StoreContextMut<'_, State>,
         (argv, cwd, fds): (Vec<String>, String, Vec<FdBinding>)| {
            let workspace = ctx.data().workspace_host.clone();
            let exit = match simulate_command(&argv, &cwd, workspace.as_path(), &fds) {
                Ok(code) => code,
                Err(msg) => return Ok((Result::<u64, String>::Err(msg),)),
            };

            let mut proc = ctx.data().processes.lock().unwrap();
            let child = proc.alloc_child();
            proc.exits.insert(child, exit);
            Ok((Result::<u64, String>::Ok(child),))
        },
    )?;

    process.func_wrap(
        "wait-child",
        |ctx: wasmtime::StoreContextMut<'_, State>, (child,): (u64,)| {
            let mut proc = ctx.data().processes.lock().unwrap();
            let result = match proc.exits.remove(&child) {
                Some(code) => Result::<i32, String>::Ok(code),
                None => Result::<i32, String>::Err("invalid child handle".into()),
            };
            Ok((result,))
        },
    )?;

    process.func_wrap(
        "try-wait-child",
        |ctx: wasmtime::StoreContextMut<'_, State>, (child,): (u64,)| {
            let proc = ctx.data().processes.lock().unwrap();
            Ok((proc.exits.get(&child).copied(),))
        },
    )?;

    process.func_wrap(
        "close-child",
        |ctx: wasmtime::StoreContextMut<'_, State>, (child,): (u64,)| {
            let mut proc = ctx.data().processes.lock().unwrap();
            proc.exits.remove(&child);
            Ok(())
        },
    )?;

    process.func_wrap(
        "kill-child",
        |ctx: wasmtime::StoreContextMut<'_, State>, (child,): (u64,)| {
            let mut proc = ctx.data().processes.lock().unwrap();
            let result = if proc.exits.remove(&child).is_some() {
                Result::<(), String>::Ok(())
            } else {
                Result::<(), String>::Err("invalid child handle".into())
            };
            Ok((result,))
        },
    )?;

    Ok(())
}

#[tokio::test]
async fn test_python_subprocess_shim() -> Result<(), Box<dyn std::error::Error>> {
    let component_bytes = build_component()?;

    let mut config = Config::new();
    config.async_support(true);
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);

    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, &component_bytes)?;

    let stdlib_path = std::env::var("MCPONE_PYTHON_STDLIB")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .unwrap_or_else(|| {
            let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            manifest_dir.join("tests/python-stdlib")
        });

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let site_packages_path = manifest_dir.join("tests/site-packages");
    let workdir_path = manifest_dir.join("tests/workdir");
    std::fs::create_dir_all(&site_packages_path)?;
    std::fs::create_dir_all(&workdir_path)?;

    let wasi = WasiCtxBuilder::new()
        .inherit_stdio()
        .env("PYTHONPATH", "/python-stdlib:/site-packages")
        .preopened_dir(
            &stdlib_path,
            "/python-stdlib",
            wasmtime_wasi::DirPerms::READ,
            wasmtime_wasi::FilePerms::READ,
        )?
        .preopened_dir(
            &site_packages_path,
            "/site-packages",
            wasmtime_wasi::DirPerms::READ,
            wasmtime_wasi::FilePerms::READ,
        )?
        .preopened_dir(
            &workdir_path,
            "/workspace",
            wasmtime_wasi::DirPerms::READ | wasmtime_wasi::DirPerms::MUTATE,
            wasmtime_wasi::FilePerms::READ | wasmtime_wasi::FilePerms::WRITE,
        )?
        .build();

    let state = State {
        wasi,
        table: ResourceTable::new(),
        processes: Arc::new(Mutex::new(FakeProcesses::default())),
        workspace_host: workdir_path,
    };

    let mut store = Store::new(&engine, state);

    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    add_callback_imports(&mut linker)?;
    add_network_stubs(&mut linker)?;
    add_process_stubs(&mut linker)?;

    let instance = linker.instantiate_async(&mut store, &component).await?;
    let execute = instance
        .get_typed_func::<(String,), (Result<ExecuteOutput, String>,)>(&mut store, "[async]execute")
        .or_else(|_| {
            instance.get_typed_func::<(String,), (Result<ExecuteOutput, String>,)>(
                &mut store, "execute",
            )
        })?;

    let code = r#"
import subprocess

WORKDIR = "/workspace"

res = subprocess.run(["mcpone-test-echo", "hello", "world"], capture_output=True, text=True, cwd=WORKDIR)
assert res.returncode == 0
assert res.stdout.strip() == "hello world"
assert res.stderr == ""

cat = subprocess.run(["mcpone-test-cat"], input="abc123", text=True, capture_output=True, cwd=WORKDIR)
assert cat.stdout == "abc123"

merged = subprocess.run(["mcpone-test-stderr"], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, cwd=WORKDIR)
assert merged.stdout.strip() == "from-stderr"
assert merged.stderr is None

try:
    subprocess.run(["mcpone-test-fail", "7"], check=True, capture_output=True, text=True, cwd=WORKDIR)
    raise AssertionError("expected CalledProcessError")
except subprocess.CalledProcessError as e:
    assert e.returncode == 7

print("subprocess-ok")
"#;

    let (result,) = execute.call_async(&mut store, (code.to_string(),)).await?;
    execute.post_return_async(&mut store).await?;

    match result {
        Ok(output) => {
            assert!(
                output.stdout.contains("subprocess-ok"),
                "expected success marker in stdout: {:?}",
                output
            );
            assert!(
                output.stderr.is_empty(),
                "stderr should be empty for subprocess shim test: {:?}",
                output
            );
            Ok(())
        }
        Err(err) => Err(format!("execute failed: {err}").into()),
    }
}
