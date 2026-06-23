//! Plain-core-import networking/TLS for the THREADED (preview1) CPython core
//! module.
//!
//! In the shipped preview2 build the guest bridge reaches the host through the
//! wit-dylib component machinery: TCP/DNS go via `wasi:sockets/*` and TLS via
//! the `mcpone:host-sockets/tls@0.1.0` component import, dispatched by string
//! lookup (`Wit::get_import` -> a function pointer the wit-dylib bindings fill
//! in at component-instantiation time). That machinery only exists inside a
//! COMPONENT instantiated by those bindings.
//!
//! A wasi-threads CPython build is a plain CORE MODULE (no component, no
//! wit-dylib bindings). So under the `core-net` feature we bind networking to
//! PLAIN CORE IMPORTS in module `monocp` instead -- exactly the surface the
//! preview1 host registers (see backend .../preview1/host_core.go:
//! `monocp.tcp_*`, `monocp.dns_*`, `monocp.tls_*`). Each import is an
//! errno-returning function that reads/writes guest linear memory through raw
//! pointers, mirroring the WASI preview1 ABI.
//!
//! The public `do_tcp_*` / `do_tls_*` surface in `lib.rs` is unchanged; only the
//! private `call_*` layer is swapped for this implementation when `core-net`
//! is enabled. `lib.rs` delegates to the functions here.

use crate::NetResult;

// -----------------------------------------------------------------------------
// Plain core imports (module "monocp"), matching the preview1 host ABI.
//
// All functions return a WASI errno (0 == success). Outputs are written through
// guest-memory pointers. The sockaddr layout is 32 bytes, little-endian:
//   [0..2)  family (u16): 2 = IPv4, 10 = IPv6
//   [2..4)  port   (u16)
//   [4..20) ip     (16 bytes; IPv4 uses the first 4)
//   [20..24) scope (u32)
// (see backend .../preview1/runtime/sockaddr.go)
// -----------------------------------------------------------------------------

#[link(wasm_import_module = "monocp")]
unsafe extern "C" {
    // net_instance(out_handle_ptr) -> errno
    #[link_name = "net_instance"]
    fn monocp_net_instance(out_handle_ptr: *mut u32) -> i32;

    // tcp_create(family, out_handle_ptr) -> errno
    #[link_name = "tcp_create"]
    fn monocp_tcp_create(family: i32, out_handle_ptr: *mut u32) -> i32;

    // tcp_start_connect(socket_handle, network_handle, addr_ptr, addr_len) -> errno
    #[link_name = "tcp_start_connect"]
    fn monocp_tcp_start_connect(
        socket_handle: u32,
        network_handle: u32,
        addr_ptr: *const u8,
        addr_len: u32,
    ) -> i32;

    // tcp_finish_connect(socket_handle) -> errno
    #[link_name = "tcp_finish_connect"]
    fn monocp_tcp_finish_connect(socket_handle: u32) -> i32;

    // tcp_read(socket_handle, buf_ptr, buf_len, nread_ptr) -> errno
    #[link_name = "tcp_read"]
    fn monocp_tcp_read(
        socket_handle: u32,
        buf_ptr: *mut u8,
        buf_len: u32,
        nread_ptr: *mut u32,
    ) -> i32;

    // tcp_write(socket_handle, buf_ptr, buf_len, nwritten_ptr) -> errno
    #[link_name = "tcp_write"]
    fn monocp_tcp_write(
        socket_handle: u32,
        buf_ptr: *const u8,
        buf_len: u32,
        nwritten_ptr: *mut u32,
    ) -> i32;

    // tcp_close(socket_handle) -> errno
    #[link_name = "tcp_close"]
    fn monocp_tcp_close(socket_handle: u32) -> i32;

    // dns_resolve(network_handle, name_ptr, name_len, out_handle_ptr) -> errno
    #[link_name = "dns_resolve"]
    fn monocp_dns_resolve(
        network_handle: u32,
        name_ptr: *const u8,
        name_len: u32,
        out_handle_ptr: *mut u32,
    ) -> i32;

    // dns_resolve_next(resolver_handle, addr_ptr, done_ptr) -> errno
    #[link_name = "dns_resolve_next"]
    fn monocp_dns_resolve_next(resolver_handle: u32, addr_ptr: *mut u8, done_ptr: *mut u32) -> i32;

    // dns_close(resolver_handle) -> errno
    #[link_name = "dns_close"]
    fn monocp_dns_close(resolver_handle: u32) -> i32;

    // tls_upgrade(hostname_ptr, hostname_len, network_handle, out_handle_ptr) -> errno
    #[link_name = "tls_upgrade"]
    fn monocp_tls_upgrade(
        hostname_ptr: *const u8,
        hostname_len: u32,
        network_handle: u32,
        out_handle_ptr: *mut u32,
    ) -> i32;

    // tls_read(handle, buf_ptr, buf_len, nread_ptr) -> errno
    #[link_name = "tls_read"]
    fn monocp_tls_read(handle: u32, buf_ptr: *mut u8, buf_len: u32, nread_ptr: *mut u32) -> i32;

    // tls_write(handle, buf_ptr, buf_len, nwritten_ptr) -> errno
    #[link_name = "tls_write"]
    fn monocp_tls_write(
        handle: u32,
        buf_ptr: *const u8,
        buf_len: u32,
        nwritten_ptr: *mut u32,
    ) -> i32;

    // tls_close(handle) -> errno
    #[link_name = "tls_close"]
    fn monocp_tls_close(handle: u32) -> i32;
}

const SOCKADDR_SIZE: usize = 32;
const ADDR_FAMILY_IPV4: u16 = 2;
const ADDR_FAMILY_IPV6: u16 = 10;

// WASI errno values used for net error classification.
const ERRNO_SUCCESS: i32 = 0;

// NetResult error discriminants (must match tcp_error_name / tls_error_name in lib.rs).
const TCP_ERR_HOST_NOT_FOUND: u32 = 3;
const TCP_ERR_IO: u32 = 4;
const TCP_ERR_NOT_PERMITTED: u32 = 5;
const TCP_ERR_INVALID_HANDLE: u32 = 6;
const TLS_ERR_HANDSHAKE_FAILED: u32 = 1;
const TLS_ERR_INVALID_HANDLE: u32 = 3;

/// Map a WASI errno from a monocp.* import into a TCP NetResult error
/// discriminant, mirroring the spirit of map_wasi_socket_error_to_tcp in lib.rs.
fn errno_to_tcp_err(errno: i32) -> (u32, Option<String>) {
    // wasiErrnoNotcapable == 76 (permission denied by policy).
    match errno {
        76 => (TCP_ERR_NOT_PERMITTED, Some("not-permitted".to_string())),
        8 => (TCP_ERR_INVALID_HANDLE, Some("bad-handle".to_string())), // EBADF
        _ => (TCP_ERR_IO, Some(format!("io-error (errno {errno})"))),
    }
}

/// Encode a 32-byte sockaddr from a resolver-produced address blob, overriding
/// the port (dns_resolve_next always returns port 0).
fn sockaddr_with_port(addr: &[u8; SOCKADDR_SIZE], port: u16) -> [u8; SOCKADDR_SIZE] {
    let mut out = *addr;
    out[2..4].copy_from_slice(&port.to_le_bytes());
    out
}

// -----------------------------------------------------------------------------
// Network handle (cached). monocp.net_instance returns the host's default
// network; cache it per-thread to avoid repeated calls.
// -----------------------------------------------------------------------------

std::thread_local! {
    static CORE_NETWORK_HANDLE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn network_handle() -> Result<u32, String> {
    CORE_NETWORK_HANDLE.with(|cell| {
        let cached = cell.get();
        if cached != 0 {
            return Ok(cached);
        }
        let mut handle: u32 = 0;
        let errno = unsafe { monocp_net_instance(&mut handle as *mut u32) };
        if errno != ERRNO_SUCCESS {
            return Err(format!("monocp.net_instance failed (errno {errno})"));
        }
        cell.set(handle);
        Ok(handle)
    })
}

// -----------------------------------------------------------------------------
// TCP
// -----------------------------------------------------------------------------

/// Connect to host:port over TCP via the monocp.* plain imports:
/// net_instance -> dns_resolve(host) -> tcp_create -> tcp_start_connect ->
/// tcp_finish_connect. Returns the socket handle (used directly by read/write).
pub fn call_tcp_connect(host: &str, port: u16) -> Result<NetResult<u32>, String> {
    let network = network_handle()?;

    // Resolve the hostname to one or more addresses through the host resolver.
    let mut resolver: u32 = 0;
    let errno = unsafe {
        monocp_dns_resolve(
            network,
            host.as_ptr(),
            host.len() as u32,
            &mut resolver as *mut u32,
        )
    };
    if errno != ERRNO_SUCCESS {
        let (discr, payload) = errno_to_tcp_err(errno);
        // A resolve denied by policy reads more naturally as host-not-found
        // unless it was explicitly not-permitted.
        let discr = if discr == TCP_ERR_IO {
            TCP_ERR_HOST_NOT_FOUND
        } else {
            discr
        };
        return Ok(NetResult::Err(discr, payload));
    }

    // Walk the resolver's addresses, trying each until one connects.
    let mut last_err: (u32, Option<String>) = (TCP_ERR_HOST_NOT_FOUND, None);
    loop {
        let mut addr = [0u8; SOCKADDR_SIZE];
        let mut done: u32 = 0;
        let errno =
            unsafe { monocp_dns_resolve_next(resolver, addr.as_mut_ptr(), &mut done as *mut u32) };
        if errno != ERRNO_SUCCESS {
            last_err = errno_to_tcp_err(errno);
            break;
        }
        if done != 0 {
            break;
        }

        let family = u16::from_le_bytes([addr[0], addr[1]]);
        let family_arg = if family == ADDR_FAMILY_IPV6 {
            ADDR_FAMILY_IPV6 as i32
        } else {
            ADDR_FAMILY_IPV4 as i32
        };

        // Create a socket of the matching family.
        let mut socket: u32 = 0;
        let errno = unsafe { monocp_tcp_create(family_arg, &mut socket as *mut u32) };
        if errno != ERRNO_SUCCESS {
            last_err = errno_to_tcp_err(errno);
            continue;
        }

        // Encode the destination sockaddr with the requested port.
        let dest = sockaddr_with_port(&addr, port);
        let errno = unsafe {
            monocp_tcp_start_connect(socket, network, dest.as_ptr(), SOCKADDR_SIZE as u32)
        };
        if errno != ERRNO_SUCCESS {
            last_err = errno_to_tcp_err(errno);
            unsafe {
                let _ = monocp_tcp_close(socket);
            }
            continue;
        }

        let errno = unsafe { monocp_tcp_finish_connect(socket) };
        if errno != ERRNO_SUCCESS {
            last_err = errno_to_tcp_err(errno);
            unsafe {
                let _ = monocp_tcp_close(socket);
            }
            continue;
        }

        // Connected. Close the resolver and return the socket handle.
        unsafe {
            let _ = monocp_dns_close(resolver);
        }
        return Ok(NetResult::Ok(socket));
    }

    unsafe {
        let _ = monocp_dns_close(resolver);
    }
    Ok(NetResult::Err(last_err.0, last_err.1))
}

/// Read up to `len` bytes from a TCP socket handle.
pub fn call_tcp_read(handle: u32, len: u32) -> Result<NetResult<Vec<u8>>, String> {
    let mut buf = vec![0u8; len as usize];
    let mut nread: u32 = 0;
    let errno = unsafe { monocp_tcp_read(handle, buf.as_mut_ptr(), len, &mut nread as *mut u32) };
    if errno != ERRNO_SUCCESS {
        let (discr, payload) = errno_to_tcp_err(errno);
        return Ok(NetResult::Err(discr, payload));
    }
    buf.truncate(nread as usize);
    Ok(NetResult::Ok(buf))
}

/// Write bytes to a TCP socket handle.
pub fn call_tcp_write(handle: u32, data: &[u8]) -> Result<NetResult<u32>, String> {
    let mut nwritten: u32 = 0;
    let errno = unsafe {
        monocp_tcp_write(
            handle,
            data.as_ptr(),
            data.len() as u32,
            &mut nwritten as *mut u32,
        )
    };
    if errno != ERRNO_SUCCESS {
        let (discr, payload) = errno_to_tcp_err(errno);
        return Ok(NetResult::Err(discr, payload));
    }
    Ok(NetResult::Ok(nwritten))
}

/// Close a TCP socket handle.
pub fn call_tcp_close(handle: u32) {
    unsafe {
        let _ = monocp_tcp_close(handle);
    }
}

// -----------------------------------------------------------------------------
// TLS
// -----------------------------------------------------------------------------

/// Upgrade to TLS. The preview1 host's tls_upgrade RE-DIALS by hostname and
/// ignores any tcp handle (matching preview2's tls.upgrade), so we pass the
/// hostname directly and discard `tcp_handle`.
pub fn call_tls_upgrade(_tcp_handle: u32, hostname: &str) -> Result<NetResult<u32>, String> {
    let network = network_handle()?;
    let mut handle: u32 = 0;
    let errno = unsafe {
        monocp_tls_upgrade(
            hostname.as_ptr(),
            hostname.len() as u32,
            network,
            &mut handle as *mut u32,
        )
    };
    if errno != ERRNO_SUCCESS {
        // Map errno to a TLS error discriminant.
        let (discr, payload) = match errno {
            76 => (TLS_ERR_HANDSHAKE_FAILED, Some("not-permitted".to_string())),
            _ => (
                TLS_ERR_HANDSHAKE_FAILED,
                Some(format!("tls upgrade failed (errno {errno})")),
            ),
        };
        return Ok(NetResult::Err(discr, payload));
    }
    Ok(NetResult::Ok(handle))
}

/// Read up to `len` bytes from a TLS connection handle.
pub fn call_tls_read(handle: u32, len: u32) -> Result<NetResult<Vec<u8>>, String> {
    let mut buf = vec![0u8; len as usize];
    let mut nread: u32 = 0;
    let errno = unsafe { monocp_tls_read(handle, buf.as_mut_ptr(), len, &mut nread as *mut u32) };
    if errno != ERRNO_SUCCESS {
        return Ok(NetResult::Err(
            TLS_ERR_INVALID_HANDLE,
            Some(format!("tls read failed (errno {errno})")),
        ));
    }
    buf.truncate(nread as usize);
    Ok(NetResult::Ok(buf))
}

/// Write bytes to a TLS connection handle.
pub fn call_tls_write(handle: u32, data: &[u8]) -> Result<NetResult<u32>, String> {
    let mut nwritten: u32 = 0;
    let errno = unsafe {
        monocp_tls_write(
            handle,
            data.as_ptr(),
            data.len() as u32,
            &mut nwritten as *mut u32,
        )
    };
    if errno != ERRNO_SUCCESS {
        return Ok(NetResult::Err(
            TLS_ERR_INVALID_HANDLE,
            Some(format!("tls write failed (errno {errno})")),
        ));
    }
    Ok(NetResult::Ok(nwritten))
}

/// Close a TLS connection handle.
pub fn call_tls_close(handle: u32) {
    unsafe {
        let _ = monocp_tls_close(handle);
    }
}
