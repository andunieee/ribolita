mod run;
mod store;
mod store_mem;
pub mod sync;

#[cfg(feature = "backend-libmdbx")]
mod store_mdbx;
#[cfg(feature = "backend-redb")]
mod store_redb;

pub use run::{
    ApplyReport, DEFAULT_FUEL_PER_ENTRY, DEFAULT_MAX_MEMORY, DEFAULT_MAX_SNAPSHOTS,
    DEFAULT_SNAPSHOT_INTERVAL, Query, RunEnvironment, RunError, now_secs,
};
pub use store::{ReadTxn, Row, Store, Table, WriteTxn};
pub use store_mem::MemStore;

#[cfg(feature = "backend-libmdbx")]
pub use store_mdbx::MdbxStore;
#[cfg(feature = "backend-redb")]
pub use store_redb::RedbStore;

#[doc(hidden)]
pub mod testing {
    /// A tiny program for tests: on every entry it increments `c` and sets `last` to the size
    /// of the encoded entry, which makes the state depend on the order entries are applied in.
    /// Entries that encode to more than 100 bytes are rejected.
    pub const COUNTER_WAT: &str = r#"
(module
  (import "env" "state_modify_internal" (func $modify (param i32 i32 i32 i32 i32) (result i32)))
  (import "env" "return_err" (func $err (param i32 i32)))
  (memory (export "memory") 2)
  (global $heap (mut i32) (i32.const 4096))
  ;; path "c" and path "last"
  (data (i32.const 0) "\81c")
  (data (i32.const 8) "\84last")
  ;; row blobs: one row, empty relative key, 1-byte value (patched for "last")
  (data (i32.const 16) "\01\00\00\00\00\00\01\00\00\00\01")
  (data (i32.const 32) "\01\00\00\00\00\00\01\00\00\00\00")
  (data (i32.const 64) "too long")
  (func (export "malloc") (param $size i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $size)))
    (local.get $p))
  (func (export "init"))
  (func (export "process") (param $ptr i32) (param $len i32)
    (if (i32.gt_u (local.get $len) (i32.const 100))
      (then (call $err (i32.const 64) (i32.const 8)) (return)))
    ;; incr c by 1
    (drop (call $modify (i32.const 0) (i32.const 2) (i32.const 16) (i32.const 11) (i32.const 7)))
    ;; set last = len (a positive fixint)
    (i32.store8 (i32.const 42) (local.get $len))
    (drop (call $modify (i32.const 8) (i32.const 5) (i32.const 32) (i32.const 11) (i32.const 1)))))
"#;
}
