use std::path::{Path, PathBuf};

use compute_runtime_conformance::{ConformanceFixture, run_contract};
use compute_runtime_wasm::WasmRuntime;

struct WasiFixture;

impl ConformanceFixture for WasiFixture {
    fn prepare(&self, directory: &Path) -> PathBuf {
        let path = directory.join("conformance.wasm");
        std::fs::write(&path, wat::parse_str(WASI_PROBE).unwrap()).unwrap();
        path
    }
}

#[tokio::test]
async fn wasm_conforms() {
    run_contract(&WasmRuntime, &WasiFixture).await;
}

const WASI_PROBE: &str = r#"
(module
  (import "wasi_snapshot_preview1" "args_sizes_get" (func $args_sizes_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "args_get" (func $args_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "environ_sizes_get" (func $env_sizes_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "environ_get" (func $env_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_close" (func $fd_close (param i32) (result i32)))
  (import "wasi_snapshot_preview1" "path_open" (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (memory (export "memory") 3)

  (data (i32.const 100) "[]\0a,\22|missing")
  (data (i32.const 120) "out α\0asecond line with spaces\0a")
  (data (i32.const 160) "err β\0asecond error line\0a")
  (data (i32.const 200) "output before exit\0a")
  (data (i32.const 224) "COMPUTE_TEST_VALUE")
  (data (i32.const 244) "COMPUTE_EMPTY_VALUE")
  (data (i32.const 264) "COMPUTE_UNDECLARED_VALUE")
  (data (i32.const 300) "result.jsonreport.txt{\22ok\22:true}reportx")
  (data (i32.const 400) "declared.txt../host-secret/etc/passwdblockedlimited")
  (data (i32.const 500) "data/input.txtdata/output.txt")

  (func $write (param $fd i32) (param $ptr i32) (param $len i32)
    (i32.store (i32.const 0) (local.get $ptr))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 8))))

  (func $strlen (param $ptr i32) (result i32) (local $cursor i32)
    (local.set $cursor (local.get $ptr))
    (block $done (loop $scan
      (br_if $done (i32.eqz (i32.load8_u (local.get $cursor))))
      (local.set $cursor (i32.add (local.get $cursor) (i32.const 1)))
      (br $scan)))
    (i32.sub (local.get $cursor) (local.get $ptr)))

  (func $prefix (param $value i32) (param $key i32) (param $len i32) (result i32)
    (local $i i32)
    (block $no (loop $compare
      (br_if $no (i32.eq (local.get $i) (local.get $len)))
      (if (i32.ne
            (i32.load8_u (i32.add (local.get $value) (local.get $i)))
            (i32.load8_u (i32.add (local.get $key) (local.get $i))))
        (then (return (i32.const 0))))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br $compare)))
    (i32.eq (i32.load8_u (i32.add (local.get $value) (local.get $len))) (i32.const 61)))

  (func $find_env (param $key i32) (param $len i32) (result i32)
    (local $count i32) (local $i i32) (local $entry i32)
    (drop (call $env_sizes_get (i32.const 16) (i32.const 20)))
    (local.set $count (i32.load (i32.const 16)))
    (drop (call $env_get (i32.const 2048) (i32.const 32768)))
    (block $missing (loop $each
      (br_if $missing (i32.ge_u (local.get $i) (local.get $count)))
      (local.set $entry (i32.load (i32.add (i32.const 2048) (i32.mul (local.get $i) (i32.const 4)))))
      (if (call $prefix (local.get $entry) (local.get $key) (local.get $len))
        (then (return (i32.add (local.get $entry) (i32.add (local.get $len) (i32.const 1))))))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br $each)))
    (i32.const 0))

  (func $write_env (param $key i32) (param $len i32) (local $value i32)
    (local.set $value (call $find_env (local.get $key) (local.get $len)))
    (if (local.get $value)
      (then (call $write (i32.const 1) (local.get $value) (call $strlen (local.get $value))))
      (else (call $write (i32.const 1) (i32.const 106) (i32.const 7)))))

  (func $args
    (local $argc i32) (local $i i32) (local $arg i32)
    (drop (call $args_sizes_get (i32.const 16) (i32.const 20)))
    (local.set $argc (i32.load (i32.const 16)))
    (drop (call $args_get (i32.const 1024) (i32.const 4096)))
    (call $write (i32.const 1) (i32.const 100) (i32.const 1))
    (local.set $i (i32.const 2))
    (block $done (loop $each
      (br_if $done (i32.ge_u (local.get $i) (local.get $argc)))
      (if (i32.gt_u (local.get $i) (i32.const 2))
        (then (call $write (i32.const 1) (i32.const 103) (i32.const 1))))
      (call $write (i32.const 1) (i32.const 104) (i32.const 1))
      (local.set $arg (i32.load (i32.add (i32.const 1024) (i32.mul (local.get $i) (i32.const 4)))))
      (call $write (i32.const 1) (local.get $arg) (call $strlen (local.get $arg)))
      (call $write (i32.const 1) (i32.const 104) (i32.const 1))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br $each)))
    (call $write (i32.const 1) (i32.const 101) (i32.const 2)))

  (func $stdin (local $read i32)
    (block $done (loop $again
      (i32.store (i32.const 0) (i32.const 65536))
      (i32.store (i32.const 4) (i32.const 65536))
      (drop (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 8)))
      (local.set $read (i32.load (i32.const 8)))
      (br_if $done (i32.eqz (local.get $read)))
      (call $write (i32.const 1) (i32.const 65536) (local.get $read))
      (br $again))))

  (func $artifact (local $fd i32)
    (drop (call $path_open (i32.const 5) (i32.const 0) (i32.const 300) (i32.const 11)
      (i32.const 1) (i64.const 64) (i64.const 0) (i32.const 0) (i32.const 16)))
    (local.set $fd (i32.load (i32.const 16)))
    (call $write (local.get $fd) (i32.const 321) (i32.const 11))
    (drop (call $fd_close (local.get $fd)))
    (drop (call $path_open (i32.const 5) (i32.const 0) (i32.const 311) (i32.const 10)
      (i32.const 1) (i64.const 64) (i64.const 0) (i32.const 0) (i32.const 16)))
    (local.set $fd (i32.load (i32.const 16)))
    (call $write (local.get $fd) (i32.const 332) (i32.const 6))
    (drop (call $fd_close (local.get $fd))))

  (func $large (param $fd i32) (local $i i32)
    (loop $again
      (call $write (local.get $fd) (i32.const 338) (i32.const 1))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $again (i32.lt_u (local.get $i) (i32.const 8192)))))

  (func $filesystem (local $fd i32) (local $read i32) (local $errno i32)
    (drop (call $path_open (i32.const 3) (i32.const 0) (i32.const 400) (i32.const 12)
      (i32.const 0) (i64.const 2) (i64.const 0) (i32.const 0) (i32.const 16)))
    (local.set $fd (i32.load (i32.const 16)))
    (i32.store (i32.const 0) (i32.const 65536))
    (i32.store (i32.const 4) (i32.const 256))
    (drop (call $fd_read (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 8)))
    (local.set $read (i32.load (i32.const 8)))
    (call $write (i32.const 1) (i32.const 65536) (local.get $read))
    (drop (call $fd_close (local.get $fd)))
    (call $write (i32.const 1) (i32.const 105) (i32.const 1))
    (local.set $errno (call $path_open (i32.const 3) (i32.const 0) (i32.const 412) (i32.const 14)
      (i32.const 0) (i64.const 2) (i64.const 0) (i32.const 0) (i32.const 16)))
    (if (local.get $errno)
      (then (call $write (i32.const 1) (i32.const 437) (i32.const 7))))
    (call $write (i32.const 1) (i32.const 105) (i32.const 1))
    (local.set $errno (call $path_open (i32.const 3) (i32.const 0) (i32.const 426) (i32.const 11)
      (i32.const 0) (i64.const 2) (i64.const 0) (i32.const 0) (i32.const 16)))
    (if (local.get $errno)
      (then (call $write (i32.const 1) (i32.const 437) (i32.const 7)))))

  (func $portable_io (local $input i32) (local $output i32) (local $read i32)
    (drop (call $path_open (i32.const 3) (i32.const 0) (i32.const 500) (i32.const 14)
      (i32.const 0) (i64.const 2) (i64.const 0) (i32.const 0) (i32.const 16)))
    (local.set $input (i32.load (i32.const 16)))
    (i32.store (i32.const 0) (i32.const 65536))
    (i32.store (i32.const 4) (i32.const 256))
    (drop (call $fd_read (local.get $input) (i32.const 0) (i32.const 1) (i32.const 8)))
    (local.set $read (i32.load (i32.const 8)))
    (drop (call $fd_close (local.get $input)))
    (drop (call $path_open (i32.const 5) (i32.const 0) (i32.const 514) (i32.const 15)
      (i32.const 1) (i64.const 64) (i64.const 0) (i32.const 0) (i32.const 16)))
    (local.set $output (i32.load (i32.const 16)))
    (call $write (local.get $output) (i32.const 65536) (local.get $read))
    (drop (call $fd_close (local.get $output))))

  (func (export "_start") (local $op i32) (local $code i32)
    (drop (call $args_sizes_get (i32.const 16) (i32.const 20)))
    (drop (call $args_get (i32.const 1024) (i32.const 4096)))
    (local.set $op (i32.load (i32.const 1028)))
    (if (i32.eq (i32.load8_u (local.get $op)) (i32.const 97)) (then
      (if (i32.eq (i32.load8_u (i32.add (local.get $op) (i32.const 2))) (i32.const 103))
        (then (call $args)) (else (call $artifact))) (return)))
    (if (i32.eq (i32.load8_u (local.get $op)) (i32.const 115)) (then
      (if (i32.eq (i32.load8_u (i32.add (local.get $op) (i32.const 1))) (i32.const 108))
        (then (loop $forever (br $forever))))
      (if (i32.eq (i32.load8_u (i32.add (local.get $op) (i32.const 2))) (i32.const 100))
        (then (call $stdin))
        (else (call $write (i32.const 1) (i32.const 120) (i32.const 31))
              (call $write (i32.const 2) (i32.const 160) (i32.const 25)))) (return)))
    (if (i32.eq (i32.load8_u (local.get $op)) (i32.const 101)) (then
      (if (i32.eq (i32.load8_u (i32.add (local.get $op) (i32.const 1))) (i32.const 120))
        (then (call $write (i32.const 1) (i32.const 200) (i32.const 19))
              (local.set $code (i32.sub (i32.load8_u (i32.load (i32.const 1032))) (i32.const 48)))
              (call $proc_exit (local.get $code))))
      (if (i32.eq (i32.load8_u (i32.add (local.get $op) (i32.const 1))) (i32.const 110))
        (then (call $write_env (i32.const 224) (i32.const 18))
              (call $write (i32.const 1) (i32.const 105) (i32.const 1))
              (call $write_env (i32.const 244) (i32.const 19))
              (call $write (i32.const 1) (i32.const 105) (i32.const 1))
              (call $write_env (i32.const 264) (i32.const 24))
              (call $write (i32.const 1) (i32.const 102) (i32.const 1))))
      (return)))
    (if (i32.eq (i32.load8_u (local.get $op)) (i32.const 108)) (then
      (if (i32.eq (i32.load8_u (i32.add (local.get $op) (i32.const 6))) (i32.const 111))
        (then (call $large (i32.const 1))) (else (call $large (i32.const 2))))))
    (if (i32.eq (i32.load8_u (local.get $op)) (i32.const 102))
      (then (call $filesystem)))
    (if (i32.eq (i32.load8_u (local.get $op)) (i32.const 109))
      (then (if (i32.eq (memory.grow (i32.const 10)) (i32.const -1))
        (then (call $write (i32.const 1) (i32.const 444) (i32.const 7))))))
    (if (i32.eq (i32.load8_u (local.get $op)) (i32.const 112))
      (then (call $portable_io)))
  )
)
"#;
