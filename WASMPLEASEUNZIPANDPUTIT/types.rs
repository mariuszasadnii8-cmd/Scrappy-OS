// ============================================================
//  scrappy-wasm/src/types.rs
//  Core WASM and Scrappy OS type definitions
// ============================================================

use alloc::{vec::Vec, string::String};

// ── WASM primitive types ─────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValType {
    I32,
    I64,
    F32,
    F64,
}

/// Runtime value (tagged union; we keep f32/f64 as bit-patterns
/// to avoid any libm / float-in-match issues in no_std contexts).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    I32(i32),
    I64(i64),
    F32(u32), // raw bits
    F64(u64), // raw bits
}

impl Value {
    pub fn default_for(ty: ValType) -> Self {
        match ty {
            ValType::I32 => Value::I32(0),
            ValType::I64 => Value::I64(0),
            ValType::F32 => Value::F32(0),
            ValType::F64 => Value::F64(0),
        }
    }
    pub fn as_i32(self)  -> Option<i32>  { if let Value::I32(v) = self { Some(v) } else { None } }
    pub fn as_i64(self)  -> Option<i64>  { if let Value::I64(v) = self { Some(v) } else { None } }
    pub fn as_f32b(self) -> Option<u32>  { if let Value::F32(v) = self { Some(v) } else { None } }
    pub fn as_f64b(self) -> Option<u64>  { if let Value::F64(v) = self { Some(v) } else { None } }
    pub fn as_f32(self)  -> Option<f32>  { self.as_f32b().map(f32::from_bits) }
    pub fn as_f64(self)  -> Option<f64>  { self.as_f64b().map(f64::from_bits) }
    pub fn ty(self) -> ValType {
        match self {
            Value::I32(_) => ValType::I32,
            Value::I64(_) => ValType::I64,
            Value::F32(_) => ValType::F32,
            Value::F64(_) => ValType::F64,
        }
    }
    pub fn is_true(self) -> bool {
        match self {
            Value::I32(v) => v != 0,
            Value::I64(v) => v != 0,
            Value::F32(v) => v != 0,
            Value::F64(v) => v != 0,
        }
    }
}

// ── Function types ───────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuncType {
    pub params:  Vec<ValType>,
    pub results: Vec<ValType>,
}

// ── Limits / Memory / Table ──────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub min: u32,
    pub max: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
pub struct MemType(pub Limits);

#[derive(Debug, Clone, Copy)]
pub struct TableType {
    pub limits: Limits,
    // Only funcref tables for now
}

// ── Globals ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct GlobalType {
    pub ty:     ValType,
    pub mutable: bool,
}

#[derive(Debug, Clone)]
pub struct GlobalInst {
    pub ty:    GlobalType,
    pub value: Value,
}

// ── Imports / Exports ────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum ImportDesc {
    Func(u32),   // type index
    Table(TableType),
    Mem(MemType),
    Global(GlobalType),
}

#[derive(Debug, Clone)]
pub struct Import {
    pub module: String,
    pub name:   String,
    pub desc:   ImportDesc,
}

#[derive(Debug, Clone)]
pub enum ExportDesc {
    Func(u32),
    Table(u32),
    Mem(u32),
    Global(u32),
}

#[derive(Debug, Clone)]
pub struct Export {
    pub name: String,
    pub desc: ExportDesc,
}

// ── Function body ────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LocalEntry {
    pub count: u32,
    pub ty:    ValType,
}

#[derive(Debug, Clone)]
pub struct FuncBody {
    pub locals: Vec<LocalEntry>,
    pub code:   Vec<u8>, // raw expression bytes (decoded on demand)
}

// ── Element / Data segments ──────────────────────────────────

#[derive(Debug, Clone)]
pub struct DataSegment {
    pub mem_idx: u32,
    pub offset:  Vec<u8>, // constant-expr bytes
    pub init:    Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ElemSegment {
    pub table_idx: u32,
    pub offset:    Vec<u8>, // constant-expr bytes
    pub indices:   Vec<u32>,
}

// ── Scrappy OS: Email message system ─────────────────────────

/// A cell address (0-6) in Scrappy OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellId(pub u8);

impl CellId {
    pub const MAX: u8 = 6;
    pub fn new(id: u8) -> Option<Self> {
        if id <= Self::MAX { Some(CellId(id)) } else { None }
    }
    pub fn as_usize(self) -> usize { self.0 as usize }
}

/// Direction of an email message.
#[derive(Debug, Clone)]
pub enum MailDirection {
    Inbound,   // arriving at this cell
    Outbound,  // sent from this cell
}

/// Minimal email envelope used by the WASI bridge.
#[derive(Debug, Clone)]
pub struct EmailMessage {
    pub from:    CellId,
    pub to:      CellId,
    pub subject: String,      // maps to WASI fd path or syscall name
    pub body:    Vec<u8>,     // payload bytes
    pub reply:   Option<Vec<u8>>, // filled in by the kernel on response
}

/// What a WASI host call returns.
#[derive(Debug, Clone)]
pub enum WasiResult {
    Ok(Vec<Value>),
    Err(WasiErrno),
    Exit(i32),
}

/// A minimal WASI errno set (wasi_snapshot_preview1 codes).
#[derive(Debug, Clone, Copy)]
#[repr(u32)]
pub enum WasiErrno {
    Success  = 0,
    Badf     = 8,
    Inval    = 28,
    Nosys    = 52,
    Io       = 29,
}

// ── Errors ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum WasmError {
    InvalidMagic,
    InvalidVersion,
    UnexpectedEof,
    InvalidSection(u8),
    InvalidOpcode(u8),
    InvalidType,
    TypeMismatch,
    StackUnderflow,
    CallStackOverflow,
    UndefinedLocal(u32),
    UndefinedGlobal(u32),
    UndefinedFunc(u32),
    MemoryOutOfBounds,
    DivisionByZero,
    IntegerOverflow,
    UnreachableExecuted,
    ExportNotFound,
    NoFreeCell,
    CellOutOfRange(u8),
    WasiError(WasiErrno),
    Trap(TrapCode),
}

#[derive(Debug, Clone, Copy)]
pub enum TrapCode {
    Unreachable,
    MemoryOutOfBounds,
    DivisionByZero,
    IntegerOverflow,
    InvalidConversionToInt,
    StackOverflow,
    IndirectCallTypeMismatch,
}
