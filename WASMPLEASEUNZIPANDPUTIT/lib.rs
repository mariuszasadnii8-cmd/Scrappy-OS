// ============================================================
//  scrappy-wasm/src/lib.rs
//  Crate root — no_std WASM interpreter for Scrappy OS
// ============================================================

#![no_std]
#![allow(clippy::result_unit_err)]
#![deny(unsafe_op_in_unsafe_fn)]

//  We need heap allocation (Vec, Box, String).
//  In Scrappy OS the kernel provides a global allocator
//  (e.g. a slab or buddy allocator) backed by the kernel heap.
extern crate alloc;

pub mod types;
pub mod decode;
pub mod interp;
pub mod wasi;
pub mod runner;

// ── Re-exports for ergonomic kernel use ──────────────────────

pub use types::{
    CellId, Value, ValType, WasmError, TrapCode,
    EmailMessage, WasiErrno,
    FuncType, GlobalType, MemType,
};

pub use decode::parse as parse_wasm;

pub use interp::ModuleInstance;

pub use wasi::{WasiCtx, Mailbox,
    CELL_SCHEDULER, CELL_FS, CELL_STDIN, CELL_STDOUT,
    CELL_STDERR,   CELL_TIMER, CELL_ENTROPY};

pub use runner::{ScrappyWasmRunner, CellStatus, HostImport};

// ── Kernel integration example ────────────────────────────────
//
//  ```rust (in kernel crate)
//  use scrappy_wasm::{
//      ScrappyWasmRunner, CellId, WasiCtx, Value,
//      CELL_STDOUT, CELL_ENTROPY,
//  };
//
//  struct KernelMailbox;
//  impl scrappy_wasm::Mailbox for KernelMailbox {
//      fn send_recv(&self, msg: scrappy_wasm::EmailMessage) -> alloc::vec::Vec<u8> {
//          // Route to the appropriate kernel subsystem
//          kernel::email::route(msg)
//      }
//      fn bytes_ready(&self, _: CellId) -> usize { 0 }
//      fn now_ns(&self) -> u64 { kernel::timer::now_ns() }
//  }
//
//  fn run_wasm_program(wasm: &[u8], cell: CellId) -> Result<(), scrappy_wasm::WasmError> {
//      let ctx = WasiCtx::new(
//          cell,
//          alloc::vec![b"my_program\0".to_vec()],
//          alloc::vec![],
//          alloc::boxed::Box::new(KernelMailbox),
//      );
//      let mut runner = ScrappyWasmRunner::new();
//      runner.load(wasm, cell, Some(ctx), alloc::vec![])?;
//      runner.start(cell)
//  }
//  ```
