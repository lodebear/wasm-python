;; Stub "monocp" module satisfying the 15 plain-core net/TLS/exec imports of the
;; threaded CPython bridge module, so it can be INSTANTIATED standalone under
;; wasmtime for the `import socket`/`import ssl`/`import zlib`/`pip --version`
;; load smoke. These funcs are never CALLED by the smoke (which only imports the
;; shims / runs pure-python, not connects or spawns), so the net/TLS funcs return
;; errno 8 (EBADF) and exec_with_context_v2 returns -1 (host error) as safe
;; "not wired" values.
(module
  (func (export "exec_with_context_v2") (param i32 i32)       (result i32) i32.const -1)
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
