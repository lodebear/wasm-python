;; Stub "monocp" module satisfying the 14 plain-core net/TLS imports of the
;; threaded CPython bridge module, so it can be INSTANTIATED standalone under
;; wasmtime for the `import socket`/`import ssl` load smoke. These funcs are
;; never CALLED by the smoke (which only imports the shims, not connects), so
;; each returns errno 8 (EBADF) as a safe "not wired" value.
(module
  (func (export "net_instance")       (param i32)            (result i32) i32.const 8)
  (func (export "tcp_create")         (param i32 i32)        (result i32) i32.const 8)
  (func (export "tcp_start_connect")  (param i32 i32 i32 i32)(result i32) i32.const 8)
  (func (export "tcp_finish_connect") (param i32)            (result i32) i32.const 8)
  (func (export "tcp_read")           (param i32 i32 i32 i32)(result i32) i32.const 8)
  (func (export "tcp_write")          (param i32 i32 i32 i32)(result i32) i32.const 8)
  (func (export "tcp_close")          (param i32)            (result i32) i32.const 8)
  (func (export "dns_resolve")        (param i32 i32 i32 i32)(result i32) i32.const 8)
  (func (export "dns_resolve_next")   (param i32 i32 i32)    (result i32) i32.const 8)
  (func (export "dns_close")          (param i32)            (result i32) i32.const 8)
  (func (export "tls_upgrade")        (param i32 i32 i32 i32)(result i32) i32.const 8)
  (func (export "tls_read")           (param i32 i32 i32 i32)(result i32) i32.const 8)
  (func (export "tls_write")          (param i32 i32 i32 i32)(result i32) i32.const 8)
  (func (export "tls_close")          (param i32)            (result i32) i32.const 8))
