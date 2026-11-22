// Tests use expect/unwrap for simplicity
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::manual_strip)]

//! Test to verify the runtime component can be instantiated and called.
//!
//! # Prerequisites
//!
//! Before running tests, you need to extract the Python stdlib from componentize-py:
//!
//! ```bash
//! # From the mcpone-python-wasm-runtime crate directory:
//! mkdir -p tests/python-stdlib tests/site-packages
//! tar -xf ../mcpone-python-runtime/.venv/lib/python3.12/site-packages/componentize_py/python-lib.tar.zst \
//!     -C tests/python-stdlib
//! ```
//!
//! # Running
//!
//! ```bash
//! cargo test --package mcpone-python-wasm-runtime --test runtime_test
//! ```

use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wit_component::{StringEncoding, embed_component_metadata};

fn decompress_zstd(data: &[u8]) -> Vec<u8> {
    zstd::decode_all(Cursor::new(data)).expect("failed to decompress")
}

// Define a struct matching the WIT execute-output record
#[derive(Debug, Clone, wasmtime::component::ComponentType, wasmtime::component::Lift)]
#[component(record)]
struct ExecuteOutput {
    #[component(name = "stdout")]
    stdout: String,
    #[component(name = "stderr")]
    stderr: String,
}

#[derive(Debug, Default)]
struct FakeProcessTable {
    next_child: u64,
    children: HashMap<u64, i32>,
    next_pipe: u32,
    pipes: HashMap<u32, Vec<u8>>,
}

impl FakeProcessTable {
    fn alloc_child_handle(&mut self) -> u64 {
        let handle = if self.next_child == 0 {
            1
        } else {
            self.next_child
        };
        self.next_child = handle.wrapping_add(1);
        if self.next_child == 0 {
            self.next_child = 1;
        }
        handle
    }

    fn alloc_pipe_handle(&mut self) -> u32 {
        let handle = if self.next_pipe == 0 {
            1
        } else {
            self.next_pipe
        };
        self.next_pipe = handle.wrapping_add(1);
        if self.next_pipe == 0 {
            self.next_pipe = 1;
        }
        handle
    }
}

struct State {
    ctx: WasiCtx,
    table: ResourceTable,
    processes: Arc<Mutex<FakeProcessTable>>,
    workspace_host: PathBuf,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

/// Load a library from decompressed dir, or decompress from .zst if needed
fn load_lib(libs_dir: &std::path::Path, name: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let decompressed = libs_dir.join("decompressed").join(name);
    if decompressed.exists() {
        return Ok(std::fs::read(&decompressed)?);
    }

    let compressed = libs_dir.join(format!("{name}.zst"));
    if compressed.exists() {
        return Ok(decompress_zstd(&std::fs::read(&compressed)?));
    }

    Err(format!("Library not found: {name} (checked {decompressed:?} and {compressed:?})").into())
}

fn build_component() -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let project_root = manifest_dir.parent().unwrap().parent().unwrap();
    let libs_dir = project_root.join("crates/mcpone-python-runtime/libs");

    // Path to our runtime .so
    let runtime_path = manifest_dir.join("target/libmcpone_python_runtime.so");
    if !runtime_path.exists() {
        panic!(
            "Runtime not found at {}. Run `BUILD_MCPONE_PY_RUNTIME=1 cargo build -p mcpone-python-runtime` first",
            runtime_path.display()
        );
    }

    let runtime = std::fs::read(&runtime_path)?;

    // Load base libraries
    let libc = load_lib(&libs_dir, "libc.so")?;
    let libcxx = load_lib(&libs_dir, "libc++.so")?;
    let libcxxabi = load_lib(&libs_dir, "libc++abi.so")?;
    let wasi_clocks = load_lib(&libs_dir, "libwasi-emulated-process-clocks.so")?;
    let wasi_signal = load_lib(&libs_dir, "libwasi-emulated-signal.so")?;
    let wasi_mman = load_lib(&libs_dir, "libwasi-emulated-mman.so")?;
    let wasi_getpid = load_lib(&libs_dir, "libwasi-emulated-getpid.so")?;
    let libpython = load_lib(&libs_dir, "libpython3.14.so")?;
    let adapter = load_lib(&libs_dir, "wasi_snapshot_preview1.reactor.wasm")?;

    // Parse the WIT files from the wit/ directory
    let wit_path = project_root.join("crates/mcpone-python-runtime/wit");
    let mut resolve = wit_parser::Resolve::default();
    let (pkg_id, _) = resolve.push_dir(&wit_path)?;
    let world_id = resolve.select_world(&[pkg_id], Some("sandbox"))?;

    // Generate bindings pointing to our runtime
    let mut opts = wit_dylib::DylibOpts {
        interpreter: Some("libmcpone_python_runtime.so".to_string()),
        async_: wit_dylib::AsyncFilterSet::default(),
    };

    let mut bindings = wit_dylib::create(&resolve, world_id, Some(&mut opts));
    embed_component_metadata(&mut bindings, &resolve, world_id, StringEncoding::UTF8)?;

    // Link - order matters! Dependencies must come before dependents
    let linker = wit_component::Linker::default()
        .validate(true)
        .use_built_in_libdl(true)
        // WASI emulation libs
        .library("libwasi-emulated-process-clocks.so", &wasi_clocks, false)?
        .library("libwasi-emulated-signal.so", &wasi_signal, false)?
        .library("libwasi-emulated-mman.so", &wasi_mman, false)?
        .library("libwasi-emulated-getpid.so", &wasi_getpid, false)?
        // C/C++ runtime
        .library("libc.so", &libc, false)?
        .library("libc++abi.so", &libcxxabi, false)?
        .library("libc++.so", &libcxx, false)?
        // Python
        .library("libpython3.14.so", &libpython, false)?
        // Our runtime and bindings
        .library("libmcpone_python_runtime.so", &runtime, false)?
        .library("libmcpone_python_bindings.so", &bindings, false)?
        // WASI adapter
        .adapter("wasi_snapshot_preview1", &adapter)?;

    Ok(linker.encode()?)
}

/// Build, instantiate, and wire up the Python runtime component, returning the store and
/// the typed `execute` function. Shared by the integration tests so each can run probes
/// against a fresh runtime without duplicating the (sizable) host-import wiring.
async fn setup_execute() -> Result<
    (
        Store<State>,
        wasmtime::component::Instance,
        wasmtime::component::TypedFunc<(String,), (Result<ExecuteOutput, String>,)>,
    ),
    Box<dyn std::error::Error>,
> {
    println!("Building component...");
    let component_bytes = build_component()?;
    println!("Component size: {} bytes", component_bytes.len());

    // Create engine with async and component model support
    let mut config = Config::new();
    config.async_support(true);
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);

    let engine = Engine::new(&config)?;

    println!("Loading component into wasmtime...");
    let component = Component::new(&engine, &component_bytes)?;

    // Set up paths for Python stdlib
    // Check MCPONE_PYTHON_STDLIB env var first (used in CI)
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

    if !stdlib_path.exists() {
        panic!(
            "Python stdlib not found at {}. Set MCPONE_PYTHON_STDLIB or extract python-lib.tar.zst first.",
            stdlib_path.display()
        );
    }

    // Create site-packages/workdir if they don't exist
    std::fs::create_dir_all(&site_packages_path)?;
    std::fs::create_dir_all(&workdir_path)?;

    // Create WASI context with preopened directories for Python
    let wasi = WasiCtxBuilder::new()
        .inherit_stdio()
        .env("LANG", "C")
        .env("LC_ALL", "C")
        // Set PYTHONPATH so Python can find stdlib during initialization
        .env("PYTHONPATH", "/python-stdlib:/site-packages")
        // Mount stdlib at /python-stdlib (matches what initialize_python expects)
        .preopened_dir(
            &stdlib_path,
            "/python-stdlib",
            wasmtime_wasi::DirPerms::READ,
            wasmtime_wasi::FilePerms::READ,
        )?
        // Mount site-packages
        .preopened_dir(
            &site_packages_path,
            "/site-packages",
            wasmtime_wasi::DirPerms::READ,
            wasmtime_wasi::FilePerms::READ,
        )?
        // Writable workspace used by subprocess tests.
        .preopened_dir(
            &workdir_path,
            "/workspace",
            wasmtime_wasi::DirPerms::READ | wasmtime_wasi::DirPerms::MUTATE,
            wasmtime_wasi::FilePerms::READ | wasmtime_wasi::FilePerms::WRITE,
        )?
        .build();

    let state = State {
        ctx: wasi,
        table: ResourceTable::new(),
        processes: Arc::new(Mutex::new(FakeProcessTable::default())),
        workspace_host: workdir_path,
    };

    let mut store = Store::new(&engine, state);

    // Create linker and add WASI
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;

    // Add stub implementations for sandbox imports
    use wasmtime::component::{Accessor, Val};

    // invoke: async func(name: string, arguments-json: string) -> result<string, string>
    // This implementation handles a few test callbacks:
    // - "get_time": returns a fixed timestamp as JSON
    // - "echo": echoes back the arguments
    // - "add": adds two numbers
    linker.root().func_wrap_concurrent(
        "invoke",
        |_accessor: &Accessor<State>, (name, args): (String, String)| {
            Box::pin(async move {
                let result: Result<String, String> = match name.as_str() {
                    "get_time" => Ok(r#"{"timestamp": 1234567890}"#.to_string()),
                    "echo" => {
                        // Echo back the arguments
                        Ok(args)
                    }
                    "add" => {
                        // Simple JSON parsing for {"a": x, "b": y}
                        // Extract numbers using basic string operations
                        let extract_num = |key: &str| -> i64 {
                            args.find(&format!("\"{key}\":"))
                                .and_then(|pos| {
                                    let start = pos + key.len() + 3; // skip "key":
                                    let rest = &args[start..];
                                    let rest = rest.trim_start();
                                    let end = rest
                                        .find(|c: char| !c.is_ascii_digit() && c != '-')
                                        .unwrap_or(rest.len());
                                    rest[..end].parse().ok()
                                })
                                .unwrap_or(0)
                        };
                        let a = extract_num("a");
                        let b = extract_num("b");
                        Ok(format!(r#"{{"result": {}}}"#, a + b))
                    }
                    "http.get" => {
                        // Extract URL from {"url": "..."}
                        let url = args
                            .find("\"url\":")
                            .and_then(|pos| {
                                let rest = &args[pos + 6..];
                                let rest = rest.trim_start();
                                if rest.starts_with('"') {
                                    let rest = &rest[1..];
                                    rest.find('"').map(|end| &rest[..end])
                                } else {
                                    None
                                }
                            })
                            .unwrap_or("unknown");
                        Ok(format!(
                            r#"{{"status": 200, "url": "{url}", "body": "test response"}}"#
                        ))
                    }
                    _ => Err(format!("unknown callback: {name}")),
                };
                Ok((result,))
            })
        },
    )?;

    // list-callbacks: func() -> list<callback-info>
    // Returns a list of available callbacks for testing
    linker.root().func_new(
        "list-callbacks",
        |_ctx: wasmtime::StoreContextMut<'_, State>,
         _func_ty: wasmtime::component::types::ComponentFunc,
         _params: &[Val],
         results: &mut [Val]| {
            // Create callback info records
            let callbacks = vec![
                // Simple callback
                Val::Record(vec![
                    ("name".to_string(), Val::String("get_time".to_string())),
                    (
                        "description".to_string(),
                        Val::String("Get current timestamp".to_string()),
                    ),
                    (
                        "parameters-schema-json".to_string(),
                        Val::String("{}".to_string()),
                    ),
                ]),
                // Echo callback
                Val::Record(vec![
                    ("name".to_string(), Val::String("echo".to_string())),
                    (
                        "description".to_string(),
                        Val::String("Echo back arguments".to_string()),
                    ),
                    (
                        "parameters-schema-json".to_string(),
                        Val::String("{}".to_string()),
                    ),
                ]),
                // Add callback
                Val::Record(vec![
                    ("name".to_string(), Val::String("add".to_string())),
                    (
                        "description".to_string(),
                        Val::String("Add two numbers".to_string()),
                    ),
                    (
                        "parameters-schema-json".to_string(),
                        Val::String(r#"{"a": "number", "b": "number"}"#.to_string()),
                    ),
                ]),
                // Dotted namespace callback
                Val::Record(vec![
                    ("name".to_string(), Val::String("http.get".to_string())),
                    (
                        "description".to_string(),
                        Val::String("HTTP GET request".to_string()),
                    ),
                    (
                        "parameters-schema-json".to_string(),
                        Val::String(r#"{"url": "string"}"#.to_string()),
                    ),
                ]),
            ];

            results[0] = Val::List(callbacks);
            Ok(())
        },
    )?;

    // report-trace: func(lineno: u32, event-json: string, context-json: string)
    linker.root().func_new(
        "report-trace",
        |_ctx: wasmtime::StoreContextMut<'_, State>,
         _func_ty: wasmtime::component::types::ComponentFunc,
         _params: &[Val],
         _results: &mut [Val]| { Ok(()) },
    )?;

    // report-output: func(stream: u32, data: string)
    linker.root().func_new(
        "report-output",
        |_ctx: wasmtime::StoreContextMut<'_, State>,
         _func_ty: wasmtime::component::types::ComponentFunc,
         _params: &[Val],
         _results: &mut [Val]| { Ok(()) },
    )?;

    // Add network stub implementations (TCP and TLS)
    // These return "not-permitted" errors since networking is not available in this test.
    add_network_stubs(&mut linker)?;

    // Add process stub implementations for subprocess tests.
    add_process_stubs(&mut linker)?;

    println!("Instantiating component...");
    let instance = linker.instantiate_async(&mut store, &component).await?;

    println!("SUCCESS! Component instantiated");

    // Get the execute function
    // execute: async func(code: string) -> result<execute-output, string>
    println!("Looking for execute function...");

    let execute = instance
        .get_typed_func::<(String,), (Result<ExecuteOutput, String>,)>(&mut store, "[async]execute")
        .or_else(|_| {
            instance.get_typed_func::<(String,), (Result<ExecuteOutput, String>,)>(
                &mut store, "execute",
            )
        })?;

    Ok((store, instance, execute))
}

#[tokio::test]
async fn test_default_text_encoding() -> Result<(), Box<dyn std::error::Error>> {
    let (mut store, _instance, execute) = setup_execute().await?;
    let code = r#"
import codecs
import locale
import sys
import tempfile
from pathlib import Path

assert sys.flags.utf8_mode == 1, sys.flags
# getencoding() reports libc's locale even in UTF-8 mode; the I/O default must
# be UTF-8 independently of that locale.
assert codecs.lookup(locale.getpreferredencoding(False)).name == 'utf-8'
text = 'café — 日本語 🐍\n'
with tempfile.TemporaryDirectory(dir='/workspace') as directory:
    path = Path(directory) / 'text.txt'
    for change_locale in (False, True):
        if change_locale:
            locale.setlocale(locale.LC_CTYPE, 'C')
        path.write_bytes(text.encode('utf-8'))
        assert path.read_text() == text
        with open(path) as source:
            assert source.read() == text
        path.write_text(text)
        assert path.read_bytes() == text.encode('utf-8')
        with open(path, 'w') as target:
            target.write(text)
        assert path.read_bytes() == text.encode('utf-8')

    def read_ascii_open():
        with open(path, encoding='ascii') as source:
            return source.read()

    for read in (lambda: path.read_text(encoding='ascii'), read_ascii_open):
        try:
            read()
        except UnicodeDecodeError:
            pass
        else:
            raise AssertionError('explicit ASCII read accepted non-ASCII text')
    for use_path in (False, True):
        try:
            if use_path:
                path.write_text(text, encoding='ascii')
            else:
                with open(path, 'w', encoding='ascii') as target:
                    target.write(text)
        except UnicodeEncodeError:
            pass
        else:
            raise AssertionError('explicit ASCII write accepted non-ASCII text')
    path.write_text('ASCII\n', encoding='ascii')
    with open(path, encoding='ascii') as source:
        assert source.read() == 'ASCII\n'
print('UTF-8 defaults and explicit ASCII passed')
"#;
    let (result,) = execute.call_async(&mut store, (code.to_string(),)).await?;
    execute.post_return_async(&mut store).await?;
    let output = result.expect("default and explicit encodings should work in the C locale");
    assert_eq!(
        output.stdout.trim(),
        "UTF-8 defaults and explicit ASCII passed"
    );
    assert!(output.stderr.is_empty(), "{}", output.stderr);
    Ok(())
}

#[tokio::test]
async fn test_instantiate_component() -> Result<(), Box<dyn std::error::Error>> {
    let (mut store, instance, execute) = setup_execute().await?;

    // Test 1: Simple print statement
    println!("Test 1: execute('print(1+1)')...");
    let (result,) = execute
        .call_async(&mut store, ("print(1+1)".to_string(),))
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert_eq!(output.stdout.trim(), "2", "print(1+1) should output '2'");
        }
        Err(error) => {
            panic!("Test 1 failed with error: {error}");
        }
    }

    // Test 2: Multiple prints
    println!("Test 2: Multiple print statements...");
    let (result,) = execute
        .call_async(&mut store, ("print('hello')\nprint('world')".to_string(),))
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert_eq!(
                output.stdout, "hello\nworld",
                "Should have two lines of output"
            );
        }
        Err(error) => {
            panic!("Test 2 failed with error: {error}");
        }
    }

    // Test 3: Variable assignment (no output)
    println!("Test 3: Variable assignment with no output...");
    let (result,) = execute
        .call_async(&mut store, ("x = 42".to_string(),))
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert_eq!(output.stdout, "", "Assignment should produce no output");
        }
        Err(error) => {
            panic!("Test 3 failed with error: {error}");
        }
    }

    // Test 4: Syntax error should return Err
    println!("Test 4: Syntax error...");
    let (result,) = execute
        .call_async(&mut store, ("def broken(".to_string(),))
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            panic!("Test 4 should have failed, but got: {output:?}");
        }
        Err(error) => {
            println!("  Expected error: {error}");
            assert!(
                error.contains("SyntaxError") || error.contains("syntax"),
                "Error should mention syntax: {error}"
            );
        }
    }

    // Test 5: Runtime error should return Err
    println!("Test 5: Runtime error (NameError)...");
    let (result,) = execute
        .call_async(&mut store, ("print(undefined_variable)".to_string(),))
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            panic!("Test 5 should have failed, but got: {output:?}");
        }
        Err(error) => {
            println!("  Expected error: {error}");
            assert!(
                error.contains("NameError") || error.contains("undefined"),
                "Error should mention NameError: {error}"
            );
        }
    }

    // Test 6: State persists between calls
    println!("Test 6: State persistence...");
    let (result,) = execute
        .call_async(&mut store, ("my_var = 'persisted'".to_string(),))
        .await?;
    execute.post_return_async(&mut store).await?;
    match &result {
        Ok(_) => println!("  Assignment OK"),
        Err(e) => panic!("Assignment failed with: {e}"),
    }

    let (result,) = execute
        .call_async(&mut store, ("print(my_var)".to_string(),))
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert_eq!(
                output.stdout.trim(),
                "persisted",
                "Variable should persist between calls"
            );
        }
        Err(error) => {
            panic!("Test 6 failed with error: {error}");
        }
    }

    // Test 7: Import stdlib module
    println!("Test 7: Import stdlib (math)...");
    let (result,) = execute
        .call_async(&mut store, ("import math; print(math.pi)".to_string(),))
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert!(
                output.stdout.starts_with("3.14"),
                "math.pi should start with 3.14: {output:?}"
            );
        }
        Err(error) => {
            panic!("Test 7 failed with error: {error}");
        }
    }

    // =========================================================================
    // State Management Tests
    // =========================================================================

    // Get the state management functions
    let snapshot_state = instance
        .get_typed_func::<(), (Result<Vec<u8>, String>,)>(&mut store, "[async]snapshot-state")
        .or_else(|_| {
            instance.get_typed_func::<(), (Result<Vec<u8>, String>,)>(&mut store, "snapshot-state")
        })?;

    let restore_state = instance
        .get_typed_func::<(Vec<u8>,), (Result<(), String>,)>(&mut store, "[async]restore-state")
        .or_else(|_| {
            instance
                .get_typed_func::<(Vec<u8>,), (Result<(), String>,)>(&mut store, "restore-state")
        })?;

    let clear_state = instance
        .get_typed_func::<(), ()>(&mut store, "[async]clear-state")
        .or_else(|_| instance.get_typed_func::<(), ()>(&mut store, "clear-state"))?;

    // Test 8: Snapshot state
    println!("Test 8: Snapshot state...");
    // First set some variables
    let (result,) = execute
        .call_async(
            &mut store,
            ("x = 42\ny = 'hello'\nz = [1, 2, 3]".to_string(),),
        )
        .await?;
    execute.post_return_async(&mut store).await?;
    assert!(result.is_ok(), "Setting variables should succeed");

    // Take a snapshot
    let (snapshot_result,) = snapshot_state.call_async(&mut store, ()).await?;
    snapshot_state.post_return_async(&mut store).await?;

    let snapshot_data = match snapshot_result {
        Ok(data) => {
            println!("  OK: Snapshot taken, {} bytes", data.len());
            assert!(!data.is_empty(), "Snapshot should not be empty");
            data
        }
        Err(error) => {
            panic!("Test 8 failed: {error}");
        }
    };

    // Test 9: Clear state
    println!("Test 9: Clear state...");
    clear_state.call_async(&mut store, ()).await?;
    clear_state.post_return_async(&mut store).await?;

    // Verify variables are gone
    let (result,) = execute
        .call_async(&mut store, ("print(x)".to_string(),))
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            panic!("Test 9 should have failed (x should be cleared), but got: {output:?}");
        }
        Err(error) => {
            println!("  OK: Variable x is cleared (error: {error})");
            assert!(
                error.contains("NameError") || error.contains("not defined"),
                "Error should indicate x is not defined: {error}"
            );
        }
    }

    // Test 10: Restore state
    println!("Test 10: Restore state...");
    let (restore_result,) = restore_state
        .call_async(&mut store, (snapshot_data,))
        .await?;
    restore_state.post_return_async(&mut store).await?;

    match &restore_result {
        Ok(()) => {
            println!("  OK: State restored");
        }
        Err(error) => {
            panic!("Test 10 failed: {error}");
        }
    }

    // Verify variables are restored
    let (result,) = execute
        .call_async(&mut store, ("print(x, y, z)".to_string(),))
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: Restored values: {output:?}");
            assert!(
                output.stdout.contains("42")
                    && output.stdout.contains("hello")
                    && output.stdout.contains("[1, 2, 3]"),
                "Restored values should match: {output:?}"
            );
        }
        Err(error) => {
            panic!("Test 10 verification failed: {error}");
        }
    }

    // =========================================================================
    // Callback Tests
    // =========================================================================
    // These tests verify that Python can call host functions via invoke()
    // and the callback wrapper functions.

    // Test 11: list_callbacks() returns available callbacks
    println!("Test 11: list_callbacks()...");
    let (result,) = execute
        .call_async(
            &mut store,
            (r#"
cbs = list_callbacks()
print(f"list_callbacks returned: {type(cbs).__name__}")
print(f"count: {len(cbs)}")
"#
            .to_string(),),
        )
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert!(
                output.stdout.contains("list_callbacks returned: list"),
                "list_callbacks should return a list: {output:?}"
            );
        }
        Err(error) => {
            panic!("Test 11 failed: {error}");
        }
    }

    // Test 12: invoke() calls host callback and returns result
    println!("Test 12: invoke() calls get_time callback...");
    let (result,) = execute
        .call_async(
            &mut store,
            (r#"
result = await invoke("get_time")
print(f"result type: {type(result).__name__}")
print(f"timestamp: {result.get('timestamp', 'missing')}")
"#
            .to_string(),),
        )
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert!(
                output.stdout.contains("result type: dict"),
                "invoke should return a dict: {output:?}"
            );
            assert!(
                output.stdout.contains("timestamp: 1234567890"),
                "invoke should return correct timestamp: {output:?}"
            );
        }
        Err(error) => {
            panic!("Test 12 failed: {error}");
        }
    }

    // Test 13: Verify list_callbacks() still works (introspection is available)
    println!("Test 13: list_callbacks() provides callback info...");
    let (result,) = execute
        .call_async(
            &mut store,
            (r#"
cbs = list_callbacks()
names = [cb['name'] for cb in cbs]
print(f"callbacks: {names}")
"#
            .to_string(),),
        )
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert!(
                output.stdout.contains("get_time"),
                "Should list get_time callback: {output:?}"
            );
        }
        Err(error) => {
            panic!("Test 13 failed: {error}");
        }
    }

    // Test 14: invoke() with arguments (add callback)
    println!("Test 14: invoke() with arguments...");
    let (result,) = execute
        .call_async(
            &mut store,
            (r#"
result = await invoke("add", a=10, b=32)
print(f"add result: {result.get('result', 'missing')}")
"#
            .to_string(),),
        )
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert!(
                output.stdout.contains("add result: 42"),
                "invoke('add', a=10, b=32) should return 42: {output:?}"
            );
        }
        Err(error) => {
            panic!("Test 14 failed: {error}");
        }
    }

    // Test 15: Callback wrappers work (http.get via namespace)
    println!("Test 15: Namespace callback (http.get)...");
    let (result,) = execute
        .call_async(
            &mut store,
            (r#"
result = await http.get(url="https://example.com")
print(f"status: {result.get('status', 'missing')}")
print(f"url: {result.get('url', 'missing')}")
"#
            .to_string(),),
        )
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert!(
                output.stdout.contains("status: 200"),
                "http.get should return status 200: {output:?}"
            );
            assert!(
                output.stdout.contains("url: https://example.com"),
                "http.get should return correct url: {output:?}"
            );
        }
        Err(error) => {
            panic!("Test 15 failed: {error}");
        }
    }

    // Test 16: Error handling for unknown callback
    println!("Test 16: invoke() error handling for unknown callback...");
    let (result,) = execute
        .call_async(
            &mut store,
            (r#"
try:
    await invoke("nonexistent_callback")
    print("ERROR: should have raised")
except RuntimeError as e:
    print(f"OK: RuntimeError raised")
    if "unknown callback" in str(e):
        print("CORRECT: error message")
"#
            .to_string(),),
        )
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert!(
                output.stdout.contains("RuntimeError raised"),
                "invoke('nonexistent') should raise RuntimeError: {output:?}"
            );
        }
        Err(error) => {
            panic!("Test 16 failed: {error}");
        }
    }

    // Test 17: stderr capture works
    println!("Test 17: stderr capture...");
    let (result,) = execute
        .call_async(
            &mut store,
            (r#"
import sys
print("to stdout")
print("to stderr", file=sys.stderr)
"#
            .to_string(),),
        )
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  stdout: {:?}", output.stdout);
            println!("  stderr: {:?}", output.stderr);
            assert_eq!(
                output.stdout.trim(),
                "to stdout",
                "stdout should capture print()"
            );
            assert_eq!(
                output.stderr.trim(),
                "to stderr",
                "stderr should capture print(..., file=sys.stderr)"
            );
        }
        Err(error) => {
            panic!("Test 17 failed: {error}");
        }
    }

    // Test 18: subprocess.run capture_output + text mode
    println!("Test 18: subprocess.run(capture_output=True, text=True)...");
    let (result,) = execute
        .call_async(
            &mut store,
            (r#"
import subprocess
WORKDIR = "/workspace"
res = subprocess.run(["mcpone-test-echo", "hello", "world"], capture_output=True, text=True, cwd=WORKDIR)
print(f"rc={res.returncode}")
print(f"stdout={res.stdout.strip()}")
print(f"stderr={res.stderr!r}")
"#
            .to_string(),),
        )
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert!(
                output.stdout.contains("rc=0"),
                "subprocess rc should be 0: {output:?}"
            );
            assert!(
                output.stdout.contains("stdout=hello world"),
                "subprocess stdout should be captured: {output:?}"
            );
            assert!(
                output.stdout.contains("stderr=''"),
                "subprocess stderr should be empty string: {output:?}"
            );
        }
        Err(error) => {
            panic!("Test 18 failed: {error}");
        }
    }

    // Test 19: subprocess.run with stdin input and check_output/check=True behavior
    println!("Test 19: subprocess input + check=True behavior...");
    let (result,) = execute
        .call_async(
            &mut store,
            (r#"
import subprocess
WORKDIR = "/workspace"
cat = subprocess.run(["mcpone-test-cat"], input="abc123", text=True, capture_output=True, cwd=WORKDIR)
print(f"cat={cat.stdout}")
try:
    subprocess.run(["mcpone-test-fail", "7"], check=True, capture_output=True, text=True, cwd=WORKDIR)
    print("check_failed=False")
except subprocess.CalledProcessError as e:
    print(f"check_failed=True rc={e.returncode}")
"#
            .to_string(),),
        )
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert!(
                output.stdout.contains("cat=abc123"),
                "stdin input should be passed to child: {output:?}"
            );
            assert!(
                output.stdout.contains("check_failed=True rc=7"),
                "check=True should raise CalledProcessError: {output:?}"
            );
        }
        Err(error) => {
            panic!("Test 19 failed: {error}");
        }
    }

    // Test 20: stderr=STDOUT merge behavior
    println!("Test 20: subprocess stderr=STDOUT merge...");
    let (result,) = execute
        .call_async(
            &mut store,
            (r#"
import subprocess
WORKDIR = "/workspace"
res = subprocess.run(["mcpone-test-stderr"], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, cwd=WORKDIR)
print(f"merged={res.stdout.strip()}")
print(f"stderr_is_none={res.stderr is None}")
"#
            .to_string(),),
        )
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert!(
                output.stdout.contains("merged=from-stderr"),
                "stderr should be merged into stdout: {output:?}"
            );
            assert!(
                output.stdout.contains("stderr_is_none=True"),
                "stderr should be None when redirected to STDOUT: {output:?}"
            );
        }
        Err(error) => {
            panic!("Test 20 failed: {error}");
        }
    }

    println!("\nAll tests passed!");
    Ok(())
}

/// The writer monocp swaps in as sys.stdout/sys.stderr is captured/streamed, never a TTY.
/// It must present a correct, non-interactive file-like interface so that pip/rich and common
/// CLI libraries degrade naturally (e.g. isatty() -> False) instead of raising AttributeError.
#[tokio::test]
async fn test_streaming_writer_interface() -> Result<(), Box<dyn std::error::Error>> {
    let (mut store, _instance, execute) = setup_execute().await?;

    // The file-like contract _McponeStreamingWriter must satisfy.
    println!("streaming writer: file-like interface...");
    let (result,) = execute
        .call_async(
            &mut store,
            (r#"
import sys, io
errs = []
def check(cond, name):
    if not cond:
        errs.append(name)
# Non-interactive: isatty must return False and never raise (pip spinners.py:138 calls it directly).
check(sys.stdout.isatty() is False, "stdout.isatty!=False")
check(sys.stderr.isatty() is False, "stderr.isatty!=False")
# Idiomatic text stream: a real sys.stdout is an io.TextIOBase.
check(isinstance(sys.stdout, io.TextIOBase), "not-TextIOBase")
# Capability queries.
check(sys.stdout.writable() is True, "writable!=True")
check(sys.stdout.readable() is False, "readable!=False")
check(sys.stdout.seekable() is False, "seekable!=False")
check(sys.stdout.closed is False, "closed!=False")
# Text attributes consumers read (rich getattr(file,'encoding'); click reads encoding/errors).
check(sys.stdout.encoding == "utf-8", "encoding!=utf-8")
check(sys.stdout.errors == "strict", "errors!=strict")
# No real fd: fileno() must raise io.UnsupportedOperation (which is both OSError and ValueError).
try:
    sys.stdout.fileno()
    errs.append("fileno-did-not-raise")
except io.UnsupportedOperation as e:
    check(isinstance(e, OSError) and isinstance(e, ValueError), "fileno-exc-mro")
except Exception as e:
    errs.append("fileno-raised-" + type(e).__name__)
# reconfigure() is a no-op that accepts kwargs (user scripts call sys.stdout.reconfigure(encoding=...)).
sys.stdout.reconfigure(encoding="utf-8")
sys.stdout.reconfigure()
sys.stderr.reconfigure(encoding="utf-8", errors="strict")
# write() returns the character count and streams to the host.
check(sys.stdout.write("XY") == 2, "write-len")
# The singleton is reused across executions: close() must be a no-op that leaves it usable.
sys.stdout.close()
check(sys.stdout.closed is False, "closed-after-close")
check(sys.stdout.write("Z") == 1, "write-after-close")
print("WRITER_OK" if not errs else "WRITER_FAIL:" + ",".join(errs))
"#
            .to_string(),),
        )
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert!(
                output.stdout.contains("WRITER_OK"),
                "streaming writer interface incomplete: {output:?}"
            );
        }
        Err(error) => {
            panic!("writer interface probe failed: {error}");
        }
    }

    // The persistent writer singleton keeps capturing output across execute() calls even after
    // a prior call invoked close() on it (regression guard for the no-op close()).
    println!("streaming writer: reuse after close across execute() calls...");
    let (result,) = execute
        .call_async(&mut store, ("print('REUSE_OK')".to_string(),))
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert_eq!(
                output.stdout.trim(),
                "REUSE_OK",
                "writer should still capture output after a prior close(): {output:?}"
            );
        }
        Err(error) => {
            panic!("writer reuse probe failed: {error}");
        }
    }

    Ok(())
}

/// socket.makefile() must provide a buffered binary reader by default while
/// retaining SocketIO's one-read semantics for explicit buffering=0 callers.
#[tokio::test]
async fn test_socket_io_buffers_short_reads_for_http_client()
-> Result<(), Box<dyn std::error::Error>> {
    let (mut store, _instance, execute) = setup_execute().await?;

    let (result,) = execute
        .call_async(
            &mut store,
            (r#"
import http.client
import socket

class FakeSocket:
    def __init__(self, data, cap=None, limits=None):
        self._data = bytearray(data)
        self._cap = cap
        self._limits = list(limits or [])
        self.calls = []

    def recv(self, size):
        self.calls.append(size)
        if not self._data:
            return b''
        count = min(size, len(self._data))
        if size > 1 and self._limits:
            count = min(count, self._limits.pop(0))
        elif self._cap is not None:
            count = min(count, self._cap)
        chunk = bytes(self._data[:count])
        del self._data[:count]
        return chunk

    def makefile(self, mode='rb', buffering=-1, **kwargs):
        # Exercise the shim's real socket.makefile implementation rather than
        # recreating its buffering behavior in this fake.
        return socket.socket.makefile(self, mode, buffering, **kwargs)

body = b'x' * 759
wire = b'HTTP/1.1 200 OK\r\nContent-Length: 759\r\n\r\n' + body
http_socket = FakeSocket(wire, limits=[245, 514])
response = http.client.HTTPResponse(http_socket)
response.begin()
assert response.read() == body
assert http_socket._limits == [], http_socket.calls

# The default binary makefile buffers a short raw read, including readinto.
readinto_socket = FakeSocket(b'abcdef', cap=2)
buffer = bytearray(6)
assert socket.socket.makefile(readinto_socket, 'rb').readinto(buffer) == 6
assert bytes(buffer) == b'abcdef'
assert len(readinto_socket.calls) > 1

# Explicit unbuffered binary mode preserves the raw one-recv contract.
unbuffered_socket = FakeSocket(b'abcdef', cap=2)
unbuffered = socket.socket.makefile(unbuffered_socket, 'rb', buffering=0)
assert isinstance(unbuffered, socket.SocketIO)
assert unbuffered.read(6) == b'ab'
assert unbuffered_socket.calls == [6]

# Zero-size reads must not probe the transport, and read(-1) must still stop
# only at an actual empty read.
zero_socket = FakeSocket(b'z', cap=1)
zero_file = socket.SocketIO(zero_socket, 'rb')
assert zero_file.read(0) == b''
assert zero_file.readline(0) == b''
assert zero_socket.calls == []
zero_file.close()
try:
    zero_file.readline()
except ValueError:
    pass
else:
    raise AssertionError('closed readline did not raise ValueError')
all_socket = FakeSocket(b'abcdef', cap=2)
assert socket.SocketIO(all_socket, 'rb').read() == b'abcdef'

# A true EOF after a partial declared body must remain an IncompleteRead.
truncated_socket = FakeSocket(
    b'HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nabc', cap=3
)
truncated = http.client.HTTPResponse(truncated_socket)
truncated.begin()
try:
    truncated.read()
except http.client.IncompleteRead as exc:
    assert exc.partial == b'abc'
    assert exc.expected == 1
else:
    raise AssertionError('expected IncompleteRead for genuine EOF')

class PartialErrorSocket(FakeSocket):
    def recv(self, size):
        if self._data:
            return super().recv(size)
        raise socket.error('transport error')

partial_error_socket = PartialErrorSocket(
    b'HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nabc', cap=3
)
partial_error = http.client.HTTPResponse(partial_error_socket)
partial_error.begin()
try:
    partial_error.read()
except socket.error:
    pass
else:
    raise AssertionError('partial transport error was treated as EOF')

print('SOCKET_IO_BUFFERING_OK')
"#
            .to_string(),),
        )
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            assert_eq!(
                output.stdout.trim(),
                "SOCKET_IO_BUFFERING_OK",
                "SocketIO short-read regression: {output:?}"
            );
        }
        Err(error) => panic!("SocketIO short-read probe failed: {error}"),
    }

    Ok(())
}

/// TCP error type for test stubs.
#[derive(
    wasmtime::component::ComponentType, wasmtime::component::Lift, wasmtime::component::Lower, Clone,
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

/// TLS error type for test stubs.
#[derive(
    wasmtime::component::ComponentType, wasmtime::component::Lift, wasmtime::component::Lower, Clone,
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

/// Add stub implementations for network imports (TCP and TLS).
/// These return "not-permitted" errors since networking is not available in this test.
fn add_network_stubs(linker: &mut Linker<State>) -> Result<(), Box<dyn std::error::Error>> {
    // TCP interface stubs
    let mut tcp = linker.instance("mcpone:host-sockets/tcp@0.1.0")?;

    // tcp.connect: func(host: string, port: u16) -> result<tcp-handle, tcp-error>
    tcp.func_wrap_async(
        "connect",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_host, _port): (String, u16)| {
            Box::new(async move {
                Ok((Result::<u32, TcpError>::Err(TcpError::NotPermitted(
                    "networking not available in test".into(),
                )),))
            })
        },
    )?;

    // tcp.read: func(handle: tcp-handle, len: u32) -> result<list<u8>, tcp-error>
    tcp.func_wrap_async(
        "read",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_handle, _len): (u32, u32)| {
            Box::new(async move {
                Ok((Result::<Vec<u8>, TcpError>::Err(TcpError::NotPermitted(
                    "networking not available in test".into(),
                )),))
            })
        },
    )?;

    // tcp.write: func(handle: tcp-handle, data: list<u8>) -> result<u32, tcp-error>
    tcp.func_wrap_async(
        "write",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_handle, _data): (u32, Vec<u8>)| {
            Box::new(async move {
                Ok((Result::<u32, TcpError>::Err(TcpError::NotPermitted(
                    "networking not available in test".into(),
                )),))
            })
        },
    )?;

    // tcp.close: func(handle: tcp-handle)
    tcp.func_wrap(
        "close",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_handle,): (u32,)| Ok(()),
    )?;

    // TLS interface stubs
    let mut tls = linker.instance("mcpone:host-sockets/tls@0.1.0")?;

    // tls.upgrade: func(tcp: tcp-handle, hostname: string) -> result<tls-handle, tls-error>
    tls.func_wrap_async(
        "upgrade",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_tcp_handle, _hostname): (u32, String)| {
            Box::new(async move {
                Ok((Result::<u32, TlsError>::Err(TlsError::HandshakeFailed(
                    "networking not available in test".into(),
                )),))
            })
        },
    )?;

    // tls.read: func(handle: tls-handle, len: u32) -> result<list<u8>, tls-error>
    tls.func_wrap_async(
        "read",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_handle, _len): (u32, u32)| {
            Box::new(async move {
                Ok((Result::<Vec<u8>, TlsError>::Err(TlsError::HandshakeFailed(
                    "networking not available in test".into(),
                )),))
            })
        },
    )?;

    // tls.write: func(handle: tls-handle, data: list<u8>) -> result<u32, tls-error>
    tls.func_wrap_async(
        "write",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_handle, _data): (u32, Vec<u8>)| {
            Box::new(async move {
                Ok((Result::<u32, TlsError>::Err(TlsError::HandshakeFailed(
                    "networking not available in test".into(),
                )),))
            })
        },
    )?;

    // tls.close: func(handle: tls-handle)
    tls.func_wrap(
        "close",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (_handle,): (u32,)| Ok(()),
    )?;

    Ok(())
}

#[derive(
    wasmtime::component::ComponentType,
    wasmtime::component::Lift,
    wasmtime::component::Lower,
    Clone,
    Debug,
)]
#[component(record)]
struct PipeHandles {
    #[component(name = "read")]
    read: u32,
    #[component(name = "write")]
    write: u32,
}

#[derive(
    wasmtime::component::ComponentType,
    wasmtime::component::Lift,
    wasmtime::component::Lower,
    Clone,
    Debug,
)]
#[component(record)]
struct FileWriteTarget {
    #[component(name = "path")]
    path: String,
    #[component(name = "append")]
    append: bool,
}

#[derive(
    wasmtime::component::ComponentType,
    wasmtime::component::Lift,
    wasmtime::component::Lower,
    Clone,
    Debug,
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
    wasmtime::component::ComponentType,
    wasmtime::component::Lift,
    wasmtime::component::Lower,
    Clone,
    Debug,
)]
#[component(record)]
struct FdBinding {
    #[component(name = "fd")]
    fd: u32,
    #[component(name = "target")]
    target: FdTarget,
}

fn resolve_child_path(cwd: &str, path: &str, workspace_host: &Path) -> PathBuf {
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

fn write_to_target(
    target: &FdTarget,
    cwd: &str,
    workspace_host: &Path,
    data: &[u8],
) -> Result<(), String> {
    match target {
        FdTarget::Inherit | FdTarget::Close | FdTarget::PipeRead(_) | FdTarget::PipeWrite(_) => {
            Ok(())
        }
        FdTarget::Dup(_) => Ok(()),
        FdTarget::FileRead(_) => Ok(()),
        FdTarget::FileWrite(file_target) => {
            let path = resolve_child_path(cwd, &file_target.path, workspace_host);
            let mut opts = std::fs::OpenOptions::new();
            opts.create(true).write(true);
            if file_target.append {
                opts.append(true);
            } else {
                opts.truncate(true);
            }
            let mut file = opts
                .open(&path)
                .map_err(|e| format!("failed to open {}: {e}", path.display()))?;
            std::io::Write::write_all(&mut file, data)
                .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
            Ok(())
        }
    }
}

fn read_stdin_from_target(
    target: &FdTarget,
    cwd: &str,
    workspace_host: &Path,
) -> Result<Vec<u8>, String> {
    match target {
        FdTarget::FileRead(path) => {
            let resolved = resolve_child_path(cwd, path, workspace_host);
            std::fs::read(&resolved)
                .map_err(|e| format!("failed to read {}: {e}", resolved.display()))
        }
        _ => Ok(Vec::new()),
    }
}

fn fd_target_for<'a>(bindings: &'a [FdBinding], fd: u32) -> Option<&'a FdTarget> {
    bindings.iter().find(|b| b.fd == fd).map(|b| &b.target)
}

fn simulate_test_command(
    argv: &[String],
    cwd: &str,
    workspace_host: &Path,
    bindings: &[FdBinding],
) -> Result<i32, String> {
    let cmd = argv
        .first()
        .map(String::as_str)
        .ok_or_else(|| "missing argv[0]".to_string())?;

    let mut stdout_data = Vec::new();
    let mut stderr_data = Vec::new();
    let mut exit_code = 0_i32;

    match cmd {
        "mcpone-test-echo" => {
            stdout_data = format!("{}\n", argv[1..].join(" ")).into_bytes();
        }
        "mcpone-test-cat" => {
            let stdin_target = fd_target_for(bindings, 0).unwrap_or(&FdTarget::Inherit);
            stdout_data = read_stdin_from_target(stdin_target, cwd, workspace_host)?;
        }
        "mcpone-test-stderr" => {
            stderr_data = b"from-stderr\n".to_vec();
        }
        "mcpone-test-fail" => {
            exit_code = argv.get(1).and_then(|s| s.parse::<i32>().ok()).unwrap_or(1);
            stderr_data = format!("failed with {exit_code}\n").into_bytes();
        }
        _ => {
            return Err(format!("unknown test command: {cmd}"));
        }
    }

    let stdout_target = fd_target_for(bindings, 1).unwrap_or(&FdTarget::Inherit);
    let stderr_target = fd_target_for(bindings, 2).unwrap_or(&FdTarget::Inherit);

    if let FdTarget::Dup(target_fd) = stderr_target {
        if *target_fd == 1 {
            stdout_data.extend_from_slice(&stderr_data);
            stderr_data.clear();
        }
    }

    write_to_target(stdout_target, cwd, workspace_host, &stdout_data)?;
    write_to_target(stderr_target, cwd, workspace_host, &stderr_data)?;

    Ok(exit_code)
}

fn add_process_stubs(linker: &mut Linker<State>) -> Result<(), Box<dyn std::error::Error>> {
    let mut process = linker.instance("monocp:host/monocp@0.2.0")?;

    process.func_wrap(
        "pipe-create",
        |ctx: wasmtime::StoreContextMut<'_, State>, (): ()| {
            let mut state = ctx.data().processes.lock().unwrap();
            let read = state.alloc_pipe_handle();
            let write = state.alloc_pipe_handle();
            state.pipes.insert(read, Vec::new());
            state.pipes.insert(write, Vec::new());
            Ok((PipeHandles { read, write },))
        },
    )?;

    process.func_wrap(
        "pipe-clone",
        |ctx: wasmtime::StoreContextMut<'_, State>, (handle,): (u32,)| {
            let mut state = ctx.data().processes.lock().unwrap();
            let cloned = state.alloc_pipe_handle();
            let data = state.pipes.get(&handle).cloned().unwrap_or_default();
            state.pipes.insert(cloned, data);
            Ok((cloned,))
        },
    )?;

    process.func_wrap(
        "pipe-read",
        |ctx: wasmtime::StoreContextMut<'_, State>, (handle, max_len): (u32, u32)| {
            let mut state = ctx.data().processes.lock().unwrap();
            let data = state.pipes.entry(handle).or_default();
            let n = usize::min(max_len as usize, data.len());
            let out = data.drain(..n).collect::<Vec<u8>>();
            Ok((out,))
        },
    )?;

    process.func_wrap(
        "pipe-write",
        |ctx: wasmtime::StoreContextMut<'_, State>, (handle, data): (u32, Vec<u8>)| {
            let mut state = ctx.data().processes.lock().unwrap();
            state
                .pipes
                .entry(handle)
                .or_default()
                .extend_from_slice(&data);
            Ok((u32::try_from(data.len()).unwrap_or(u32::MAX),))
        },
    )?;

    process.func_wrap(
        "pipe-close",
        |ctx: wasmtime::StoreContextMut<'_, State>, (handle,): (u32,)| {
            let mut state = ctx.data().processes.lock().unwrap();
            state.pipes.remove(&handle);
            Ok(())
        },
    )?;

    process.func_wrap(
        "process-id",
        |_ctx: wasmtime::StoreContextMut<'_, State>, (): ()| Ok((12345_u32,)),
    )?;

    process.func_wrap(
        "spawn-with-context",
        |ctx: wasmtime::StoreContextMut<'_, State>,
         (argv, cwd, fds): (Vec<String>, String, Vec<FdBinding>)| {
            let workspace = ctx.data().workspace_host.clone();
            let exit_code = match simulate_test_command(&argv, &cwd, workspace.as_path(), &fds) {
                Ok(code) => code,
                Err(msg) => return Ok((Result::<u64, String>::Err(msg),)),
            };

            let mut state = ctx.data().processes.lock().unwrap();
            let handle = state.alloc_child_handle();
            state.children.insert(handle, exit_code);
            Ok((Result::<u64, String>::Ok(handle),))
        },
    )?;

    process.func_wrap(
        "wait-child",
        |ctx: wasmtime::StoreContextMut<'_, State>, (child_handle,): (u64,)| {
            let mut state = ctx.data().processes.lock().unwrap();
            let result = match state.children.remove(&child_handle) {
                Some(code) => Result::<i32, String>::Ok(code),
                None => Result::<i32, String>::Err("invalid child handle".into()),
            };
            Ok((result,))
        },
    )?;

    process.func_wrap(
        "try-wait-child",
        |ctx: wasmtime::StoreContextMut<'_, State>, (child_handle,): (u64,)| {
            let state = ctx.data().processes.lock().unwrap();
            Ok((state.children.get(&child_handle).copied(),))
        },
    )?;

    process.func_wrap(
        "close-child",
        |ctx: wasmtime::StoreContextMut<'_, State>, (child_handle,): (u64,)| {
            let mut state = ctx.data().processes.lock().unwrap();
            state.children.remove(&child_handle);
            Ok(())
        },
    )?;

    process.func_wrap(
        "kill-child",
        |ctx: wasmtime::StoreContextMut<'_, State>, (child_handle,): (u64,)| {
            let mut state = ctx.data().processes.lock().unwrap();
            let result = if state.children.remove(&child_handle).is_some() {
                Result::<(), String>::Ok(())
            } else {
                Result::<(), String>::Err("invalid child handle".into())
            };
            Ok((result,))
        },
    )?;

    Ok(())
}
