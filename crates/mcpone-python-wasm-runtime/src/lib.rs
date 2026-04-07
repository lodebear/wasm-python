//! MCP One Python WASM Guest Runtime
//!
//! This crate implements the wit-dylib interpreter interface for the MCP One sandbox.
//! It replaces `libcomponentize_py_runtime.so` with a purpose-built runtime that
//! hardcodes the sandbox exports (execute, snapshot-state, restore-state, clear-state).
//!
//! # Architecture
//!
//! wit-dylib generates bindings that call into an "interpreter" via a C FFI interface.
//! This crate implements that interface by:
//!
//! 1. Implementing the `Interpreter` trait from `wit-dylib-ffi`
//! 2. Using the `export!` macro to generate the required `#[no_mangle]` exports
//! 3. Dispatching export calls to hardcoded implementations
//!
//! # Exports
//!
//! - `execute(code: string) -> result<string, string>` - Run Python code
//! - `snapshot-state() -> result<list<u8>, string>` - Pickle globals
//! - `restore-state(data: list<u8>) -> result<_, string>` - Restore globals
//! - `clear-state()` - Clear globals

#![allow(unsafe_code)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

pub mod python;

use std::alloc::Layout;
use wit_dylib_ffi::{
    Call, Enum, ExportFunction, Flags, Future, Interpreter, List, Record, Resource, Stream, Tuple,
    Variant, Wit, WitOption, WitResult,
};

// =============================================================================
// Component Model async intrinsics
// =============================================================================
//
// These are imported from the "$root" module and provided by wasmtime's
// Component Model async support. They enable suspend/resume of async operations.

// Note: we intentionally provide local fallback implementations for the
// component-model async helper intrinsics. This allows building/running in
// environments where component async builtins are unavailable (e.g. current
// Wasmtime C API integrations used by the Go host).
//
// These helpers are sufficient for the current synchronous host contract where
// invoke/network operations complete immediately from the guest's perspective.

std::thread_local! {
    static CONTEXT_SLOT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

static NEXT_WAITABLE_SET: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

/// Create a waitable set handle.
pub fn waitable_set_new() -> u32 {
    NEXT_WAITABLE_SET.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Drop a waitable set (no-op in synchronous fallback mode).
pub fn waitable_set_drop(_set: u32) {}

/// Join a waitable into a waitable set (no-op in synchronous fallback mode).
pub fn waitable_join(_waitable: u32, _set: u32) {}

/// Store a context pointer in thread-local storage.
pub fn context_set(ptr: u32) {
    CONTEXT_SLOT.with(|slot| slot.set(ptr));
}

/// Read the thread-local context pointer.
pub fn context_get() -> u32 {
    CONTEXT_SLOT.with(|slot| slot.get())
}

/// Drop a completed subtask (no-op in synchronous fallback mode).
pub fn subtask_drop(_task: u32) {}

/// Our call context - holds a stack for passing values between wit-dylib and our code.
#[derive(Debug)]
pub struct McponeCall {
    /// Stack of values being passed.
    /// For simplicity, we use a `Vec<Value>` where `Value` is an enum of possible types.
    stack: Vec<Value>,
    /// Deferred deallocations
    deferred: Vec<(*mut u8, Layout)>,
}

/// A value on the call stack.
#[derive(Debug, Clone)]
enum Value {
    Bool(bool),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    S8(i8),
    S16(i16),
    S32(i32),
    S64(i64),
    F32(f32),
    F64(f64),
    Char(char),
    String(String),
    Bytes(Vec<u8>),
    /// Generic list argument encoding.
    ///
    /// Each outer vector entry is one list element.
    /// Each inner vector contains the stack values for that element in push order.
    ListArg(Vec<Vec<Value>>),
    /// For result<T, E>: true = ok, false = err
    ResultDiscriminant(bool),
    /// For option<T>: true = some, false = none
    OptionDiscriminant(bool),
}

impl McponeCall {
    fn new() -> Self {
        Self {
            stack: Vec::new(),
            deferred: Vec::new(),
        }
    }
}

impl Drop for McponeCall {
    fn drop(&mut self) {
        // Clean up all deferred allocations
        for (ptr, layout) in self.deferred.drain(..) {
            if !ptr.is_null() && layout.size() > 0 {
                // Safety: ptr and layout were created together via Box::into_raw
                unsafe {
                    std::alloc::dealloc(ptr, layout);
                }
            }
        }
    }
}

impl Call for McponeCall {
    unsafe fn defer_deallocate(&mut self, ptr: *mut u8, layout: Layout) {
        self.deferred.push((ptr, layout));
    }

    fn pop_bool(&mut self) -> bool {
        match self.stack.pop() {
            Some(Value::Bool(v)) => v,
            other => panic!("expected Bool, got {other:?}"),
        }
    }

    fn pop_u8(&mut self) -> u8 {
        match self.stack.pop() {
            Some(Value::U8(v)) => v,
            other => panic!("expected U8, got {other:?}"),
        }
    }

    fn pop_u16(&mut self) -> u16 {
        match self.stack.pop() {
            Some(Value::U16(v)) => v,
            other => panic!("expected U16, got {other:?}"),
        }
    }

    fn pop_u32(&mut self) -> u32 {
        match self.stack.pop() {
            Some(Value::U32(v)) => v,
            other => panic!("expected U32, got {other:?}"),
        }
    }

    fn pop_u64(&mut self) -> u64 {
        match self.stack.pop() {
            Some(Value::U64(v)) => v,
            other => panic!("expected U64, got {other:?}"),
        }
    }

    fn pop_s8(&mut self) -> i8 {
        match self.stack.pop() {
            Some(Value::S8(v)) => v,
            other => panic!("expected S8, got {other:?}"),
        }
    }

    fn pop_s16(&mut self) -> i16 {
        match self.stack.pop() {
            Some(Value::S16(v)) => v,
            other => panic!("expected S16, got {other:?}"),
        }
    }

    fn pop_s32(&mut self) -> i32 {
        match self.stack.pop() {
            Some(Value::S32(v)) => v,
            other => panic!("expected S32, got {other:?}"),
        }
    }

    fn pop_s64(&mut self) -> i64 {
        match self.stack.pop() {
            Some(Value::S64(v)) => v,
            other => panic!("expected S64, got {other:?}"),
        }
    }

    fn pop_f32(&mut self) -> f32 {
        match self.stack.pop() {
            Some(Value::F32(v)) => v,
            other => panic!("expected F32, got {other:?}"),
        }
    }

    fn pop_f64(&mut self) -> f64 {
        match self.stack.pop() {
            Some(Value::F64(v)) => v,
            other => panic!("expected F64, got {other:?}"),
        }
    }

    fn pop_char(&mut self) -> char {
        match self.stack.pop() {
            Some(Value::Char(v)) => v,
            other => panic!("expected Char, got {other:?}"),
        }
    }

    fn pop_string(&mut self) -> &str {
        match self.stack.pop() {
            Some(Value::String(s)) => {
                // Convert to boxed str and get raw pointer
                let boxed = s.into_boxed_str();
                let ptr = Box::into_raw(boxed);
                // Safety: ptr is valid and points to a str
                let layout = Layout::for_value(unsafe { &*ptr });
                // Track for deallocation when McponeCall is dropped
                self.deferred.push((ptr as *mut u8, layout));
                // Safety: ptr remains valid until McponeCall is dropped
                unsafe { &*ptr }
            }
            other => panic!("expected String, got {other:?}"),
        }
    }

    fn pop_borrow(&mut self, _ty: Resource) -> u32 {
        self.pop_u32()
    }

    fn pop_own(&mut self, _ty: Resource) -> u32 {
        self.pop_u32()
    }

    fn pop_enum(&mut self, _ty: Enum) -> u32 {
        self.pop_u32()
    }

    fn pop_flags(&mut self, _ty: Flags) -> u32 {
        self.pop_u32()
    }

    fn pop_future(&mut self, _ty: Future) -> u32 {
        self.pop_u32()
    }

    fn pop_stream(&mut self, _ty: Stream) -> u32 {
        self.pop_u32()
    }

    fn pop_option(&mut self, _ty: WitOption) -> u32 {
        match self.stack.pop() {
            Some(Value::OptionDiscriminant(is_some)) => {
                if is_some {
                    1
                } else {
                    0
                }
            }
            other => panic!("expected OptionDiscriminant, got {other:?}"),
        }
    }

    fn pop_result(&mut self, _ty: WitResult) -> u32 {
        match self.stack.pop() {
            Some(Value::ResultDiscriminant(is_ok)) => {
                if is_ok {
                    0
                } else {
                    1
                }
            }
            other => panic!("expected ResultDiscriminant, got {other:?}"),
        }
    }

    fn pop_variant(&mut self, _ty: Variant) -> u32 {
        self.pop_u32()
    }

    fn pop_record(&mut self, _ty: Record) {
        // Records are flattened - fields are already on the stack
    }

    fn pop_tuple(&mut self, _ty: Tuple) {
        // Tuples are flattened - elements are already on the stack
    }

    fn pop_list(&mut self, _ty: List) -> usize {
        match self.stack.pop() {
            Some(Value::Bytes(bytes)) => {
                let len = bytes.len();
                // Push bytes back for iteration in natural order.
                for b in bytes.into_iter().rev() {
                    self.stack.push(Value::U8(b));
                }
                len
            }
            Some(Value::ListArg(elements)) => {
                let len = elements.len();
                // Push list elements in reverse so the first element is popped first.
                for element in elements.into_iter().rev() {
                    for value in element {
                        self.stack.push(value);
                    }
                }
                len
            }
            other => panic!("expected list argument, got {other:?}"),
        }
    }

    fn pop_iter_next(&mut self, _ty: List) {
        // Called for each element during iteration - element should already be ready
    }

    fn pop_iter(&mut self, _ty: List) {
        // Called when iteration is complete
    }

    fn push_bool(&mut self, val: bool) {
        self.stack.push(Value::Bool(val));
    }

    fn push_char(&mut self, val: char) {
        self.stack.push(Value::Char(val));
    }

    fn push_u8(&mut self, val: u8) {
        self.stack.push(Value::U8(val));
    }

    fn push_s8(&mut self, val: i8) {
        self.stack.push(Value::S8(val));
    }

    fn push_u16(&mut self, val: u16) {
        self.stack.push(Value::U16(val));
    }

    fn push_s16(&mut self, val: i16) {
        self.stack.push(Value::S16(val));
    }

    fn push_u32(&mut self, val: u32) {
        self.stack.push(Value::U32(val));
    }

    fn push_s32(&mut self, val: i32) {
        self.stack.push(Value::S32(val));
    }

    fn push_u64(&mut self, val: u64) {
        self.stack.push(Value::U64(val));
    }

    fn push_s64(&mut self, val: i64) {
        self.stack.push(Value::S64(val));
    }

    fn push_f32(&mut self, val: f32) {
        self.stack.push(Value::F32(val));
    }

    fn push_f64(&mut self, val: f64) {
        self.stack.push(Value::F64(val));
    }

    fn push_string(&mut self, val: String) {
        self.stack.push(Value::String(val));
    }

    fn push_record(&mut self, _ty: Record) {
        // Records are flattened - fields will be pushed individually
    }

    fn push_tuple(&mut self, _ty: Tuple) {
        // Tuples are flattened - elements will be pushed individually
    }

    fn push_flags(&mut self, _ty: Flags, bits: u32) {
        self.stack.push(Value::U32(bits));
    }

    fn push_enum(&mut self, _ty: Enum, discr: u32) {
        self.stack.push(Value::U32(discr));
    }

    fn push_borrow(&mut self, _ty: Resource, handle: u32) {
        self.stack.push(Value::U32(handle));
    }

    fn push_own(&mut self, _ty: Resource, handle: u32) {
        self.stack.push(Value::U32(handle));
    }

    fn push_future(&mut self, _ty: Future, handle: u32) {
        self.stack.push(Value::U32(handle));
    }

    fn push_stream(&mut self, _ty: Stream, handle: u32) {
        self.stack.push(Value::U32(handle));
    }

    fn push_variant(&mut self, _ty: Variant, discr: u32) {
        self.stack.push(Value::U32(discr));
    }

    fn push_option(&mut self, _ty: WitOption, is_some: bool) {
        self.stack.push(Value::OptionDiscriminant(is_some));
    }

    fn push_result(&mut self, _ty: WitResult, is_err: bool) {
        self.stack.push(Value::ResultDiscriminant(!is_err));
    }

    fn push_list(&mut self, _ty: List, _capacity: usize) {
        // Start collecting list elements
        self.stack.push(Value::Bytes(Vec::new()));
    }

    fn list_append(&mut self, _ty: List) {
        // Pop the element and append to the list
        let elem = self.stack.pop();
        if let Some(Value::Bytes(bytes)) = self.stack.last_mut()
            && let Some(Value::U8(b)) = elem
        {
            bytes.push(b);
        }
    }
}

/// Our interpreter implementation.
#[derive(Debug)]
pub struct McponeInterpreter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExportKind {
    Execute,
    SnapshotState,
    RestoreState,
    ClearState,
    FinalizePreinit,
    WasiCliRun,
}

fn resolve_export_kind(func: ExportFunction) -> Option<ExportKind> {
    match (func.interface(), func.name()) {
        (None, "execute") => Some(ExportKind::Execute),
        (None, "snapshot-state") => Some(ExportKind::SnapshotState),
        (None, "restore-state") => Some(ExportKind::RestoreState),
        (None, "clear-state") => Some(ExportKind::ClearState),
        (None, "finalize-preinit") => Some(ExportKind::FinalizePreinit),
        (Some(interface), "run") if interface.starts_with("wasi:cli/run") => {
            Some(ExportKind::WasiCliRun)
        }
        _ => None,
    }
}

// =============================================================================
// Invoke callback mechanism
// =============================================================================
//
// This allows Python code to call host functions via the invoke() function.
// The mechanism works as follows:
// 1. Python calls invoke(name, **kwargs) which serializes args to JSON
// 2. invoke() calls _mcpone._mcpone_invoke(name, args_json) (C extension)
// 3. _mcpone_invoke calls python::do_invoke() which uses the INVOKE_CALLBACK
// 4. The callback (set by with_wit) calls the WIT invoke import
// 5. Result JSON is returned back through the chain

// Thread-local storage for the current Wit handle during export execution.
std::thread_local! {
    static CURRENT_WIT: std::cell::RefCell<Option<Wit>> = const { std::cell::RefCell::new(None) };
}

/// Execute a function with the Wit handle available for host imports.
fn with_wit<T>(wit: Wit, f: impl FnOnce() -> T) -> T {
    CURRENT_WIT.with(|cell| {
        let old = cell.borrow_mut().replace(wit);
        let result = f();
        *cell.borrow_mut() = old;
        result
    })
}

/// Wrapper function that matches the InvokeCallback signature (synchronous).
fn invoke_callback_wrapper(name: &str, args_json: &str) -> Result<String, String> {
    call_invoke(name, args_json)
}

/// Wrapper function that matches the InvokeAsyncCallback signature.
fn invoke_async_callback_wrapper(name: &str, args_json: &str) -> Result<InvokeResult, String> {
    call_invoke_async(name, args_json)
}

/// Result of calling an async invoke - either immediate result or pending
#[derive(Debug)]
pub enum InvokeResult {
    /// Immediate success with JSON result
    Ok(String),
    /// Immediate error with error message
    Err(String),
    /// Pending - need to wait for callback. Contains (waitable_id, promise_id)
    Pending(u32, u32),
}

// =============================================================================
// Pending async import tracking
// =============================================================================
//
// When an async import is pending, we need to store state so that when the
// subtask completes, we can lift the result from the buffer.

/// Type of async import, used to determine how to lift the result.
///
/// Note: TCP/TLS operations now use fiber-based async (`call_import_sync` with `func_wrap_async`
/// on the host), so they complete synchronously from the guest's perspective and don't need
/// to be tracked as pending imports. Only `invoke` remains async with Component Model async.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportType {
    /// invoke: result<string, string>
    Invoke,
}

/// Stored state for a pending async import call.
struct PendingImportState {
    /// Type of import (determines result lifting logic)
    import_type: ImportType,
    /// Function to lift the result from buffer onto call stack
    async_lift_impl: unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void),
    /// Buffer where the result is stored
    buffer: *mut u8,
    /// Keep the call context alive so the buffer isn't deallocated.
    /// wit-dylib calls `cx.defer_deallocate(buffer, layout)` which means
    /// the buffer is freed when the cx drops. We need to keep it alive.
    _cx: Box<McponeCall>,
}

// Safety: WASM is single-threaded
unsafe impl Send for PendingImportState {}
unsafe impl Sync for PendingImportState {}

// Map of subtask -> pending import state
std::thread_local! {
    static PENDING_IMPORTS: std::cell::RefCell<std::collections::HashMap<u32, PendingImportState>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Call the invoke import with the given callback name and JSON arguments.
///
/// In the MCP One runtime we currently use a synchronous WIT import for `invoke`.
/// The Python API still exposes an async `invoke()` helper, but that helper now
/// resolves immediately with this synchronous host call.
fn call_invoke_async(name: &str, args_json: &str) -> Result<InvokeResult, String> {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let wit = wit
            .as_ref()
            .ok_or_else(|| "invoke() called outside of execute context".to_string())?;

        let import_func = wit
            .get_import(None, "invoke")
            .ok_or_else(|| "invoke import not found".to_string())?;

        let mut cx = Box::new(McponeCall::new());

        // wit-dylib pops in reverse order, so push arguments in reverse.
        cx.push_string(args_json.to_string());
        cx.push_string(name.to_string());

        // Synchronous host import call.
        import_func.call_import_sync(&mut *cx);

        // Result is on the stack: result<string, string>
        let is_ok = match cx.stack.pop() {
            Some(Value::ResultDiscriminant(v)) => v,
            other => {
                return Err(format!("unexpected result discriminant: {other:?}"));
            }
        };

        let value = match cx.stack.pop() {
            Some(Value::String(s)) => s,
            other => {
                return Err(format!("unexpected result value: {other:?}"));
            }
        };

        if is_ok {
            Ok(InvokeResult::Ok(value))
        } else {
            Ok(InvokeResult::Err(value))
        }
    })
}

/// Synchronous invoke wrapper for backwards compatibility.
/// Returns error if the operation is pending (requires async).
fn call_invoke(name: &str, args_json: &str) -> Result<String, String> {
    match call_invoke_async(name, args_json) {
        Ok(InvokeResult::Ok(result)) => Ok(result),
        Ok(InvokeResult::Err(error)) => Err(error),
        Ok(InvokeResult::Pending(_, _)) => Err(
            "callback requires async execution which is not supported in synchronous mode"
                .to_string(),
        ),
        Err(e) => Err(e),
    }
}

/// Call list-callbacks import to get available callbacks from the host.
fn call_list_callbacks(wit: Wit) -> Vec<python::CallbackInfo> {
    // Get the list-callbacks import function
    let import_func = match wit.get_import(None, "list-callbacks") {
        Some(f) => f,
        None => {
            return Vec::new();
        }
    };

    // Create a call context to receive the result
    let mut cx = McponeCall::new();

    // Call the import (synchronous)
    import_func.call_import_sync(&mut cx);

    // Parse the stack contents into callback info.
    // The actual format depends on how wasmtime and wit-dylib interact.
    // From observation, the stack contains flattened record fields.
    // We'll collect all string values and try to group them.
    let mut callbacks = Vec::new();
    let mut strings: Vec<String> = Vec::new();

    // Collect all string values from the stack, skipping empty Bytes
    for value in cx.stack.drain(..) {
        match value {
            Value::String(s) => strings.push(s),
            Value::Bytes(b) if !b.is_empty() => {
                // Non-empty bytes might be a string
                if let Ok(s) = String::from_utf8(b) {
                    strings.push(s);
                }
            }
            _ => {} // Skip empty Bytes and other types
        }
    }

    // Group strings into callbacks.
    // Each callback has: name, description, (optional parameters_schema_json)
    // From observation, we get pairs of (name, description) when schema is empty
    if strings.len() >= 2 {
        // Assume pairs of (name, description)
        for chunk in strings.chunks(2) {
            if chunk.len() >= 2 {
                callbacks.push(python::CallbackInfo {
                    name: chunk[0].clone(),
                    description: chunk[1].clone(),
                    parameters_schema_json: String::new(), // Schema not available in this format
                });
            }
        }
    }

    callbacks
}

/// Call report-trace import to send a trace event to the host.
/// This is a synchronous call with no return value.
fn call_report_trace(lineno: u32, event_json: &str, context_json: &str) {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let Some(wit) = wit.as_ref() else {
            return; // No WIT context - tracing not available
        };

        // Get the report-trace import function
        let import_func = match wit.get_import(None, "report-trace") {
            Some(f) => f,
            None => return, // Tracing not available
        };

        // Create a call context and push arguments
        let mut cx = McponeCall::new();

        // Push arguments in reverse order (wit-dylib pops in reverse)
        cx.push_string(context_json.to_string());
        cx.push_string(event_json.to_string());
        cx.push_u32(lineno);

        // Call the import (synchronous, no return value)
        import_func.call_import_sync(&mut cx);
    });
}

/// Wrapper function that matches the ReportTraceCallback signature.
fn report_trace_callback_wrapper(lineno: u32, event_json: &str, context_json: &str) {
    call_report_trace(lineno, event_json, context_json);
}

/// Call report-output import to stream output to the host in real-time.
/// This is a synchronous call with no return value.
fn call_report_output(stream: u32, data: &str) {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let Some(wit) = wit.as_ref() else {
            return; // No WIT context - output streaming not available
        };

        // Get the report-output import function
        let import_func = match wit.get_import(None, "report-output") {
            Some(f) => f,
            None => return, // Output streaming not available
        };

        // Create a call context and push arguments
        let mut cx = McponeCall::new();

        // Push arguments in reverse order (wit-dylib pops in reverse)
        cx.push_string(data.to_string());
        cx.push_u32(stream);

        // Call the import (synchronous, no return value)
        import_func.call_import_sync(&mut cx);
    });
}

/// Wrapper function that matches the ReportOutputCallback signature.
fn report_output_callback_wrapper(stream: u32, data: &str) {
    call_report_output(stream, data);
}

// =============================================================================
// Network import wrappers (TCP and TLS)
// =============================================================================
//
// TCP has been ported to WASI Preview2 sockets so the guest can use monocp's
// existing host interfaces. TLS remains on the custom mcpone host import for
// now (unchanged).

/// Result of calling an async network operation.
#[derive(Debug)]
pub enum NetResult<T> {
    /// Immediate success with value.
    Ok(T),
    /// Immediate error with error variant discriminant and optional message.
    Err(u32, Option<String>),
    /// Pending - need to wait for callback. Contains (waitable_id, promise_id).
    Pending(u32, u32),
}

#[derive(Debug, Clone, Copy)]
struct WasiTcpConnection {
    socket: u32,
    input_stream: u32,
    output_stream: u32,
}

#[derive(Debug, Clone, Copy)]
enum WasiIpAddress {
    V4([u8; 4]),
    V6([u16; 8], u32),
}

std::thread_local! {
    static WASI_TCP_CONNECTIONS: std::cell::RefCell<std::collections::HashMap<u32, WasiTcpConnection>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    static NEXT_WASI_TCP_HANDLE: std::cell::Cell<u32> = const { std::cell::Cell::new(1) };
    static CACHED_WASI_NETWORK_HANDLE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn get_import_any_interface(
    wit: &Wit,
    interfaces: &[&str],
    name: &str,
) -> Option<wit_dylib_ffi::ImportFunction> {
    for interface in interfaces {
        if let Some(import) = wit.get_import(Some(interface), name) {
            return Some(import);
        }
    }
    None
}

fn get_wasi_import(
    wit: &Wit,
    interfaces: &[&str],
    name: &str,
    label: &str,
) -> Result<wit_dylib_ffi::ImportFunction, String> {
    get_import_any_interface(wit, interfaces, name).ok_or_else(|| {
        format!(
            "{label} import not found (tried interfaces: {})",
            interfaces.join(", ")
        )
    })
}

fn pop_result_discriminant(cx: &mut McponeCall) -> Result<bool, String> {
    match cx.stack.pop() {
        Some(Value::ResultDiscriminant(v)) => Ok(v),
        other => Err(format!("unexpected result discriminant: {other:?}")),
    }
}

fn pop_u32_value(cx: &mut McponeCall, what: &str) -> Result<u32, String> {
    match cx.stack.pop() {
        Some(Value::U32(v)) => Ok(v),
        other => Err(format!("unexpected {what}: {other:?}")),
    }
}

fn pop_u64_value(cx: &mut McponeCall, what: &str) -> Result<u64, String> {
    match cx.stack.pop() {
        Some(Value::U64(v)) => Ok(v),
        other => Err(format!("unexpected {what}: {other:?}")),
    }
}

fn pop_s32_value(cx: &mut McponeCall, what: &str) -> Result<i32, String> {
    match cx.stack.pop() {
        Some(Value::S32(v)) => Ok(v),
        other => Err(format!("unexpected {what}: {other:?}")),
    }
}

fn pop_string_value(cx: &mut McponeCall, what: &str) -> Result<String, String> {
    match cx.stack.pop() {
        Some(Value::String(v)) => Ok(v),
        other => Err(format!("unexpected {what}: {other:?}")),
    }
}

fn pop_u16_value(cx: &mut McponeCall, what: &str) -> Result<u16, String> {
    match cx.stack.pop() {
        Some(Value::U16(v)) => Ok(v),
        other => Err(format!("unexpected {what}: {other:?}")),
    }
}

fn pop_u8_value(cx: &mut McponeCall, what: &str) -> Result<u8, String> {
    match cx.stack.pop() {
        Some(Value::U8(v)) => Ok(v),
        other => Err(format!("unexpected {what}: {other:?}")),
    }
}

fn pop_bytes_value(cx: &mut McponeCall, what: &str) -> Result<Vec<u8>, String> {
    match cx.stack.pop() {
        Some(Value::Bytes(v)) => Ok(v),
        other => Err(format!("unexpected {what}: {other:?}")),
    }
}

fn pop_option_discriminant(cx: &mut McponeCall, what: &str) -> Result<bool, String> {
    match cx.stack.pop() {
        Some(Value::OptionDiscriminant(v)) => Ok(v),
        other => Err(format!("unexpected {what}: {other:?}")),
    }
}

fn parse_ip_literal(host: &str) -> Option<WasiIpAddress> {
    use std::net::IpAddr;

    let parsed = host.parse::<IpAddr>().ok()?;
    match parsed {
        IpAddr::V4(v4) => Some(WasiIpAddress::V4(v4.octets())),
        IpAddr::V6(v6) => Some(WasiIpAddress::V6(v6.segments(), 0)),
    }
}

fn wasi_socket_error_name(discr: u32) -> &'static str {
    match discr {
        0 => "unknown",
        1 => "access-denied",
        2 => "not-supported",
        3 => "invalid-argument",
        4 => "out-of-memory",
        5 => "timeout",
        6 => "concurrency-conflict",
        7 => "not-in-progress",
        8 => "would-block",
        9 => "invalid-state",
        10 => "new-socket-limit",
        11 => "address-not-bindable",
        12 => "address-in-use",
        13 => "remote-unreachable",
        14 => "connection-refused",
        15 => "connection-reset",
        16 => "connection-aborted",
        17 => "datagram-too-large",
        18 => "name-unresolvable",
        19 => "temporary-resolver-failure",
        20 => "permanent-resolver-failure",
        _ => "unknown",
    }
}

fn map_wasi_socket_error_to_tcp(discr: u32, detail: Option<String>) -> (u32, Option<String>) {
    match discr {
        14 => (0, detail),           // connection-refused
        15 => (1, detail),           // connection-reset
        5 => (2, detail),            // timeout -> timed-out
        18 | 19 | 20 => (3, detail), // resolver failures -> host-not-found
        1 => (
            5,
            detail.or_else(|| Some(wasi_socket_error_name(discr).to_string())),
        ), // access-denied -> not-permitted
        9 => (6, detail),            // invalid-state -> invalid-handle
        _ => (
            4,
            detail.or_else(|| Some(wasi_socket_error_name(discr).to_string())),
        ), // io-error
    }
}

const WASI_INSTANCE_NETWORK_INTERFACES: &[&str] = &[
    "wasi:sockets/instance-network@0.2.6",
    "wasi:sockets/instance-network@0.2.3",
    "wasi:sockets/instance-network@0.2.0",
    "wasi:sockets/instance-network",
    "wasi:network/instance-network@0.2.6",
    "wasi:network/instance-network@0.2.3",
    "wasi:network/instance-network@0.2.0",
    "wasi:network/instance-network",
];

const WASI_IP_LOOKUP_INTERFACES: &[&str] = &[
    "wasi:sockets/ip-name-lookup@0.2.6",
    "wasi:sockets/ip-name-lookup@0.2.3",
    "wasi:sockets/ip-name-lookup@0.2.0",
    "wasi:sockets/ip-name-lookup",
    "wasi:ip-name-lookup/ip-name-lookup@0.2.6",
    "wasi:ip-name-lookup/ip-name-lookup@0.2.3",
    "wasi:ip-name-lookup/ip-name-lookup@0.2.0",
    "wasi:ip-name-lookup/ip-name-lookup",
];

const WASI_TCP_CREATE_INTERFACES: &[&str] = &[
    "wasi:sockets/tcp-create-socket@0.2.6",
    "wasi:sockets/tcp-create-socket@0.2.3",
    "wasi:sockets/tcp-create-socket@0.2.0",
    "wasi:sockets/tcp-create-socket",
];

const WASI_TCP_INTERFACES: &[&str] = &[
    "wasi:sockets/tcp@0.2.6",
    "wasi:sockets/tcp@0.2.3",
    "wasi:sockets/tcp@0.2.0",
    "wasi:sockets/tcp",
];

const WASI_IO_STREAMS_INTERFACES: &[&str] = &[
    "wasi:io/streams@0.2.6",
    "wasi:io/streams@0.2.3",
    "wasi:io/streams@0.2.0",
    "wasi:io/streams",
];

const WASI_IO_ERROR_INTERFACES: &[&str] = &[
    "wasi:io/error@0.2.6",
    "wasi:io/error@0.2.3",
    "wasi:io/error@0.2.0",
    "wasi:io/error",
];

fn map_stream_error_to_tcp(closed: bool, detail: Option<String>) -> (u32, Option<String>) {
    if closed {
        return (6, None); // invalid-handle
    }

    if let Some(msg) = detail.as_ref() {
        let lower = msg.to_ascii_lowercase();
        if lower.contains("connection refused") {
            return (0, Some(msg.clone()));
        }
        if lower.contains("connection reset") || lower.contains("broken pipe") {
            return (1, Some(msg.clone()));
        }
        if lower.contains("timeout") {
            return (2, Some(msg.clone()));
        }
        if lower.contains("name") && lower.contains("resolv") {
            return (3, Some(msg.clone()));
        }
        if lower.contains("permission") || lower.contains("access denied") {
            return (5, Some(msg.clone()));
        }
        if lower.contains("closed") {
            return (6, Some(msg.clone()));
        }
    }

    (4, detail)
}

fn wasi_tcp_has_handle(handle: u32) -> bool {
    WASI_TCP_CONNECTIONS.with(|cell| cell.borrow().contains_key(&handle))
}

fn wasi_tcp_get(handle: u32) -> Option<WasiTcpConnection> {
    WASI_TCP_CONNECTIONS.with(|cell| cell.borrow().get(&handle).copied())
}

fn wasi_tcp_remove(handle: u32) -> Option<WasiTcpConnection> {
    WASI_TCP_CONNECTIONS.with(|cell| cell.borrow_mut().remove(&handle))
}

fn wasi_tcp_insert(conn: WasiTcpConnection) -> u32 {
    WASI_TCP_CONNECTIONS.with(|cell| {
        NEXT_WASI_TCP_HANDLE.with(|next| {
            let mut map = cell.borrow_mut();
            let mut handle = next.get();

            if handle == 0 {
                handle = 1;
            }

            while handle == 0 || map.contains_key(&handle) {
                handle = handle.wrapping_add(1);
                if handle == 0 {
                    handle = 1;
                }
            }

            map.insert(handle, conn);

            let mut next_handle = handle.wrapping_add(1);
            if next_handle == 0 {
                next_handle = 1;
            }
            next.set(next_handle);

            handle
        })
    })
}

fn call_wasi_io_error_to_debug_string(wit: &Wit, error_handle: u32) -> Result<String, String> {
    let import_func = get_wasi_import(
        wit,
        WASI_IO_ERROR_INTERFACES,
        "[method]error.to-debug-string",
        "wasi:io/error.to-debug-string",
    )?;

    let mut cx = McponeCall::new();
    cx.push_u32(error_handle);
    import_func.call_import_sync(&mut cx);

    match cx.stack.pop() {
        Some(Value::String(s)) => Ok(s),
        other => Err(format!("unexpected io error debug string: {other:?}")),
    }
}

fn get_or_create_wasi_network_handle(wit: &Wit) -> Result<u32, String> {
    if let Some(existing) = CACHED_WASI_NETWORK_HANDLE.with(|cell| {
        let value = cell.get();
        if value == 0 { None } else { Some(value) }
    }) {
        return Ok(existing);
    }

    let import_func = get_wasi_import(
        wit,
        WASI_INSTANCE_NETWORK_INTERFACES,
        "instance-network",
        "wasi:sockets/instance-network.instance-network",
    )?;

    let mut cx = McponeCall::new();
    import_func.call_import_sync(&mut cx);

    let network = pop_u32_value(&mut cx, "instance-network result")?;
    CACHED_WASI_NETWORK_HANDLE.with(|cell| cell.set(network));
    Ok(network)
}

fn resolve_wasi_ip_address(
    wit: &Wit,
    network_handle: u32,
    host: &str,
) -> Result<WasiIpAddress, (u32, Option<String>)> {
    if let Some(ip) = parse_ip_literal(host) {
        return Ok(ip);
    }

    let resolve_func = get_wasi_import(
        wit,
        WASI_IP_LOOKUP_INTERFACES,
        "resolve-addresses",
        "wasi:sockets/ip-name-lookup.resolve-addresses",
    )
    .map_err(|e| (4, Some(e)))?;

    let mut cx = McponeCall::new();
    // Args (network, name) pushed in reverse.
    cx.push_string(host.to_string());
    cx.push_u32(network_handle);
    resolve_func.call_import_sync(&mut cx);

    let is_ok = pop_result_discriminant(&mut cx)
        .map_err(|e| (4, Some(format!("resolve-addresses: {e}"))))?;
    if !is_ok {
        let discr = pop_u32_value(&mut cx, "resolve-addresses error")
            .map_err(|e| (4, Some(format!("resolve-addresses: {e}"))))?;
        return Err(map_wasi_socket_error_to_tcp(
            discr,
            Some(wasi_socket_error_name(discr).to_string()),
        ));
    }

    let stream_handle = pop_u32_value(&mut cx, "resolve-addresses stream")
        .map_err(|e| (4, Some(format!("resolve-addresses: {e}"))))?;

    let next_func = get_wasi_import(
        wit,
        WASI_IP_LOOKUP_INTERFACES,
        "[method]resolve-address-stream.resolve-next-address",
        "wasi:sockets/ip-name-lookup.resolve-next-address",
    )
    .map_err(|e| (4, Some(e)))?;

    let mut cx = McponeCall::new();
    cx.push_u32(stream_handle);
    next_func.call_import_sync(&mut cx);

    let is_ok = pop_result_discriminant(&mut cx)
        .map_err(|e| (4, Some(format!("resolve-next-address: {e}"))))?;
    if !is_ok {
        let discr = pop_u32_value(&mut cx, "resolve-next-address error")
            .map_err(|e| (4, Some(format!("resolve-next-address: {e}"))))?;
        return Err(map_wasi_socket_error_to_tcp(
            discr,
            Some(wasi_socket_error_name(discr).to_string()),
        ));
    }

    let is_some = pop_option_discriminant(&mut cx, "resolve-next-address option")
        .map_err(|e| (4, Some(format!("resolve-next-address: {e}"))))?;
    if !is_some {
        return Err((3, Some("host-not-found".to_string())));
    }

    let addr_discr = pop_u32_value(&mut cx, "resolve-next-address ip discriminant")
        .map_err(|e| (4, Some(format!("resolve-next-address: {e}"))))?;

    match addr_discr {
        0 => {
            let mut octets = [0_u8; 4];
            for (idx, octet) in octets.iter_mut().enumerate() {
                *octet = pop_u8_value(&mut cx, &format!("ipv4 octet {idx}"))
                    .map_err(|e| (4, Some(format!("resolve-next-address: {e}"))))?;
            }
            Ok(WasiIpAddress::V4(octets))
        }
        1 => {
            let mut segments = [0_u16; 8];
            for (idx, segment) in segments.iter_mut().enumerate() {
                *segment = pop_u16_value(&mut cx, &format!("ipv6 segment {idx}"))
                    .map_err(|e| (4, Some(format!("resolve-next-address: {e}"))))?;
            }
            Ok(WasiIpAddress::V6(segments, 0))
        }
        other => Err((
            4,
            Some(format!("unsupported ip-address discriminant: {other}")),
        )),
    }
}

fn push_ip_socket_address_arg(cx: &mut McponeCall, addr: WasiIpAddress, port: u16) {
    match addr {
        WasiIpAddress::V4(octets) => {
            // Payload order for ipv4-socket-address record:
            // { port, address(tuple<u8,u8,u8,u8>) }
            // Push in reverse because wit-dylib pops in reverse.
            cx.push_u8(octets[3]);
            cx.push_u8(octets[2]);
            cx.push_u8(octets[1]);
            cx.push_u8(octets[0]);
            cx.push_u16(port);
            // ip-socket-address discriminant: ipv4 = 0
            cx.push_u32(0);
        }
        WasiIpAddress::V6(segments, scope_id) => {
            // Payload order for ipv6-socket-address record:
            // { port, flow-info, address(tuple<u16;8>), scope-id }
            // Push in reverse because wit-dylib pops in reverse.
            cx.push_u32(scope_id);
            cx.push_u16(segments[7]);
            cx.push_u16(segments[6]);
            cx.push_u16(segments[5]);
            cx.push_u16(segments[4]);
            cx.push_u16(segments[3]);
            cx.push_u16(segments[2]);
            cx.push_u16(segments[1]);
            cx.push_u16(segments[0]);
            cx.push_u32(0); // flow-info
            cx.push_u16(port);
            // ip-socket-address discriminant: ipv6 = 1
            cx.push_u32(1);
        }
    }
}

fn wasi_ip_family_discriminant(addr: WasiIpAddress) -> u32 {
    match addr {
        WasiIpAddress::V4(_) => 0,
        WasiIpAddress::V6(_, _) => 1,
    }
}

// -----------------------------------------------------------------------------
// TCP operations (ported to wasi:sockets)
// -----------------------------------------------------------------------------

/// Call the TCP connect import (synchronous - blocks until complete).
///
/// This uses WASI Preview2 sockets and stores stream resources in a local
/// handle table exposed as u32 handles to Python.
fn call_tcp_connect(host: &str, port: u16) -> Result<NetResult<u32>, String> {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let wit = wit
            .as_ref()
            .ok_or_else(|| "TCP connect called outside of execute context".to_string())?;

        let network_handle = get_or_create_wasi_network_handle(wit)?;

        let remote_addr = match resolve_wasi_ip_address(wit, network_handle, host) {
            Ok(addr) => addr,
            Err((discr, payload)) => return Ok(NetResult::Err(discr, payload)),
        };

        let create_func = get_wasi_import(
            wit,
            WASI_TCP_CREATE_INTERFACES,
            "create-tcp-socket",
            "wasi:sockets/tcp-create-socket.create-tcp-socket",
        )?;

        let mut cx = McponeCall::new();
        cx.push_u32(wasi_ip_family_discriminant(remote_addr));
        create_func.call_import_sync(&mut cx);

        let is_ok = pop_result_discriminant(&mut cx)?;
        let socket_handle = if is_ok {
            pop_u32_value(&mut cx, "create-tcp-socket tcp-socket")?
        } else {
            let discr = pop_u32_value(&mut cx, "create-tcp-socket error-code")?;
            let (tcp_discr, payload) = map_wasi_socket_error_to_tcp(
                discr,
                Some(wasi_socket_error_name(discr).to_string()),
            );
            return Ok(NetResult::Err(tcp_discr, payload));
        };

        let start_connect_func = get_wasi_import(
            wit,
            WASI_TCP_INTERFACES,
            "[method]tcp-socket.start-connect",
            "wasi:sockets/tcp.start-connect",
        )?;

        let mut cx = McponeCall::new();
        // Method args are (self, network, remote-address), push in reverse.
        push_ip_socket_address_arg(&mut cx, remote_addr, port);
        cx.push_u32(network_handle);
        cx.push_u32(socket_handle);
        start_connect_func.call_import_sync(&mut cx);

        let is_ok = pop_result_discriminant(&mut cx)?;
        if !is_ok {
            let discr = pop_u32_value(&mut cx, "tcp.start-connect error-code")?;
            let (tcp_discr, payload) = map_wasi_socket_error_to_tcp(
                discr,
                Some(wasi_socket_error_name(discr).to_string()),
            );
            return Ok(NetResult::Err(tcp_discr, payload));
        }

        let finish_connect_func = get_wasi_import(
            wit,
            WASI_TCP_INTERFACES,
            "[method]tcp-socket.finish-connect",
            "wasi:sockets/tcp.finish-connect",
        )?;

        let mut cx = McponeCall::new();
        cx.push_u32(socket_handle);
        finish_connect_func.call_import_sync(&mut cx);

        let is_ok = pop_result_discriminant(&mut cx)?;
        if !is_ok {
            let discr = pop_u32_value(&mut cx, "tcp.finish-connect error-code")?;
            let (tcp_discr, payload) = map_wasi_socket_error_to_tcp(
                discr,
                Some(wasi_socket_error_name(discr).to_string()),
            );
            return Ok(NetResult::Err(tcp_discr, payload));
        }

        // Success payload is tuple<input-stream, output-stream>
        let input_stream = pop_u32_value(&mut cx, "tcp.finish-connect input-stream")?;
        let output_stream = pop_u32_value(&mut cx, "tcp.finish-connect output-stream")?;

        let handle = wasi_tcp_insert(WasiTcpConnection {
            socket: socket_handle,
            input_stream,
            output_stream,
        });

        Ok(NetResult::Ok(handle))
    })
}

/// Call the TCP read import (synchronous - blocks until complete).
fn call_tcp_read(handle: u32, len: u32) -> Result<NetResult<Vec<u8>>, String> {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let wit = wit
            .as_ref()
            .ok_or_else(|| "TCP read called outside of execute context".to_string())?;

        let Some(conn) = wasi_tcp_get(handle) else {
            return Ok(NetResult::Err(6, None)); // invalid-handle
        };

        let import_func = get_wasi_import(
            wit,
            WASI_IO_STREAMS_INTERFACES,
            "[method]input-stream.blocking-read",
            "wasi:io/streams.input-stream.blocking-read",
        )?;

        let mut cx = McponeCall::new();
        // Method args are (self, len), push in reverse.
        cx.push_u64(len as u64);
        cx.push_u32(conn.input_stream);
        import_func.call_import_sync(&mut cx);

        let is_ok = pop_result_discriminant(&mut cx)?;
        if is_ok {
            let bytes = pop_bytes_value(&mut cx, "input-stream.read bytes")?;
            return Ok(NetResult::Ok(bytes));
        }

        // stream-error variant: 0=last-operation-failed(error), 1=closed
        let stream_err_discr = pop_u32_value(&mut cx, "input-stream.read stream-error")?;
        let (tcp_discr, payload) = match stream_err_discr {
            1 => map_stream_error_to_tcp(true, None),
            0 => {
                let err_handle = pop_u32_value(&mut cx, "input-stream.read error handle")?;
                let detail = call_wasi_io_error_to_debug_string(wit, err_handle).ok();
                map_stream_error_to_tcp(false, detail)
            }
            other => (
                4,
                Some(format!("unknown stream-error discriminant: {other}")),
            ),
        };

        Ok(NetResult::Err(tcp_discr, payload))
    })
}

/// Call the TCP write import (synchronous - blocks until complete).
fn call_tcp_write(handle: u32, data: &[u8]) -> Result<NetResult<u32>, String> {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let wit = wit
            .as_ref()
            .ok_or_else(|| "TCP write called outside of execute context".to_string())?;

        let Some(conn) = wasi_tcp_get(handle) else {
            return Ok(NetResult::Err(6, None)); // invalid-handle
        };

        let import_func = get_wasi_import(
            wit,
            WASI_IO_STREAMS_INTERFACES,
            "[method]output-stream.blocking-write-and-flush",
            "wasi:io/streams.output-stream.blocking-write-and-flush",
        )?;

        let mut cx = McponeCall::new();
        // Method args are (self, contents), push in reverse.
        cx.stack.push(Value::Bytes(data.to_vec()));
        cx.push_u32(conn.output_stream);
        import_func.call_import_sync(&mut cx);

        let is_ok = pop_result_discriminant(&mut cx)?;
        if is_ok {
            let written = u32::try_from(data.len()).unwrap_or(u32::MAX);
            return Ok(NetResult::Ok(written));
        }

        let stream_err_discr = pop_u32_value(&mut cx, "output-stream.write stream-error")?;
        let (tcp_discr, payload) = match stream_err_discr {
            1 => map_stream_error_to_tcp(true, None),
            0 => {
                let err_handle = pop_u32_value(&mut cx, "output-stream.write error handle")?;
                let detail = call_wasi_io_error_to_debug_string(wit, err_handle).ok();
                map_stream_error_to_tcp(false, detail)
            }
            other => (
                4,
                Some(format!("unknown stream-error discriminant: {other}")),
            ),
        };

        Ok(NetResult::Err(tcp_discr, payload))
    })
}

/// Call the TCP close import (synchronous).
fn call_tcp_close(handle: u32) {
    let Some(conn) = wasi_tcp_remove(handle) else {
        return;
    };

    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let Some(wit) = wit.as_ref() else {
            return;
        };

        let import_func = match get_wasi_import(
            wit,
            WASI_TCP_INTERFACES,
            "[method]tcp-socket.shutdown",
            "wasi:sockets/tcp.shutdown",
        ) {
            Ok(f) => f,
            Err(_) => return,
        };

        let mut cx = McponeCall::new();
        // Method args are (self, shutdown-type). Use "both" (=2).
        cx.push_u32(2);
        cx.push_u32(conn.socket);
        import_func.call_import_sync(&mut cx);
    });
}

/// TCP error discriminant to error name mapping.
fn tcp_error_name(discr: u32) -> &'static str {
    match discr {
        0 => "connection-refused",
        1 => "connection-reset",
        2 => "timed-out",
        3 => "host-not-found",
        4 => "io-error",
        5 => "not-permitted",
        6 => "invalid-handle",
        _ => "unknown-error",
    }
}

// -----------------------------------------------------------------------------
// TLS operations (unchanged for now)
// -----------------------------------------------------------------------------

/// Call the TLS upgrade import (synchronous - blocks until complete).
fn call_tls_upgrade(tcp_handle: u32, hostname: &str) -> Result<NetResult<u32>, String> {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let wit = wit
            .as_ref()
            .ok_or_else(|| "TLS upgrade called outside of execute context".to_string())?;

        let import_func = wit
            .get_import(Some("mcpone:host-sockets/tls@0.1.0"), "upgrade")
            .ok_or_else(|| "tls.upgrade import not found - networking not enabled".to_string())?;

        let mut cx = McponeCall::new();

        // Push arguments: hostname (string), then tcp_handle (u32) - reverse order
        cx.push_string(hostname.to_string());
        cx.push_u32(tcp_handle);

        // Synchronous call - blocks until the host completes the operation
        import_func.call_import_sync(&mut cx);

        // Result: result<tls-handle, tls-error>
        let is_ok = match cx.stack.pop() {
            Some(Value::ResultDiscriminant(v)) => v,
            other => return Err(format!("unexpected result discriminant: {other:?}")),
        };

        if is_ok {
            let handle = match cx.stack.pop() {
                Some(Value::U32(h)) => h,
                other => return Err(format!("unexpected handle value: {other:?}")),
            };
            Ok(NetResult::Ok(handle))
        } else {
            // TLS error is a variant - first discriminant tells us which
            let discr = match cx.stack.pop() {
                Some(Value::U32(d)) => d,
                other => return Err(format!("unexpected error discriminant: {other:?}")),
            };
            let payload = cx.stack.pop().and_then(|v| match v {
                Value::String(s) => Some(s),
                _ => None,
            });
            Ok(NetResult::Err(discr, payload))
        }
    })
}

/// Call the TLS read import (synchronous - blocks until complete).
fn call_tls_read(handle: u32, len: u32) -> Result<NetResult<Vec<u8>>, String> {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let wit = wit
            .as_ref()
            .ok_or_else(|| "TLS read called outside of execute context".to_string())?;

        let import_func = wit
            .get_import(Some("mcpone:host-sockets/tls@0.1.0"), "read")
            .ok_or_else(|| "tls.read import not found - networking not enabled".to_string())?;

        let mut cx = McponeCall::new();

        cx.push_u32(len);
        cx.push_u32(handle);

        // Synchronous call - blocks until the host completes the operation
        import_func.call_import_sync(&mut cx);

        let is_ok = match cx.stack.pop() {
            Some(Value::ResultDiscriminant(v)) => v,
            other => return Err(format!("unexpected result discriminant: {other:?}")),
        };

        if is_ok {
            let bytes = match cx.stack.pop() {
                Some(Value::Bytes(b)) => b,
                other => return Err(format!("unexpected bytes value: {other:?}")),
            };
            Ok(NetResult::Ok(bytes))
        } else {
            let discr = match cx.stack.pop() {
                Some(Value::U32(d)) => d,
                other => return Err(format!("unexpected error discriminant: {other:?}")),
            };
            let payload = cx.stack.pop().and_then(|v| match v {
                Value::String(s) => Some(s),
                _ => None,
            });
            Ok(NetResult::Err(discr, payload))
        }
    })
}

/// Call the TLS write import (synchronous - blocks until complete).
fn call_tls_write(handle: u32, data: &[u8]) -> Result<NetResult<u32>, String> {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let wit = wit
            .as_ref()
            .ok_or_else(|| "TLS write called outside of execute context".to_string())?;

        let import_func = wit
            .get_import(Some("mcpone:host-sockets/tls@0.1.0"), "write")
            .ok_or_else(|| "tls.write import not found - networking not enabled".to_string())?;

        let mut cx = McponeCall::new();

        cx.stack.push(Value::Bytes(data.to_vec()));
        cx.push_u32(handle);

        // Synchronous call - blocks until the host completes the operation
        import_func.call_import_sync(&mut cx);

        let is_ok = match cx.stack.pop() {
            Some(Value::ResultDiscriminant(v)) => v,
            other => return Err(format!("unexpected result discriminant: {other:?}")),
        };

        if is_ok {
            let written = match cx.stack.pop() {
                Some(Value::U32(n)) => n,
                other => return Err(format!("unexpected written value: {other:?}")),
            };
            Ok(NetResult::Ok(written))
        } else {
            let discr = match cx.stack.pop() {
                Some(Value::U32(d)) => d,
                other => return Err(format!("unexpected error discriminant: {other:?}")),
            };
            let payload = cx.stack.pop().and_then(|v| match v {
                Value::String(s) => Some(s),
                _ => None,
            });
            Ok(NetResult::Err(discr, payload))
        }
    })
}

/// Call the TLS close import (synchronous).
fn call_tls_close(handle: u32) {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let Some(wit) = wit.as_ref() else {
            return;
        };

        let import_func = match wit.get_import(Some("mcpone:host-sockets/tls@0.1.0"), "close") {
            Some(f) => f,
            None => return,
        };

        let mut cx = McponeCall::new();
        cx.push_u32(handle);
        import_func.call_import_sync(&mut cx);
    });
}

/// TLS error discriminant to error name mapping.
/// Note: TLS errors have different structure - 0 is tcp(tcp-error), others are TLS-specific.
fn tls_error_name(discr: u32) -> &'static str {
    match discr {
        0 => "tcp-error", // Wraps a tcp-error
        1 => "handshake-failed",
        2 => "certificate-error",
        3 => "invalid-handle",
        _ => "unknown-error",
    }
}

// Public network API used by python.rs pyfunction implementations

/// Value type for network results returned to Python.
#[derive(Debug)]
pub enum NetResultValue {
    /// Handle or bytes written count (u32).
    Handle(u32),
    /// Bytes read.
    Bytes(Vec<u8>),
    /// Error message.
    Error(String),
    /// Pending operation (waitable_id, promise_id).
    Pending(u32, u32),
}

/// Alias for backwards compatibility with existing code.
pub type TlsResultValue = NetResultValue;

// -----------------------------------------------------------------------------
// Public TCP API
// -----------------------------------------------------------------------------

/// Connect to a host:port over TCP.
/// Returns Ok((status, value)) where:
/// - status 0: success, value is handle (u32)
/// - status 1: error, value is error message (String)
/// - status 2: pending, value is (waitable_id, promise_id)
pub fn do_tcp_connect(host: &str, port: u16) -> Result<(i32, NetResultValue), String> {
    match call_tcp_connect(host, port)? {
        NetResult::Ok(handle) => Ok((0, NetResultValue::Handle(handle))),
        NetResult::Err(discr, payload) => {
            let error_name = tcp_error_name(discr);
            let msg = match payload {
                Some(p) => format!("{error_name}: {p}"),
                None => error_name.to_string(),
            };
            Ok((1, NetResultValue::Error(msg)))
        }
        NetResult::Pending(waitable, promise) => {
            Ok((2, NetResultValue::Pending(waitable, promise)))
        }
    }
}

/// Read from a TCP connection.
///
/// Returns `Ok((status, value))` where:
/// - status 0: success, value is bytes (`Vec<u8>`)
/// - status 1: error, value is error message (`String`)
/// - status 2: pending, value is (waitable_id, promise_id) (`(u32, u32)`)
pub fn do_tcp_read(handle: u32, len: u32) -> Result<(i32, NetResultValue), String> {
    match call_tcp_read(handle, len)? {
        NetResult::Ok(bytes) => Ok((0, NetResultValue::Bytes(bytes))),
        NetResult::Err(discr, payload) => {
            let error_name = tcp_error_name(discr);
            let msg = match payload {
                Some(p) => format!("{error_name}: {p}"),
                None => error_name.to_string(),
            };
            Ok((1, NetResultValue::Error(msg)))
        }
        NetResult::Pending(waitable, promise) => {
            Ok((2, NetResultValue::Pending(waitable, promise)))
        }
    }
}

/// Write to a TCP connection.
/// Returns Ok((status, value)) where:
/// - status 0: success, value is bytes written (u32)
/// - status 1: error, value is error message (String)
/// - status 2: pending, value is (waitable_id, promise_id)
pub fn do_tcp_write(handle: u32, data: &[u8]) -> Result<(i32, NetResultValue), String> {
    match call_tcp_write(handle, data)? {
        NetResult::Ok(written) => Ok((0, NetResultValue::Handle(written))),
        NetResult::Err(discr, payload) => {
            let error_name = tcp_error_name(discr);
            let msg = match payload {
                Some(p) => format!("{error_name}: {p}"),
                None => error_name.to_string(),
            };
            Ok((1, NetResultValue::Error(msg)))
        }
        NetResult::Pending(waitable, promise) => {
            Ok((2, NetResultValue::Pending(waitable, promise)))
        }
    }
}

/// Close a TCP connection (synchronous, no return value).
pub fn do_tcp_close(handle: u32) {
    call_tcp_close(handle);
}

// -----------------------------------------------------------------------------
// Public TLS API
// -----------------------------------------------------------------------------

/// Upgrade a TCP connection to TLS.
/// Returns Ok((status, value)) where:
/// - status 0: success, value is TLS handle (u32)
/// - status 1: error, value is error message (String)
/// - status 2: pending, value is (waitable_id, promise_id)
pub fn do_tls_upgrade(tcp_handle: u32, hostname: &str) -> Result<(i32, NetResultValue), String> {
    match call_tls_upgrade(tcp_handle, hostname)? {
        NetResult::Ok(handle) => Ok((0, NetResultValue::Handle(handle))),
        NetResult::Err(discr, payload) => {
            let error_name = tls_error_name(discr);
            let msg = match payload {
                Some(p) => format!("{error_name}: {p}"),
                None => error_name.to_string(),
            };
            Ok((1, NetResultValue::Error(msg)))
        }
        NetResult::Pending(waitable, promise) => {
            Ok((2, NetResultValue::Pending(waitable, promise)))
        }
    }
}

/// Read from a TLS connection.
///
/// Returns Ok((status, value)) where:
/// - status 0: success, value is bytes (`Vec<u8>`)
/// - status 1: error, value is error message (`String`)
/// - status 2: pending, value is (waitable_id, promise_id) (`(u32, u32)`)
pub fn do_tls_read(handle: u32, len: u32) -> Result<(i32, NetResultValue), String> {
    match call_tls_read(handle, len)? {
        NetResult::Ok(bytes) => Ok((0, NetResultValue::Bytes(bytes))),
        NetResult::Err(discr, payload) => {
            let error_name = tls_error_name(discr);
            let msg = match payload {
                Some(p) => format!("{error_name}: {p}"),
                None => error_name.to_string(),
            };
            Ok((1, NetResultValue::Error(msg)))
        }
        NetResult::Pending(waitable, promise) => {
            Ok((2, NetResultValue::Pending(waitable, promise)))
        }
    }
}

/// Write to a TLS connection.
/// Returns Ok((status, value)) where:
/// - status 0: success, value is bytes written (u32)
/// - status 1: error, value is error message (String)
/// - status 2: pending, value is (waitable_id, promise_id)
pub fn do_tls_write(handle: u32, data: &[u8]) -> Result<(i32, NetResultValue), String> {
    match call_tls_write(handle, data)? {
        NetResult::Ok(written) => Ok((0, NetResultValue::Handle(written))),
        NetResult::Err(discr, payload) => {
            let error_name = tls_error_name(discr);
            let msg = match payload {
                Some(p) => format!("{error_name}: {p}"),
                None => error_name.to_string(),
            };
            Ok((1, NetResultValue::Error(msg)))
        }
        NetResult::Pending(waitable, promise) => {
            Ok((2, NetResultValue::Pending(waitable, promise)))
        }
    }
}

/// Close a TLS connection (synchronous, no return value).
pub fn do_tls_close(handle: u32) {
    call_tls_close(handle);
}

// =============================================================================
// Host process execution wrappers (monocp:host/monocp@0.2.0)
// =============================================================================

const MONOCP_PROCESS_INTERFACE: &str = "monocp:host/monocp@0.2.0";

#[derive(Debug, Clone)]
enum ProcessFdTarget {
    Inherit,
    Close,
    PipeRead(u32),
    PipeWrite(u32),
    FileRead(String),
    FileWrite { path: String, append: bool },
    Dup(u32),
}

#[derive(Debug, Clone)]
struct ProcessFdBinding {
    fd: u32,
    target: ProcessFdTarget,
}

/// Stream handling mode used by the subprocess shim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubprocessStreamMode {
    /// Inherit the parent stream.
    Inherit,
    /// Capture the stream into a buffer.
    Pipe,
    /// Close/null-route the stream.
    Null,
    /// Redirect stderr to stdout (stderr mode only).
    Stdout,
}

impl SubprocessStreamMode {
    fn from_u32(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::Inherit),
            1 => Some(Self::Pipe),
            2 => Some(Self::Null),
            3 => Some(Self::Stdout),
            _ => None,
        }
    }
}

/// Output from a host-backed subprocess execution.
#[derive(Debug, Clone)]
pub struct SubprocessRunOutput {
    /// Process exit status.
    pub returncode: i32,
    /// Captured stdout bytes (empty when stdout is not captured).
    pub stdout: Vec<u8>,
    /// Captured stderr bytes (empty when stderr is not captured).
    pub stderr: Vec<u8>,
}

#[derive(Default)]
struct TempPathGuard {
    paths: Vec<std::path::PathBuf>,
}

impl TempPathGuard {
    fn push(&mut self, path: std::path::PathBuf) {
        self.paths.push(path);
    }
}

impl Drop for TempPathGuard {
    fn drop(&mut self) {
        for path in self.paths.drain(..) {
            let _ = std::fs::remove_file(path);
        }
    }
}

static NEXT_SUBPROCESS_TMP_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn resolve_working_directory(cwd: Option<&str>) -> Result<String, String> {
    let current = std::env::current_dir().map_err(|e| format!("failed to get current dir: {e}"))?;

    let resolved = match cwd {
        Some(path) if !path.is_empty() => {
            let p = std::path::PathBuf::from(path);
            if p.is_absolute() { p } else { current.join(p) }
        }
        _ => current,
    };

    Ok(resolved.to_string_lossy().into_owned())
}

fn make_temp_capture_path(base_dir: &std::path::Path, kind: &str) -> std::path::PathBuf {
    let id = NEXT_SUBPROCESS_TMP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    base_dir.join(format!(".mcpone-subprocess-{kind}-{ts}-{id}.tmp"))
}

fn can_write_temp_file_in_dir(dir: &std::path::Path) -> bool {
    if !dir.is_dir() {
        return false;
    }

    let probe = make_temp_capture_path(dir, "probe");
    match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

fn choose_temp_base_dir(working_dir: &str) -> Result<std::path::PathBuf, String> {
    let mut candidates = vec![std::path::PathBuf::from(working_dir)];

    if let Ok(current) = std::env::current_dir() {
        candidates.push(current);
    }

    candidates.push(std::path::PathBuf::from("/workspace"));
    candidates.push(std::path::PathBuf::from("/tmp"));

    for candidate in candidates {
        if can_write_temp_file_in_dir(&candidate) {
            return Ok(candidate);
        }
    }

    Err("could not find a writable temp directory for subprocess capture".to_string())
}

fn encode_string_list_arg(values: &[String]) -> Value {
    Value::ListArg(
        values
            .iter()
            .cloned()
            .map(|s| vec![Value::String(s)])
            .collect(),
    )
}

fn encode_fd_target_values(target: &ProcessFdTarget) -> Vec<Value> {
    match target {
        ProcessFdTarget::Inherit => vec![Value::U32(0)],
        ProcessFdTarget::Close => vec![Value::U32(1)],
        ProcessFdTarget::PipeRead(handle) => vec![Value::U32(*handle), Value::U32(2)],
        ProcessFdTarget::PipeWrite(handle) => vec![Value::U32(*handle), Value::U32(3)],
        ProcessFdTarget::FileRead(path) => vec![Value::String(path.clone()), Value::U32(4)],
        ProcessFdTarget::FileWrite { path, append } => {
            vec![
                Value::Bool(*append),
                Value::String(path.clone()),
                Value::U32(5),
            ]
        }
        ProcessFdTarget::Dup(fd) => vec![Value::U32(*fd), Value::U32(6)],
    }
}

fn encode_fd_bindings_arg(bindings: &[ProcessFdBinding]) -> Value {
    let encoded = bindings
        .iter()
        .map(|binding| {
            let mut values = encode_fd_target_values(&binding.target);
            // fd-binding record { fd, target } => push target first, then fd (reverse order)
            values.push(Value::U32(binding.fd));
            values
        })
        .collect();
    Value::ListArg(encoded)
}

fn call_process_spawn_with_context(
    argv: &[String],
    cwd: &str,
    fds: &[ProcessFdBinding],
) -> Result<Result<u64, String>, String> {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let wit = wit
            .as_ref()
            .ok_or_else(|| "spawn-with-context called outside of execute context".to_string())?;

        let import_func = wit
            .get_import(Some(MONOCP_PROCESS_INTERFACE), "spawn-with-context")
            .ok_or_else(|| "monocp spawn-with-context import not found".to_string())?;

        let mut cx = McponeCall::new();

        // Params are (argv, cwd, fds), push in reverse order.
        cx.stack.push(encode_fd_bindings_arg(fds));
        cx.push_string(cwd.to_string());
        cx.stack.push(encode_string_list_arg(argv));

        import_func.call_import_sync(&mut cx);

        let is_ok = pop_result_discriminant(&mut cx)?;
        if is_ok {
            let child = pop_u64_value(&mut cx, "spawn-with-context child-handle")?;
            Ok(Ok(child))
        } else {
            let msg = pop_string_value(&mut cx, "spawn-with-context error")?;
            Ok(Err(msg))
        }
    })
}

fn call_process_wait_child(child_handle: u64) -> Result<Result<i32, String>, String> {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let wit = wit
            .as_ref()
            .ok_or_else(|| "wait-child called outside of execute context".to_string())?;

        let import_func = wit
            .get_import(Some(MONOCP_PROCESS_INTERFACE), "wait-child")
            .ok_or_else(|| "monocp wait-child import not found".to_string())?;

        let mut cx = McponeCall::new();
        cx.push_u64(child_handle);
        import_func.call_import_sync(&mut cx);

        let is_ok = pop_result_discriminant(&mut cx)?;
        if is_ok {
            let status = pop_s32_value(&mut cx, "wait-child exit status")?;
            Ok(Ok(status))
        } else {
            let msg = pop_string_value(&mut cx, "wait-child error")?;
            Ok(Err(msg))
        }
    })
}

fn call_process_try_wait_child(child_handle: u64) -> Result<Option<i32>, String> {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let wit = wit
            .as_ref()
            .ok_or_else(|| "try-wait-child called outside of execute context".to_string())?;

        let import_func = wit
            .get_import(Some(MONOCP_PROCESS_INTERFACE), "try-wait-child")
            .ok_or_else(|| "monocp try-wait-child import not found".to_string())?;

        let mut cx = McponeCall::new();
        cx.push_u64(child_handle);
        import_func.call_import_sync(&mut cx);

        let is_some = pop_option_discriminant(&mut cx, "try-wait-child option")?;
        if is_some {
            let status = pop_s32_value(&mut cx, "try-wait-child exit status")?;
            Ok(Some(status))
        } else {
            Ok(None)
        }
    })
}

fn call_process_close_child(child_handle: u64) {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let Some(wit) = wit.as_ref() else {
            return;
        };

        let Some(import_func) = wit.get_import(Some(MONOCP_PROCESS_INTERFACE), "close-child")
        else {
            return;
        };

        let mut cx = McponeCall::new();
        cx.push_u64(child_handle);
        import_func.call_import_sync(&mut cx);
    });
}

fn call_process_kill_child(child_handle: u64) -> Result<Result<(), String>, String> {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let wit = wit
            .as_ref()
            .ok_or_else(|| "kill-child called outside of execute context".to_string())?;

        let import_func = wit
            .get_import(Some(MONOCP_PROCESS_INTERFACE), "kill-child")
            .ok_or_else(|| "monocp kill-child import not found".to_string())?;

        let mut cx = McponeCall::new();
        cx.push_u64(child_handle);
        import_func.call_import_sync(&mut cx);

        let is_ok = pop_result_discriminant(&mut cx)?;
        if is_ok {
            Ok(Ok(()))
        } else {
            let msg = pop_string_value(&mut cx, "kill-child error")?;
            Ok(Err(msg))
        }
    })
}

fn read_file_or_empty(path: &std::path::Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_default()
}

fn wait_for_child(child_handle: u64, timeout_secs: Option<f64>) -> Result<i32, String> {
    if let Some(timeout) = timeout_secs {
        if timeout.is_sign_negative() {
            return Err("timeout must be >= 0".to_string());
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(timeout);
        loop {
            if let Some(status) = call_process_try_wait_child(child_handle)? {
                return Ok(status);
            }

            if std::time::Instant::now() >= deadline {
                let _ = call_process_kill_child(child_handle);
                return Err(format!("process timed out after {timeout} seconds"));
            }

            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    match call_process_wait_child(child_handle)? {
        Ok(status) => Ok(status),
        Err(msg) => Err(msg),
    }
}

/// Execute a subprocess through monocp host process APIs.
///
/// Stream mode mapping:
/// - 0 = inherit
/// - 1 = capture/pipe
/// - 2 = null/close
/// - 3 = stderr->stdout only (for stderr mode)
pub fn do_subprocess_run(
    argv: &[String],
    cwd: Option<&str>,
    stdin_mode_raw: u32,
    stdout_mode_raw: u32,
    stderr_mode_raw: u32,
    stdin_data: Option<&[u8]>,
    timeout_secs: Option<f64>,
) -> Result<SubprocessRunOutput, String> {
    if argv.is_empty() {
        return Err("subprocess argv must not be empty".to_string());
    }

    let stdin_mode = SubprocessStreamMode::from_u32(stdin_mode_raw)
        .ok_or_else(|| format!("invalid stdin mode: {stdin_mode_raw}"))?;
    let stdout_mode = SubprocessStreamMode::from_u32(stdout_mode_raw)
        .ok_or_else(|| format!("invalid stdout mode: {stdout_mode_raw}"))?;
    let stderr_mode = SubprocessStreamMode::from_u32(stderr_mode_raw)
        .ok_or_else(|| format!("invalid stderr mode: {stderr_mode_raw}"))?;

    if matches!(stdin_mode, SubprocessStreamMode::Stdout) {
        return Err("stdin does not support STDOUT mode".to_string());
    }
    if matches!(stdout_mode, SubprocessStreamMode::Stdout) {
        return Err("stdout does not support STDOUT mode".to_string());
    }
    if stdin_data.is_some() && !matches!(stdin_mode, SubprocessStreamMode::Pipe) {
        return Err("input data requires stdin=PIPE".to_string());
    }

    let working_dir = resolve_working_directory(cwd)?;

    let temp_base = choose_temp_base_dir(&working_dir)?;
    let mut temp_guard = TempPathGuard::default();

    let mut stdout_capture_path: Option<std::path::PathBuf> = None;
    let mut stderr_capture_path: Option<std::path::PathBuf> = None;

    let stdin_target = match stdin_mode {
        SubprocessStreamMode::Inherit => ProcessFdTarget::Inherit,
        SubprocessStreamMode::Null => ProcessFdTarget::Close,
        SubprocessStreamMode::Pipe => {
            if let Some(data) = stdin_data {
                let input_path = make_temp_capture_path(&temp_base, "stdin");
                std::fs::write(&input_path, data)
                    .map_err(|e| format!("failed to write stdin temp file: {e}"))?;
                temp_guard.push(input_path.clone());
                ProcessFdTarget::FileRead(input_path.to_string_lossy().into_owned())
            } else {
                // stdin=PIPE without explicit input: present immediate EOF
                ProcessFdTarget::Close
            }
        }
        SubprocessStreamMode::Stdout => ProcessFdTarget::Close,
    };

    let stdout_target = match stdout_mode {
        SubprocessStreamMode::Inherit => ProcessFdTarget::Inherit,
        SubprocessStreamMode::Null => ProcessFdTarget::Close,
        SubprocessStreamMode::Pipe => {
            let path = make_temp_capture_path(&temp_base, "stdout");
            temp_guard.push(path.clone());
            stdout_capture_path = Some(path.clone());
            ProcessFdTarget::FileWrite {
                path: path.to_string_lossy().into_owned(),
                append: false,
            }
        }
        SubprocessStreamMode::Stdout => ProcessFdTarget::Inherit,
    };

    let stderr_target = match stderr_mode {
        SubprocessStreamMode::Inherit => ProcessFdTarget::Inherit,
        SubprocessStreamMode::Null => ProcessFdTarget::Close,
        SubprocessStreamMode::Pipe => {
            let path = make_temp_capture_path(&temp_base, "stderr");
            temp_guard.push(path.clone());
            stderr_capture_path = Some(path.clone());
            ProcessFdTarget::FileWrite {
                path: path.to_string_lossy().into_owned(),
                append: false,
            }
        }
        SubprocessStreamMode::Stdout => ProcessFdTarget::Dup(1),
    };

    let bindings = vec![
        ProcessFdBinding {
            fd: 0,
            target: stdin_target,
        },
        ProcessFdBinding {
            fd: 1,
            target: stdout_target,
        },
        ProcessFdBinding {
            fd: 2,
            target: stderr_target,
        },
    ];

    let child_handle = match call_process_spawn_with_context(argv, &working_dir, &bindings)? {
        Ok(handle) => handle,
        Err(msg) => return Err(format!("spawn-with-context failed: {msg}")),
    };

    let wait_result = wait_for_child(child_handle, timeout_secs);
    call_process_close_child(child_handle);

    let returncode = wait_result?;

    let stdout = stdout_capture_path
        .as_ref()
        .map_or_else(Vec::new, |path| read_file_or_empty(path));
    let stderr = stderr_capture_path
        .as_ref()
        .map_or_else(Vec::new, |path| read_file_or_empty(path));

    Ok(SubprocessRunOutput {
        returncode,
        stdout,
        stderr,
    })
}

// =============================================================================
// Async state storage
// =============================================================================
//
// When an async export is pending (waiting for a callback), we store the call
// context and task_return function so we can resume and complete when the
// callback finishes.

/// Type for the task_return callback function.
type TaskReturnFn = unsafe extern "C" fn(*mut std::ffi::c_void);

/// Stored state for a pending async export.
struct PendingAsyncState {
    cx: Box<McponeCall>,
    task_return: Option<TaskReturnFn>,
    /// The Wit handle needed for subsequent callbacks.
    /// We need to restore this when resuming async execution.
    wit: Wit,
}

// Thread-local storage for pending async state.
// WASM is single-threaded, so only one export can be pending at a time.
std::thread_local! {
    static PENDING_ASYNC_STATE: std::cell::RefCell<Option<PendingAsyncState>> =
        const { std::cell::RefCell::new(None) };
}

/// Result of handling an export call.
enum HandleExportResult {
    /// Export completed - result is on the stack.
    Complete,
    /// Export is pending - waiting for async callback.
    /// Contains the raw callback code (WAIT | waitable_set << 4) to return to the host.
    Pending(u32),
}

fn extract_code_from_python_cli_args(argv: &[String]) -> Result<CliRunInput, String> {
    if argv.len() == 2 && argv[1] == "-c" {
        return Err("python wasi:cli/run requires a code string after -c".to_string());
    }

    if argv.len() >= 3 && argv[1] == "-c" {
        return Ok(CliRunInput {
            code: argv[2].clone(),
            filename: None,
        });
    }

    if argv.len() >= 2 && !argv[1].starts_with('-') {
        let path = argv[1].clone();
        let code = std::fs::read_to_string(&path)
            .map_err(|err| format!("Failed to read python script '{path}': {err}"))?;

        return Ok(CliRunInput {
            code,
            filename: Some(path),
        });
    }

    // No -c and no script file: read code from stdin.
    // Handles both bare `python` and `python -` (explicit stdin marker, used
    // by shells with heredocs like `python - <<'PY' ... PY`).
    if argv.len() == 1 || (argv.len() >= 2 && argv[1] == "-") {
        use std::io::Read;
        let mut code = String::new();
        std::io::stdin()
            .read_to_string(&mut code)
            .map_err(|err| format!("Failed to read Python code from stdin: {err}"))?;
        if code.trim().is_empty() {
            return Err("no input: pass `-c <code>`, a script path, or pipe code to stdin".to_string());
        }
        return Ok(CliRunInput {
            code,
            filename: Some("<stdin>".to_string()),
        });
    }

    Err("python wasi:cli/run currently supports `-c <code>`, `python <script.py>`, or piped stdin".to_string())
}

#[derive(Debug, Clone)]
struct CliRunInput {
    /// Python source code to execute.
    code: String,
    /// Optional filename for compile metadata and `__file__`.
    filename: Option<String>,
}

fn handle_wasi_cli_run(wit: Wit, cx: &mut McponeCall) -> HandleExportResult {
    let argv: Vec<String> = std::env::args().collect();
    let input = match extract_code_from_python_cli_args(&argv) {
        Ok(input) => input,
        Err(message) => {
            eprintln!("{}", message);
            cx.stack.push(Value::ResultDiscriminant(false));
            return HandleExportResult::Complete;
        }
    };

    // Delegate to the same execution path used by the execute export.
    let result = with_wit(wit, || match input.filename.as_deref() {
        Some(filename) => python::execute_python_with_filename(&input.code, filename),
        None => python::execute_python(&input.code),
    });
    match result {
        python::ExecuteResult::Complete(output) => {
            use std::io::Write;

            if !output.stdout.is_empty() {
                let mut out = std::io::stdout();
                let _ = out.write_all(output.stdout.as_bytes());
                let _ = out.flush();
            }
            if !output.stderr.is_empty() {
                let mut err = std::io::stderr();
                let _ = err.write_all(output.stderr.as_bytes());
                let _ = err.flush();
            }
            // wasi:cli/run.run returns result<_, _>
            cx.stack.push(Value::ResultDiscriminant(true));
            HandleExportResult::Complete
        }
        python::ExecuteResult::Error(error) => {
            if !error.is_empty() {
                eprintln!("{}", error);
            }
            cx.stack.push(Value::ResultDiscriminant(false));
            HandleExportResult::Complete
        }
        python::ExecuteResult::Pending(_callback_code) => {
            // Keep run command-style behavior synchronous for now.
            eprintln!("python wasi:cli/run currently does not support async callback suspension");
            cx.stack.push(Value::ResultDiscriminant(false));
            HandleExportResult::Complete
        }
    }
}

/// Handle an export call by dispatching to the appropriate Python function.
/// Returns whether the export completed or is pending.
fn handle_export(wit: Wit, func: ExportFunction, cx: &mut McponeCall) -> HandleExportResult {
    match resolve_export_kind(func) {
        Some(ExportKind::Execute) => {
            // execute(code: string) -> result<string, string>
            let code = cx.pop_string().to_string();

            // Execute Python with Wit handle available for host imports
            let result = with_wit(wit, || python::execute_python(&code));

            match result {
                python::ExecuteResult::Complete(output) => {
                    // Push record fields in REVERSE order for LIFO stack
                    // WIT defines: execute-output { stdout, stderr }
                    // wit-dylib pops fields in definition order, so push stderr first, then stdout
                    cx.push_string(output.stderr);
                    cx.push_string(output.stdout);
                    cx.stack.push(Value::ResultDiscriminant(true));
                    HandleExportResult::Complete
                }
                python::ExecuteResult::Error(error) => {
                    cx.push_string(error);
                    cx.stack.push(Value::ResultDiscriminant(false));
                    HandleExportResult::Complete
                }
                python::ExecuteResult::Pending(callback_code) => {
                    // Execution is suspended, waiting for async callback
                    HandleExportResult::Pending(callback_code)
                }
            }
        }
        Some(ExportKind::SnapshotState) => {
            // snapshot-state() -> result<list<u8>, string>
            match python::snapshot_state() {
                Ok(state) => {
                    cx.stack.push(Value::Bytes(state));
                    cx.stack.push(Value::ResultDiscriminant(true));
                }
                Err(error) => {
                    cx.push_string(error);
                    cx.stack.push(Value::ResultDiscriminant(false));
                }
            }
            HandleExportResult::Complete
        }
        Some(ExportKind::RestoreState) => {
            // restore-state(data: list<u8>) -> result<_, string>
            let data = match cx.stack.pop() {
                Some(Value::Bytes(bytes)) => bytes,
                other => panic!("expected Bytes for restore_state data, got {other:?}"),
            };

            match python::restore_state(&data) {
                Ok(()) => {
                    cx.stack.push(Value::ResultDiscriminant(true));
                }
                Err(error) => {
                    cx.push_string(error);
                    cx.stack.push(Value::ResultDiscriminant(false));
                }
            }
            HandleExportResult::Complete
        }
        Some(ExportKind::ClearState) => {
            // clear-state() - no return value
            python::clear_state();
            HandleExportResult::Complete
        }
        Some(ExportKind::FinalizePreinit) => {
            // finalize-preinit() - no return value
            // Reset WASI state after all imports are done during pre-init
            python::finalize_preinit();
            HandleExportResult::Complete
        }
        Some(ExportKind::WasiCliRun) => handle_wasi_cli_run(wit, cx),
        None => {
            panic!(
                "unknown export function interface={:?} name={}",
                func.interface(),
                func.name()
            );
        }
    }
}

impl Interpreter for McponeInterpreter {
    type CallCx<'a> = McponeCall;

    fn initialize(_wit: Wit) {
        python::initialize_python();
    }

    fn export_start<'a>(_wit: Wit, _func: ExportFunction) -> Box<Self::CallCx<'a>> {
        Box::new(McponeCall::new())
    }

    fn export_call(wit: Wit, func: ExportFunction, cx: &mut Self::CallCx<'_>) {
        // Synchronous exports don't support pending - must complete
        let result = handle_export(wit, func, cx);
        if let HandleExportResult::Pending(_) = result {
            panic!("synchronous export cannot return pending");
        }
    }

    fn export_async_start(
        wit: Wit,
        func: ExportFunction,
        mut cx: Box<Self::CallCx<'static>>,
    ) -> u32 {
        let result = handle_export(wit, func, &mut cx);

        match result {
            HandleExportResult::Complete => {
                // Call task_return to signal async completion.
                if let Some(task_return) = func.task_return() {
                    unsafe {
                        task_return(Box::into_raw(cx).cast());
                    }
                }
                0
            }
            HandleExportResult::Pending(callback_code) => {
                // Store state for later resumption when callback completes
                PENDING_ASYNC_STATE.with(|cell| {
                    *cell.borrow_mut() = Some(PendingAsyncState {
                        cx,
                        task_return: func.task_return(),
                        wit,
                    });
                });
                // Return the callback code (WAIT | waitable_set << 4) to the host
                callback_code
            }
        }
    }

    fn export_async_callback(event0: u32, event1: u32, event2: u32) -> u32 {
        // Event types from Component Model (used by _mcpone_async)
        const EVENT_SUBTASK: u32 = 1;
        // Status types
        const STATUS_RETURNED: u32 = 2;

        // Get the Wit handle from pending state (without taking ownership yet).
        // We need this to restore CURRENT_WIT and callbacks so subsequent invoke() calls work.
        let wit = PENDING_ASYNC_STATE.with(|cell| cell.borrow().as_ref().map(|s| s.wit));
        let wit = match wit {
            Some(w) => w,
            None => return 0, // No pending state - shouldn't happen
        };

        // If this is a SUBTASK event with RETURNED status, lift the async import result
        // before calling Python's callback so promise_get_result can access it
        if event0 == EVENT_SUBTASK && event2 == STATUS_RETURNED {
            let subtask = event1;
            if let Some(pending_state) =
                PENDING_IMPORTS.with(|cell| cell.borrow_mut().remove(&subtask))
            {
                // Create a call context to receive the lifted result
                let mut cx = McponeCall::new();

                // Lift the result from the buffer onto the call stack
                // Safety: async_lift_impl and buffer were set by call_import_async
                unsafe {
                    (pending_state.async_lift_impl)(
                        (&raw mut cx).cast(),
                        pending_state.buffer.cast(),
                    );
                }

                // Handle result based on import type.
                // Note: Only `invoke` uses Component Model async. TCP/TLS operations
                // now use fiber-based async and complete synchronously from the guest's
                // perspective, so they don't appear as pending imports.
                match pending_state.import_type {
                    ImportType::Invoke => {
                        // result<string, string>
                        let is_ok = match cx.stack.pop() {
                            Some(Value::ResultDiscriminant(v)) => v,
                            _ => true,
                        };
                        let result_value = match cx.stack.pop() {
                            Some(Value::String(s)) => s,
                            _ => String::new(),
                        };
                        let result_json = if is_ok {
                            format!(r#"{{"ok": true, "value": {}}}"#, result_value)
                        } else {
                            // Error message needs to be JSON-encoded (with quotes and escaping)
                            let escaped = python::escape_json_string(&result_value);
                            format!(r#"{{"ok": false, "error": "{}"}}"#, escaped)
                        };
                        python::set_async_import_result(subtask, &result_json);
                    }
                }
            }
        }

        // Call Python's callback function to resume execution.
        // Restore CURRENT_WIT and callbacks so any subsequent invoke() calls work.
        let callback_code = with_wit(wit, || python::call_python_callback(event0, event1, event2));
        let code = python::callback_code::get_code(callback_code);

        if code == python::callback_code::WAIT {
            // Still pending - return the full callback code (WAIT | waitable_set << 4)
            return callback_code;
        }

        // Execution complete (EXIT) - capture output and call task_return
        PENDING_ASYNC_STATE.with(|cell| {
            if let Some(state) = cell.borrow_mut().take() {
                let mut cx = state.cx;

                // Re-capture stdout now that the async execution is complete
                // The original teardown ran before we suspended, so it captured nothing
                python::recapture_stdout();

                // Check if there was an uncaught Python exception during the callback
                if let Some(error) = python::take_last_callback_error() {
                    // Push error result
                    cx.push_string(error);
                    cx.stack.push(Value::ResultDiscriminant(false));
                } else {
                    // Get the output from Python (both stdout and stderr)
                    let stdout = unsafe {
                        python::get_python_variable_string("_mcpone_output").unwrap_or_default()
                    };
                    let stderr = unsafe {
                        python::get_python_variable_string("_mcpone_errors").unwrap_or_default()
                    };

                    // Push record fields in REVERSE order for LIFO stack (stderr first, then stdout)
                    cx.push_string(stderr.trim_end_matches('\n').to_string());
                    cx.push_string(stdout.trim_end_matches('\n').to_string());
                    cx.stack.push(Value::ResultDiscriminant(true));
                }

                // Call task_return
                if let Some(task_return) = state.task_return {
                    unsafe {
                        task_return(Box::into_raw(cx).cast());
                    }
                }
            }
        });

        0
    }

    fn resource_dtor(_ty: Resource, _handle: usize) {
        // No resources to clean up for now
    }
}

// Export the interpreter interface
mod export {
    #![allow(missing_docs)]
    #![allow(clippy::not_unsafe_ptr_arg_deref)]
    use super::McponeInterpreter;
    wit_dylib_ffi::export!(McponeInterpreter);
}
