// ============================================================
//  scrappy-wasm/src/runner.rs
//
//  ScrappyWasmRunner
//  ─────────────────
//  Top-level API for Scrappy OS kernel code.
//  Manages 7 virtual "cells" (slots), each of which can hold
//  one independently running WASM module instance.
//
//  Usage:
//    let mut runner = ScrappyWasmRunner::new();
//    let cell = runner.load(wasm_bytes, cell_id, wasi_ctx)?;
//    let result = runner.call(cell, "_start", &[])?;
//    runner.unload(cell);
// ============================================================

use alloc::{vec, vec::Vec, string::String, boxed::Box};
use crate::decode;
use crate::interp::{ModuleInstance, Func, FuncKind};
use crate::types::*;
use crate::wasi::{build_wasi_imports, WasiCtx};

// ── Cell state ────────────────────────────────────────────────

pub struct CellSlot {
    pub id:       CellId,
    pub instance: ModuleInstance,
    pub status:   CellStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellStatus {
    Idle,
    Running,
    Exited(i32),
    Faulted,
}

// ── The runner ────────────────────────────────────────────────

/// A Scrappy OS WASM runner managing exactly 7 virtual cells.
///
/// Cell layout (matches kernel cell map):
///   Cell 0 → Scheduler / System cell
///   Cell 1 → File-system cell  
///   Cell 2 → Stdin cell
///   Cell 3 → Stdout cell
///   Cell 4 → Stderr cell
///   Cell 5 → Timer cell
///   Cell 6 → Entropy cell
///
/// Any or all of these can host a WASM module simultaneously.
pub struct ScrappyWasmRunner {
    cells: [Option<CellSlot>; 7],
}

impl ScrappyWasmRunner {
    pub const NUM_CELLS: usize = 7;

    /// Create an empty runner (all cells free).
    pub fn new() -> Self {
        ScrappyWasmRunner {
            cells: [None, None, None, None, None, None, None],
        }
    }

    // ── Loading ──────────────────────────────────────────────

    /// Parse and instantiate a WASM binary into `cell`.
    ///
    /// `wasi_ctx` is an Option: if Some, WASI imports are wired up;
    /// if None, the module is expected to have no external imports
    /// (useful for pure-compute kernellets).
    ///
    /// Additional host functions can be injected via `extra_imports`.
    pub fn load(
        &mut self,
        wasm_bytes: &[u8],
        cell:       CellId,
        wasi_ctx:   Option<WasiCtx>,
        extra_imports: Vec<HostImport>,
    ) -> Result<(), WasmError> {
        if self.cells[cell.as_usize()].is_some() {
            return Err(WasmError::CellOutOfRange(cell.0)); // already occupied
        }

        // 1. Parse binary
        let module = decode::parse(wasm_bytes)?;

        // 2. Build host-function list
        let mut host_funcs: Vec<(&str, &str, Func)> = Vec::new();

        if let Some(ctx) = wasi_ctx {
            for (m, n, f) in build_wasi_imports(ctx) {
                host_funcs.push((m, n, f));
            }
        }

        // 3. Match imports declared in the module against host funcs
        let mut resolved: Vec<Func> = Vec::new();
        for import in &module.imports {
            if let ImportDesc::Func(ty_idx) = import.desc {
                let ty = module.types.get(ty_idx as usize)
                    .ok_or(WasmError::InvalidType)?.clone();

                // Search WASI built-ins first
                let found = host_funcs.iter().position(|(m, n, _)| {
                    *m == import.module.as_str() && *n == import.name.as_str()
                });

                if let Some(idx) = found {
                    let (_, _, f) = host_funcs.remove(idx);
                    resolved.push(f);
                } else {
                    // Search extra_imports
                    let extra_pos = extra_imports.iter().position(|ei| {
                        ei.module == import.module && ei.name == import.name
                    });
                    if let Some(pos) = extra_pos {
                        resolved.push(extra_imports[pos].build_func(ty));
                    } else {
                        // Provide a stub that returns ENOSYS
                        resolved.push(stub_func(ty));
                    }
                }
            }
        }

        // 4. Instantiate
        let instance = ModuleInstance::instantiate(module, resolved)?;

        // 5. Store in cell
        self.cells[cell.as_usize()] = Some(CellSlot {
            id: cell,
            instance,
            status: CellStatus::Idle,
        });

        Ok(())
    }

    // ── Calling ──────────────────────────────────────────────

    /// Call an exported function in the given cell.
    /// Returns the result values.
    pub fn call(
        &mut self,
        cell:      CellId,
        func_name: &str,
        args:      &[Value],
    ) -> Result<Vec<Value>, WasmError> {
        let slot = self.cells[cell.as_usize()].as_mut()
            .ok_or(WasmError::CellOutOfRange(cell.0))?;

        slot.status = CellStatus::Running;

        match slot.instance.call_export(func_name, args) {
            Ok(results) => {
                slot.status = CellStatus::Idle;
                Ok(results)
            }
            Err(WasmError::WasiError(WasiErrno::Success)) => {
                // proc_exit(0)
                slot.status = CellStatus::Exited(0);
                Ok(vec![])
            }
            Err(e) => {
                slot.status = CellStatus::Faulted;
                Err(e)
            }
        }
    }

    /// Call `_start` (the WASI entry point) in the given cell.
    pub fn start(&mut self, cell: CellId) -> Result<(), WasmError> {
        self.call(cell, "_start", &[]).map(|_| ())
    }

    // ── Memory access ─────────────────────────────────────────

    /// Read a slice of linear memory from a cell (useful for IPC).
    pub fn read_mem(
        &self,
        cell:   CellId,
        offset: u32,
        len:    usize,
    ) -> Result<Vec<u8>, WasmError> {
        let slot = self.cells[cell.as_usize()].as_ref()
            .ok_or(WasmError::CellOutOfRange(cell.0))?;
        let mem = slot.instance.memories.get(0)
            .ok_or(WasmError::MemoryOutOfBounds)?;
        let end = (offset as usize).checked_add(len)
            .ok_or(WasmError::MemoryOutOfBounds)?;
        if end > mem.data.len() { return Err(WasmError::MemoryOutOfBounds); }
        Ok(mem.data[offset as usize..end].to_vec())
    }

    /// Write a slice into a cell's linear memory (for kernel→WASM IPC).
    pub fn write_mem(
        &mut self,
        cell:   CellId,
        offset: u32,
        data:   &[u8],
    ) -> Result<(), WasmError> {
        let slot = self.cells[cell.as_usize()].as_mut()
            .ok_or(WasmError::CellOutOfRange(cell.0))?;
        let mem = slot.instance.memories.get_mut(0)
            .ok_or(WasmError::MemoryOutOfBounds)?;
        let end = (offset as usize) + data.len();
        if end > mem.data.len() { return Err(WasmError::MemoryOutOfBounds); }
        mem.data[offset as usize..end].copy_from_slice(data);
        Ok(())
    }

    // ── Global access ─────────────────────────────────────────

    pub fn get_global(&self, cell: CellId, name: &str) -> Result<Value, WasmError> {
        let slot = self.cells[cell.as_usize()].as_ref()
            .ok_or(WasmError::CellOutOfRange(cell.0))?;
        let inst = &slot.instance;
        let idx = inst.module.exports.iter().find_map(|e| {
            if e.name == name { if let ExportDesc::Global(i) = e.desc { return Some(i); } }
            None
        }).ok_or(WasmError::ExportNotFound)?;
        Ok(inst.globals[idx as usize].value)
    }

    // ── Lifecycle ─────────────────────────────────────────────

    /// Terminate and remove a WASM instance from its cell.
    pub fn unload(&mut self, cell: CellId) -> Result<(), WasmError> {
        self.cells[cell.as_usize()].take()
            .map(|_| ())
            .ok_or(WasmError::CellOutOfRange(cell.0))
    }

    pub fn cell_status(&self, cell: CellId) -> Option<CellStatus> {
        self.cells[cell.as_usize()].as_ref().map(|s| s.status)
    }

    pub fn is_cell_free(&self, cell: CellId) -> bool {
        self.cells[cell.as_usize()].is_none()
    }

    /// Return the first free cell id, if any.
    pub fn first_free_cell(&self) -> Option<CellId> {
        for i in 0..Self::NUM_CELLS {
            if self.cells[i].is_none() {
                return CellId::new(i as u8);
            }
        }
        None
    }

    // ── Inter-cell messaging ──────────────────────────────────

    /// Deliver an email message payload into a cell's memory at a
    /// predetermined "inbox" address, then invoke its `scrappy_recv`
    /// export (if present).  This is the Scrappy OS IPC protocol.
    ///
    /// Convention:
    ///   Linear memory[0x1000..0x1FFF] = inbox ring buffer
    ///   Exported fn `scrappy_recv(from: i32, len: i32) → i32`
    pub fn deliver_email(
        &mut self,
        to:   CellId,
        from: CellId,
        body: &[u8],
    ) -> Result<i32, WasmError> {
        const INBOX_BASE: u32 = 0x1000;
        self.write_mem(to, INBOX_BASE, body)?;
        let result = self.call(to, "scrappy_recv", &[
            Value::I32(from.0 as i32),
            Value::I32(body.len() as i32),
        ])?;
        Ok(result.first().and_then(|v| v.as_i32()).unwrap_or(0))
    }
}

// ── Stub function (for unresolved imports) ────────────────────

fn stub_func(ty: FuncType) -> Func {
    let result_count = ty.results.len();
    Func {
        ty,
        kind: FuncKind::Host {
            func: Box::new(move |_args, _mem| {
                // Return zero-valued results to avoid crashing the module.
                let results: Vec<Value> = (0..result_count)
                    .map(|_| Value::I32(WasiErrno::Nosys as i32))
                    .collect();
                Ok(results)
            }),
        },
    }
}

// ── Extra host import builder ─────────────────────────────────

/// Allows kernel subsystems to inject their own host functions.
pub struct HostImport {
    pub module: String,
    pub name:   String,
    inner: Box<dyn Fn(FuncType) -> Func>,
}

impl HostImport {
    pub fn new<F>(module: &str, name: &str, f: F) -> Self
    where
        F: Fn(FuncType) -> Func + 'static,
    {
        HostImport {
            module: String::from(module),
            name:   String::from(name),
            inner:  Box::new(f),
        }
    }

    pub fn build_func(&self, ty: FuncType) -> Func {
        (self.inner)(ty)
    }

    /// Convenience: build a simple host function from a closure.
    /// The closure receives `(args, mem)` and returns `Vec<Value>`.
    pub fn simple(
        module: &str,
        name:   &str,
        params:  Vec<ValType>,
        results: Vec<ValType>,
        f: impl Fn(&[Value], &mut crate::interp::Memory) -> Result<Vec<Value>, WasmError> + 'static + Clone,
    ) -> Self {
        let p = params.clone();
        let r = results.clone();
        HostImport::new(module, name, move |_ty| Func {
            ty: FuncType { params: p.clone(), results: r.clone() },
            kind: FuncKind::Host { func: Box::new(f.clone()) },
        })
    }
}
