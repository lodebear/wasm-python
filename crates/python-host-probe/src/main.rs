#![allow(missing_docs)]

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::{self, ClientConfig};
use wasmtime::component::{Component, Linker, ResourceTable, Val};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

#[derive(Debug, Clone, wasmtime::component::ComponentType, wasmtime::component::Lift)]
#[component(record)]
struct ExecuteOutput {
    #[component(name = "stdout")]
    stdout: String,
    #[component(name = "stderr")]
    stderr: String,
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

#[async_trait]
trait HostSockets: Send + Sync {
    async fn tcp_connect(&self, host: String, port: u16) -> std::result::Result<u32, TcpError>;
    async fn tcp_read(&self, handle: u32, len: u32) -> std::result::Result<Vec<u8>, TcpError>;
    async fn tcp_write(&self, handle: u32, data: Vec<u8>) -> std::result::Result<u32, TcpError>;
    async fn tcp_close(&self, handle: u32);

    async fn tls_upgrade(
        &self,
        tcp_handle: u32,
        hostname: String,
    ) -> std::result::Result<u32, TlsError>;
    async fn tls_read(&self, handle: u32, len: u32) -> std::result::Result<Vec<u8>, TlsError>;
    async fn tls_write(&self, handle: u32, data: Vec<u8>) -> std::result::Result<u32, TlsError>;
    async fn tls_close(&self, handle: u32);
}

#[derive(Debug)]
struct TokioSocketTables {
    next_handle: u32,
    tcp: HashMap<u32, TcpStream>,
    tls: HashMap<u32, TlsStream<TcpStream>>,
}

impl TokioSocketTables {
    fn new() -> Self {
        Self {
            next_handle: 1,
            tcp: HashMap::new(),
            tls: HashMap::new(),
        }
    }

    fn alloc_handle(&mut self) -> u32 {
        let handle = self.next_handle;
        self.next_handle = self.next_handle.wrapping_add(1);
        if self.next_handle == 0 {
            self.next_handle = 1;
        }
        handle
    }
}

struct TokioHostSockets {
    tables: Mutex<TokioSocketTables>,
    tls_connector: TlsConnector,
}

impl TokioHostSockets {
    fn new() -> Self {
        let mut roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

        let native = rustls_native_certs::load_native_certs();
        for cert in native.certs {
            let _ = roots.add(cert);
        }

        let tls = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();

        Self {
            tables: Mutex::new(TokioSocketTables::new()),
            tls_connector: TlsConnector::from(Arc::new(tls)),
        }
    }
}

#[async_trait]
impl HostSockets for TokioHostSockets {
    async fn tcp_connect(&self, host: String, port: u16) -> std::result::Result<u32, TcpError> {
        eprintln!("[host-sockets] tcp.connect {}:{}", host, port);
        let stream = TcpStream::connect((host.as_str(), port))
            .await
            .map_err(map_tcp_io_error)?;

        let mut tables = self.tables.lock().await;
        let handle = tables.alloc_handle();
        tables.tcp.insert(handle, stream);
        Ok(handle)
    }

    async fn tcp_read(&self, handle: u32, len: u32) -> std::result::Result<Vec<u8>, TcpError> {
        let mut tables = self.tables.lock().await;
        let stream = tables.tcp.get_mut(&handle).ok_or(TcpError::InvalidHandle)?;

        let mut buf = vec![0_u8; len as usize];
        let n = stream.read(&mut buf).await.map_err(map_tcp_io_error)?;
        buf.truncate(n);
        Ok(buf)
    }

    async fn tcp_write(&self, handle: u32, data: Vec<u8>) -> std::result::Result<u32, TcpError> {
        let mut tables = self.tables.lock().await;
        let stream = tables.tcp.get_mut(&handle).ok_or(TcpError::InvalidHandle)?;

        let n = stream.write(&data).await.map_err(map_tcp_io_error)?;
        Ok(n as u32)
    }

    async fn tcp_close(&self, handle: u32) {
        let mut tables = self.tables.lock().await;
        tables.tcp.remove(&handle);
    }

    async fn tls_upgrade(
        &self,
        tcp_handle: u32,
        hostname: String,
    ) -> std::result::Result<u32, TlsError> {
        eprintln!(
            "[host-sockets] tls.upgrade handle={} hostname={}",
            tcp_handle, hostname
        );
        let tcp_stream = {
            let mut tables = self.tables.lock().await;
            tables
                .tcp
                .remove(&tcp_handle)
                .ok_or(TlsError::Tcp(TcpError::InvalidHandle))?
        };

        let server_name = rustls::pki_types::ServerName::try_from(hostname.clone())
            .map_err(|_| TlsError::CertificateError(format!("invalid hostname: {hostname}")))?;

        let tls_stream = self
            .tls_connector
            .connect(server_name, tcp_stream)
            .await
            .map_err(|e| {
                eprintln!("[host-sockets] tls handshake failed: {e}");
                TlsError::HandshakeFailed(e.to_string())
            })?;

        let mut tables = self.tables.lock().await;
        let handle = tables.alloc_handle();
        tables.tls.insert(handle, tls_stream);
        Ok(handle)
    }

    async fn tls_read(&self, handle: u32, len: u32) -> std::result::Result<Vec<u8>, TlsError> {
        let mut tables = self.tables.lock().await;
        let stream = tables.tls.get_mut(&handle).ok_or(TlsError::InvalidHandle)?;

        let mut buf = vec![0_u8; len as usize];
        let n = stream
            .read(&mut buf)
            .await
            .map_err(|e| TlsError::Tcp(map_tcp_io_error(e)))?;
        buf.truncate(n);
        Ok(buf)
    }

    async fn tls_write(&self, handle: u32, data: Vec<u8>) -> std::result::Result<u32, TlsError> {
        let mut tables = self.tables.lock().await;
        let stream = tables.tls.get_mut(&handle).ok_or(TlsError::InvalidHandle)?;

        let n = stream
            .write(&data)
            .await
            .map_err(|e| TlsError::Tcp(map_tcp_io_error(e)))?;
        Ok(n as u32)
    }

    async fn tls_close(&self, handle: u32) {
        let mut tables = self.tables.lock().await;
        tables.tls.remove(&handle);
    }
}

fn map_tcp_io_error(e: std::io::Error) -> TcpError {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::ConnectionRefused => TcpError::ConnectionRefused,
        ErrorKind::ConnectionReset => TcpError::ConnectionReset,
        ErrorKind::TimedOut => TcpError::TimedOut,
        ErrorKind::AddrNotAvailable | ErrorKind::NotFound => TcpError::HostNotFound,
        _ => TcpError::IoError(e.to_string()),
    }
}

struct State {
    wasi: WasiCtx,
    table: ResourceTable,
    sockets: Arc<dyn HostSockets>,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn find_stdlib() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("PYTHON_STDLIB") {
        let p = PathBuf::from(path);
        if p.join("encodings").exists() {
            return Ok(p);
        }
    }

    let root = workspace_root();
    let candidates = [root.join("crates/mcpone-python-wasm-runtime/tests/python-stdlib")];

    candidates
        .into_iter()
        .find(|p| p.join("encodings").exists())
        .ok_or_else(|| anyhow!("Python stdlib not found. Set PYTHON_STDLIB=/path/to/python-stdlib"))
}

fn component_path() -> PathBuf {
    if let Ok(path) = std::env::var("PY_RUNTIME_COMPONENT") {
        return PathBuf::from(path);
    }
    workspace_root().join("crates/mcpone-python-runtime/runtime.wasm")
}

fn default_python_code() -> String {
    r#"
import urllib.request
resp = urllib.request.urlopen("https://example.com", timeout=10)
print("status", resp.status)
print("bytes", len(resp.read(128)))
"#
    .to_string()
}

#[tokio::main]
async fn main() -> Result<()> {
    let wasm_path = component_path();
    if !wasm_path.exists() {
        return Err(anyhow!(
            "runtime.wasm not found at {}. Build it first with BUILD_MCPONE_PY_RUNTIME=1 cargo build -p mcpone-python-runtime --release",
            wasm_path.display()
        ));
    }

    let stdlib = find_stdlib()?;
    let site_packages = workspace_root().join("target/python-host-probe-site-packages");
    std::fs::create_dir_all(&site_packages)?;

    let mut config = Config::new();
    config.async_support(true);
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);

    let engine = Engine::new(&config)?;
    let component = Component::from_file(&engine, &wasm_path)
        .with_context(|| format!("failed to load component at {}", wasm_path.display()))?;

    let wasi = WasiCtxBuilder::new()
        .inherit_stdio()
        .env("PYTHONHOME", "/python-stdlib")
        .env("PYTHONPATH", "/python-stdlib:/site-packages")
        .preopened_dir(&stdlib, "/python-stdlib", DirPerms::READ, FilePerms::READ)?
        .preopened_dir(
            &site_packages,
            "/site-packages",
            DirPerms::READ,
            FilePerms::READ,
        )?
        .build();

    let state = State {
        wasi,
        table: ResourceTable::new(),
        sockets: Arc::new(TokioHostSockets::new()),
    };
    let mut store = Store::new(&engine, state);

    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;

    add_callback_imports(&mut linker)?;
    add_socket_imports(&mut linker)?;
    add_process_imports(&mut linker)?;

    let instance = linker.instantiate_async(&mut store, &component).await?;

    let execute = instance
        .get_typed_func::<(String,), (Result<ExecuteOutput, String>,)>(&mut store, "[async]execute")
        .or_else(|_| {
            instance.get_typed_func::<(String,), (Result<ExecuteOutput, String>,)>(
                &mut store, "execute",
            )
        })?;

    let code = std::env::args().nth(1).unwrap_or_else(default_python_code);

    let (result,) = execute.call_async(&mut store, (code,)).await?;
    execute.post_return_async(&mut store).await?;

    match result {
        Ok(output) => {
            if !output.stdout.is_empty() {
                println!("\n--- execute stdout ---\n{}", output.stdout);
            }
            if !output.stderr.is_empty() {
                eprintln!("\n--- execute stderr ---\n{}", output.stderr);
            }
            Ok(())
        }
        Err(err) => Err(anyhow!("execute returned error: {err}")),
    }
}

fn add_callback_imports(linker: &mut Linker<State>) -> Result<()> {
    use wasmtime::component::Accessor;

    linker.root().func_wrap_concurrent(
        "invoke",
        |_accessor: &Accessor<State>, (_name, _args): (String, String)| {
            Box::pin(async move {
                Ok((Result::<String, String>::Err(
                    "callbacks are disabled in python-host-probe".to_string(),
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
         (_lineno, _event, _ctx_json): (u32, String, String)| { Ok(()) },
    )?;

    linker.root().func_wrap(
        "report-output",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (stream, data): (u32, String)| {
            if stream == 0 {
                print!("{data}");
            } else {
                eprint!("{data}");
            }
            Ok(())
        },
    )?;

    Ok(())
}

fn add_socket_imports(linker: &mut Linker<State>) -> Result<()> {
    let mut tcp = linker.instance("mcpone:host-sockets/tcp@0.1.0")?;

    tcp.func_wrap_async(
        "connect",
        |ctx: wasmtime::StoreContextMut<'_, State>, (host, port): (String, u16)| {
            let sockets = ctx.data().sockets.clone();
            Box::new(async move { Ok((sockets.tcp_connect(host, port).await,)) })
        },
    )?;

    tcp.func_wrap_async(
        "read",
        |ctx: wasmtime::StoreContextMut<'_, State>, (handle, len): (u32, u32)| {
            let sockets = ctx.data().sockets.clone();
            Box::new(async move { Ok((sockets.tcp_read(handle, len).await,)) })
        },
    )?;

    tcp.func_wrap_async(
        "write",
        |ctx: wasmtime::StoreContextMut<'_, State>, (handle, data): (u32, Vec<u8>)| {
            let sockets = ctx.data().sockets.clone();
            Box::new(async move { Ok((sockets.tcp_write(handle, data).await,)) })
        },
    )?;

    tcp.func_wrap_async(
        "close",
        |ctx: wasmtime::StoreContextMut<'_, State>, (handle,): (u32,)| {
            let sockets = ctx.data().sockets.clone();
            Box::new(async move {
                sockets.tcp_close(handle).await;
                Ok(())
            })
        },
    )?;

    let mut tls = linker.instance("mcpone:host-sockets/tls@0.1.0")?;

    tls.func_wrap_async(
        "upgrade",
        |ctx: wasmtime::StoreContextMut<'_, State>, (tcp_handle, hostname): (u32, String)| {
            let sockets = ctx.data().sockets.clone();
            Box::new(async move { Ok((sockets.tls_upgrade(tcp_handle, hostname).await,)) })
        },
    )?;

    tls.func_wrap_async(
        "read",
        |ctx: wasmtime::StoreContextMut<'_, State>, (handle, len): (u32, u32)| {
            let sockets = ctx.data().sockets.clone();
            Box::new(async move { Ok((sockets.tls_read(handle, len).await,)) })
        },
    )?;

    tls.func_wrap_async(
        "write",
        |ctx: wasmtime::StoreContextMut<'_, State>, (handle, data): (u32, Vec<u8>)| {
            let sockets = ctx.data().sockets.clone();
            Box::new(async move { Ok((sockets.tls_write(handle, data).await,)) })
        },
    )?;

    tls.func_wrap_async(
        "close",
        |ctx: wasmtime::StoreContextMut<'_, State>, (handle,): (u32,)| {
            let sockets = ctx.data().sockets.clone();
            Box::new(async move {
                sockets.tls_close(handle).await;
                Ok(())
            })
        },
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
#[component(record)]
struct ProbePipeHandles {
    #[component(name = "read")]
    read: u32,
    #[component(name = "write")]
    write: u32,
}

#[derive(
    Clone,
    Debug,
    wasmtime::component::ComponentType,
    wasmtime::component::Lift,
    wasmtime::component::Lower,
)]
#[component(record)]
struct ProbeFileWriteTarget {
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
enum ProbeFdTarget {
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
    FileWrite(ProbeFileWriteTarget),
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
struct ProbeFdBinding {
    #[component(name = "fd")]
    fd: u32,
    #[component(name = "target")]
    target: ProbeFdTarget,
}

fn add_process_imports(linker: &mut Linker<State>) -> Result<()> {
    let mut process = linker.instance("monocp:host/monocp@0.2.0")?;

    process.func_wrap(
        "pipe-create",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (): ()| {
            Ok((ProbePipeHandles { read: 0, write: 0 },))
        },
    )?;

    process.func_wrap(
        "pipe-clone",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_handle,): (u32,)| Ok((0_u32,)),
    )?;

    process.func_wrap(
        "pipe-read",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_handle, _max_len): (u32, u32)| {
            Ok((Vec::<u8>::new(),))
        },
    )?;

    process.func_wrap(
        "pipe-write",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_handle, data): (u32, Vec<u8>)| {
            Ok((u32::try_from(data.len()).unwrap_or(u32::MAX),))
        },
    )?;

    process.func_wrap(
        "pipe-close",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_handle,): (u32,)| Ok(()),
    )?;

    process.func_wrap(
        "process-id",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (): ()| Ok((1_u32,)),
    )?;

    process.func_wrap(
        "spawn-with-context",
        |_ctx: wasmtime::StoreContextMut<'_, State>,
         (_argv, _cwd, _fds): (Vec<String>, String, Vec<ProbeFdBinding>)| {
            Ok((Result::<u64, String>::Err(
                "process execution is not implemented in python-host-probe".into(),
            ),))
        },
    )?;

    process.func_wrap(
        "wait-child",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_child,): (u64,)| {
            Ok((Result::<i32, String>::Err("invalid child handle".into()),))
        },
    )?;

    process.func_wrap(
        "try-wait-child",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_child,): (u64,)| Ok((Option::<i32>::None,)),
    )?;

    process.func_wrap(
        "close-child",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_child,): (u64,)| Ok(()),
    )?;

    process.func_wrap(
        "kill-child",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_child,): (u64,)| {
            Ok((Result::<(), String>::Err("invalid child handle".into()),))
        },
    )?;

    Ok(())
}
