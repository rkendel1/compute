;; The minimal Compute workload for the Cloudflare free-tier profile: a WASI
;; command module that writes one line to stdout and exits 0. Any language
;; that compiles to wasm32-wasip1 produces the same kind of module; this one
;; is written by hand so the example has no toolchain to install.
;;
;; hello.wasm is this file compiled; a test keeps the two identical.
(module
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 64) "hello from Compute on Cloudflare Workers\n")
  (func (export "_start")
    ;; one iovec at 0: { buf = 64, len = 41 }
    (i32.store (i32.const 0) (i32.const 64))
    (i32.store (i32.const 4) (i32.const 41))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))))
