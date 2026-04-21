// ============================================================
//  scrappy-wasm/src/decode.rs
//  Binary format parser: sections → WasmModule
//  Instruction decoder: raw bytes → Instr enum
// ============================================================

use alloc::{vec::Vec, string::String};
use crate::types::*;

// ── Low-level reader ─────────────────────────────────────────

pub struct Reader<'a> {
    pub data: &'a [u8],
    pub pos:  usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self { Reader { data, pos: 0 } }
    pub fn remaining(&self) -> usize { self.data.len() - self.pos }
    pub fn is_empty(&self) -> bool   { self.pos >= self.data.len() }

    pub fn read_byte(&mut self) -> Result<u8, WasmError> {
        if self.pos >= self.data.len() { return Err(WasmError::UnexpectedEof); }
        let b = self.data[self.pos];
        self.pos += 1;
        Ok(b)
    }

    pub fn read_bytes(&mut self, n: usize) -> Result<&'a [u8], WasmError> {
        if self.pos + n > self.data.len() { return Err(WasmError::UnexpectedEof); }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    /// Unsigned LEB128
    pub fn read_u32(&mut self) -> Result<u32, WasmError> {
        let mut result = 0u32;
        let mut shift  = 0u32;
        loop {
            let b = self.read_byte()?;
            result |= ((b & 0x7F) as u32) << shift;
            shift  += 7;
            if b & 0x80 == 0 { break; }
            if shift > 35 { return Err(WasmError::InvalidType); }
        }
        Ok(result)
    }

    pub fn read_u64(&mut self) -> Result<u64, WasmError> {
        let mut result = 0u64;
        let mut shift  = 0u32;
        loop {
            let b = self.read_byte()?;
            result |= ((b & 0x7F) as u64) << shift;
            shift  += 7;
            if b & 0x80 == 0 { break; }
            if shift > 70 { return Err(WasmError::InvalidType); }
        }
        Ok(result)
    }

    /// Signed LEB128 (i32)
    pub fn read_i32(&mut self) -> Result<i32, WasmError> {
        let mut result = 0i32;
        let mut shift  = 0u32;
        loop {
            let b = self.read_byte()?;
            result |= ((b & 0x7F) as i32) << shift;
            shift  += 7;
            if b & 0x80 == 0 {
                if shift < 32 && (b & 0x40) != 0 {
                    result |= !0i32 << shift;
                }
                break;
            }
        }
        Ok(result)
    }

    pub fn read_i64(&mut self) -> Result<i64, WasmError> {
        let mut result = 0i64;
        let mut shift  = 0u32;
        loop {
            let b = self.read_byte()?;
            result |= ((b & 0x7F) as i64) << shift;
            shift  += 7;
            if b & 0x80 == 0 {
                if shift < 64 && (b & 0x40) != 0 {
                    result |= !0i64 << shift;
                }
                break;
            }
        }
        Ok(result)
    }

    pub fn read_f32(&mut self) -> Result<u32, WasmError> {
        let b = self.read_bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn read_f64(&mut self) -> Result<u64, WasmError> {
        let b = self.read_bytes(8)?;
        Ok(u64::from_le_bytes([b[0],b[1],b[2],b[3],b[4],b[5],b[6],b[7]]))
    }

    pub fn read_name(&mut self) -> Result<String, WasmError> {
        let len = self.read_u32()? as usize;
        let bytes = self.read_bytes(len)?;
        // UTF-8 in no_std: we trust the WASM spec that names are valid UTF-8
        let s = core::str::from_utf8(bytes).map_err(|_| WasmError::InvalidType)?;
        Ok(String::from(s))
    }

    pub fn sub(&mut self, len: usize) -> Result<Reader<'a>, WasmError> {
        if self.pos + len > self.data.len() { return Err(WasmError::UnexpectedEof); }
        let sub = Reader { data: &self.data[self.pos..self.pos+len], pos: 0 };
        self.pos += len;
        Ok(sub)
    }
}

// ── Parsed module ─────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct WasmModule {
    pub types:    Vec<FuncType>,
    pub imports:  Vec<Import>,
    pub funcs:    Vec<u32>,      // type indices for locally-defined funcs
    pub tables:   Vec<TableType>,
    pub mems:     Vec<MemType>,
    pub globals:  Vec<(GlobalType, Vec<u8>)>, // type + init expr bytes
    pub exports:  Vec<Export>,
    pub start:    Option<u32>,
    pub elems:    Vec<ElemSegment>,
    pub codes:    Vec<FuncBody>,
    pub data:     Vec<DataSegment>,
}

// ── Section IDs ───────────────────────────────────────────────

const SEC_CUSTOM:   u8 = 0;
const SEC_TYPE:     u8 = 1;
const SEC_IMPORT:   u8 = 2;
const SEC_FUNCTION: u8 = 3;
const SEC_TABLE:    u8 = 4;
const SEC_MEMORY:   u8 = 5;
const SEC_GLOBAL:   u8 = 6;
const SEC_EXPORT:   u8 = 7;
const SEC_START:    u8 = 8;
const SEC_ELEMENT:  u8 = 9;
const SEC_CODE:     u8 = 10;
const SEC_DATA:     u8 = 11;

// ── Helpers ───────────────────────────────────────────────────

fn read_valtype(r: &mut Reader) -> Result<ValType, WasmError> {
    match r.read_byte()? {
        0x7F => Ok(ValType::I32),
        0x7E => Ok(ValType::I64),
        0x7D => Ok(ValType::F32),
        0x7C => Ok(ValType::F64),
        _    => Err(WasmError::InvalidType),
    }
}

fn read_limits(r: &mut Reader) -> Result<Limits, WasmError> {
    match r.read_byte()? {
        0x00 => Ok(Limits { min: r.read_u32()?, max: None }),
        0x01 => { let mn = r.read_u32()?; Ok(Limits { min: mn, max: Some(r.read_u32()?) }) }
        _    => Err(WasmError::InvalidType),
    }
}

fn read_global_type(r: &mut Reader) -> Result<GlobalType, WasmError> {
    let ty      = read_valtype(r)?;
    let mutable = r.read_byte()? == 0x01;
    Ok(GlobalType { ty, mutable })
}

// ── Public: parse a complete WASM binary ─────────────────────

pub fn parse(bytes: &[u8]) -> Result<WasmModule, WasmError> {
    let mut r = Reader::new(bytes);

    // Magic + version
    let magic = r.read_bytes(4)?;
    if magic != [0x00, 0x61, 0x73, 0x6D] { return Err(WasmError::InvalidMagic); }
    let ver = r.read_bytes(4)?;
    if ver != [0x01, 0x00, 0x00, 0x00] { return Err(WasmError::InvalidVersion); }

    let mut module = WasmModule::default();

    while !r.is_empty() {
        let id  = r.read_byte()?;
        let len = r.read_u32()? as usize;
        let mut sec = r.sub(len)?;

        match id {
            SEC_CUSTOM => { /* skip custom sections */ }

            SEC_TYPE => {
                let n = sec.read_u32()? as usize;
                for _ in 0..n {
                    if sec.read_byte()? != 0x60 { return Err(WasmError::InvalidType); }
                    let np = sec.read_u32()? as usize;
                    let mut params = Vec::with_capacity(np);
                    for _ in 0..np { params.push(read_valtype(&mut sec)?); }
                    let nr = sec.read_u32()? as usize;
                    let mut results = Vec::with_capacity(nr);
                    for _ in 0..nr { results.push(read_valtype(&mut sec)?); }
                    module.types.push(FuncType { params, results });
                }
            }

            SEC_IMPORT => {
                let n = sec.read_u32()? as usize;
                for _ in 0..n {
                    let module_name = sec.read_name()?;
                    let name        = sec.read_name()?;
                    let desc = match sec.read_byte()? {
                        0x00 => ImportDesc::Func(sec.read_u32()?),
                        0x01 => {
                            // elem type (0x70 funcref) + limits
                            let _ = sec.read_byte()?;
                            ImportDesc::Table(TableType { limits: read_limits(&mut sec)? })
                        }
                        0x02 => ImportDesc::Mem(MemType(read_limits(&mut sec)?)),
                        0x03 => ImportDesc::Global(read_global_type(&mut sec)?),
                        _    => return Err(WasmError::InvalidType),
                    };
                    module.imports.push(Import { module: module_name, name, desc });
                }
            }

            SEC_FUNCTION => {
                let n = sec.read_u32()? as usize;
                for _ in 0..n { module.funcs.push(sec.read_u32()?); }
            }

            SEC_TABLE => {
                let n = sec.read_u32()? as usize;
                for _ in 0..n {
                    let _ = sec.read_byte()?; // elem type (funcref)
                    module.tables.push(TableType { limits: read_limits(&mut sec)? });
                }
            }

            SEC_MEMORY => {
                let n = sec.read_u32()? as usize;
                for _ in 0..n { module.mems.push(MemType(read_limits(&mut sec)?)); }
            }

            SEC_GLOBAL => {
                let n = sec.read_u32()? as usize;
                for _ in 0..n {
                    let gt    = read_global_type(&mut sec)?;
                    let start = sec.pos;
                    // consume init expr until 0x0B (end)
                    loop {
                        if sec.read_byte()? == 0x0B { break; }
                    }
                    let expr = sec.data[start..sec.pos].to_vec();
                    module.globals.push((gt, expr));
                }
            }

            SEC_EXPORT => {
                let n = sec.read_u32()? as usize;
                for _ in 0..n {
                    let name = sec.read_name()?;
                    let desc = match sec.read_byte()? {
                        0x00 => ExportDesc::Func(sec.read_u32()?),
                        0x01 => ExportDesc::Table(sec.read_u32()?),
                        0x02 => ExportDesc::Mem(sec.read_u32()?),
                        0x03 => ExportDesc::Global(sec.read_u32()?),
                        _    => return Err(WasmError::InvalidType),
                    };
                    module.exports.push(Export { name, desc });
                }
            }

            SEC_START => {
                module.start = Some(sec.read_u32()?);
            }

            SEC_ELEMENT => {
                let n = sec.read_u32()? as usize;
                for _ in 0..n {
                    let table_idx = sec.read_u32()?;
                    let off_start = sec.pos;
                    loop { if sec.read_byte()? == 0x0B { break; } }
                    let offset  = sec.data[off_start..sec.pos].to_vec();
                    let cnt     = sec.read_u32()? as usize;
                    let mut indices = Vec::with_capacity(cnt);
                    for _ in 0..cnt { indices.push(sec.read_u32()?); }
                    module.elems.push(ElemSegment { table_idx, offset, indices });
                }
            }

            SEC_CODE => {
                let n = sec.read_u32()? as usize;
                for _ in 0..n {
                    let size    = sec.read_u32()? as usize;
                    let mut body_r = sec.sub(size)?;
                    let lc      = body_r.read_u32()? as usize;
                    let mut locals = Vec::with_capacity(lc);
                    for _ in 0..lc {
                        let count = body_r.read_u32()?;
                        let ty    = read_valtype(&mut body_r)?;
                        locals.push(LocalEntry { count, ty });
                    }
                    // remaining bytes are the expression
                    let code = body_r.data[body_r.pos..].to_vec();
                    module.codes.push(FuncBody { locals, code });
                }
            }

            SEC_DATA => {
                let n = sec.read_u32()? as usize;
                for _ in 0..n {
                    let mem_idx  = sec.read_u32()?;
                    let off_start = sec.pos;
                    loop { if sec.read_byte()? == 0x0B { break; } }
                    let offset = sec.data[off_start..sec.pos].to_vec();
                    let sz     = sec.read_u32()? as usize;
                    let init   = sec.read_bytes(sz)?.to_vec();
                    module.data.push(DataSegment { mem_idx, offset, init });
                }
            }

            _ => { /* unknown section: already consumed by sub() */ }
        }
    }

    Ok(module)
}

// ── Instruction set ───────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MemArg { pub align: u32, pub offset: u32 }

/// Decoded WASM instruction.
/// We decode the code section lazily (one instruction at a time)
/// instead of allocating a Vec<Instr> per function — saves memory.
#[derive(Debug, Clone)]
pub enum Instr {
    // Control
    Unreachable,
    Nop,
    Block(BlockType),
    Loop(BlockType),
    If(BlockType),
    Else,
    End,
    Br(u32),
    BrIf(u32),
    BrTable(Vec<u32>, u32),
    Return,
    Call(u32),
    CallIndirect(u32, u32),

    // Parametric
    Drop,
    Select,

    // Variable
    LocalGet(u32),
    LocalSet(u32),
    LocalTee(u32),
    GlobalGet(u32),
    GlobalSet(u32),

    // Memory
    I32Load(MemArg),   I64Load(MemArg),
    F32Load(MemArg),   F64Load(MemArg),
    I32Load8S(MemArg), I32Load8U(MemArg),
    I32Load16S(MemArg),I32Load16U(MemArg),
    I64Load8S(MemArg), I64Load8U(MemArg),
    I64Load16S(MemArg),I64Load16U(MemArg),
    I64Load32S(MemArg),I64Load32U(MemArg),
    I32Store(MemArg),  I64Store(MemArg),
    F32Store(MemArg),  F64Store(MemArg),
    I32Store8(MemArg), I32Store16(MemArg),
    I64Store8(MemArg), I64Store16(MemArg),I64Store32(MemArg),
    MemorySize,        MemoryGrow,

    // Numeric constants
    I32Const(i32),
    I64Const(i64),
    F32Const(u32),
    F64Const(u64),

    // i32 compare
    I32Eqz, I32Eq, I32Ne,
    I32LtS, I32LtU, I32GtS, I32GtU,
    I32LeS, I32LeU, I32GeS, I32GeU,

    // i64 compare
    I64Eqz, I64Eq, I64Ne,
    I64LtS, I64LtU, I64GtS, I64GtU,
    I64LeS, I64LeU, I64GeS, I64GeU,

    // f32 compare
    F32Eq, F32Ne, F32Lt, F32Gt, F32Le, F32Ge,

    // f64 compare
    F64Eq, F64Ne, F64Lt, F64Gt, F64Le, F64Ge,

    // i32 numeric
    I32Clz, I32Ctz, I32Popcnt,
    I32Add, I32Sub, I32Mul, I32DivS, I32DivU, I32RemS, I32RemU,
    I32And, I32Or,  I32Xor, I32Shl,  I32ShrS, I32ShrU,
    I32Rotl, I32Rotr,

    // i64 numeric
    I64Clz, I64Ctz, I64Popcnt,
    I64Add, I64Sub, I64Mul, I64DivS, I64DivU, I64RemS, I64RemU,
    I64And, I64Or,  I64Xor, I64Shl,  I64ShrS, I64ShrU,
    I64Rotl, I64Rotr,

    // f32 numeric
    F32Abs, F32Neg, F32Ceil, F32Floor, F32Trunc, F32Nearest, F32Sqrt,
    F32Add, F32Sub, F32Mul, F32Div, F32Min, F32Max, F32Copysign,

    // f64 numeric
    F64Abs, F64Neg, F64Ceil, F64Floor, F64Trunc, F64Nearest, F64Sqrt,
    F64Add, F64Sub, F64Mul, F64Div, F64Min, F64Max, F64Copysign,

    // Conversions
    I32WrapI64,
    I32TruncF32S, I32TruncF32U, I32TruncF64S, I32TruncF64U,
    I64ExtendI32S, I64ExtendI32U,
    I64TruncF32S, I64TruncF32U, I64TruncF64S, I64TruncF64U,
    F32ConvertI32S, F32ConvertI32U, F32ConvertI64S, F32ConvertI64U, F32DemoteF64,
    F64ConvertI32S, F64ConvertI32U, F64ConvertI64S, F64ConvertI64U, F64PromoteF32,
    I32ReinterpretF32, I64ReinterpretF64, F32ReinterpretI32, F64ReinterpretI64,
}

#[derive(Debug, Clone, Copy)]
pub enum BlockType {
    Empty,
    Val(ValType),
    TypeIdx(u32),
}

fn read_blocktype(r: &mut Reader) -> Result<BlockType, WasmError> {
    // Peek at next byte; 0x40 = empty, type byte = ValType, else signed LEB = type index
    let b = r.data.get(r.pos).copied().ok_or(WasmError::UnexpectedEof)?;
    match b {
        0x40 => { r.pos += 1; Ok(BlockType::Empty) }
        0x7F => { r.pos += 1; Ok(BlockType::Val(ValType::I32)) }
        0x7E => { r.pos += 1; Ok(BlockType::Val(ValType::I64)) }
        0x7D => { r.pos += 1; Ok(BlockType::Val(ValType::F32)) }
        0x7C => { r.pos += 1; Ok(BlockType::Val(ValType::F64)) }
        _ => {
            let idx = r.read_i32()? as u32;
            Ok(BlockType::TypeIdx(idx))
        }
    }
}

fn read_memarg(r: &mut Reader) -> Result<MemArg, WasmError> {
    let align  = r.read_u32()?;
    let offset = r.read_u32()?;
    Ok(MemArg { align, offset })
}

/// Decode one instruction from `r`, returning `None` at the function end (0x0B).
pub fn decode_instr(r: &mut Reader) -> Result<Option<Instr>, WasmError> {
    if r.is_empty() { return Ok(None); }
    let op = r.read_byte()?;
    use Instr::*;
    let instr = match op {
        0x00 => Unreachable,
        0x01 => Nop,
        0x02 => Block(read_blocktype(r)?),
        0x03 => Loop(read_blocktype(r)?),
        0x04 => If(read_blocktype(r)?),
        0x05 => Else,
        0x0B => return Ok(None), // End of block / function

        0x0C => Br(r.read_u32()?),
        0x0D => BrIf(r.read_u32()?),
        0x0E => {
            let n = r.read_u32()? as usize;
            let mut labels = Vec::with_capacity(n);
            for _ in 0..n { labels.push(r.read_u32()?); }
            let def = r.read_u32()?;
            BrTable(labels, def)
        }
        0x0F => Return,
        0x10 => Call(r.read_u32()?),
        0x11 => { let ti = r.read_u32()?; let tab = r.read_u32()?; CallIndirect(ti, tab) }

        0x1A => Drop,
        0x1B => Select,

        0x20 => LocalGet(r.read_u32()?),
        0x21 => LocalSet(r.read_u32()?),
        0x22 => LocalTee(r.read_u32()?),
        0x23 => GlobalGet(r.read_u32()?),
        0x24 => GlobalSet(r.read_u32()?),

        0x28 => I32Load(read_memarg(r)?),
        0x29 => I64Load(read_memarg(r)?),
        0x2A => F32Load(read_memarg(r)?),
        0x2B => F64Load(read_memarg(r)?),
        0x2C => I32Load8S(read_memarg(r)?),
        0x2D => I32Load8U(read_memarg(r)?),
        0x2E => I32Load16S(read_memarg(r)?),
        0x2F => I32Load16U(read_memarg(r)?),
        0x30 => I64Load8S(read_memarg(r)?),
        0x31 => I64Load8U(read_memarg(r)?),
        0x32 => I64Load16S(read_memarg(r)?),
        0x33 => I64Load16U(read_memarg(r)?),
        0x34 => I64Load32S(read_memarg(r)?),
        0x35 => I64Load32U(read_memarg(r)?),
        0x36 => I32Store(read_memarg(r)?),
        0x37 => I64Store(read_memarg(r)?),
        0x38 => F32Store(read_memarg(r)?),
        0x39 => F64Store(read_memarg(r)?),
        0x3A => I32Store8(read_memarg(r)?),
        0x3B => I32Store16(read_memarg(r)?),
        0x3C => I64Store8(read_memarg(r)?),
        0x3D => I64Store16(read_memarg(r)?),
        0x3E => I64Store32(read_memarg(r)?),
        0x3F => { let _ = r.read_byte(); MemorySize }
        0x40 => { let _ = r.read_byte(); MemoryGrow }

        0x41 => I32Const(r.read_i32()?),
        0x42 => I64Const(r.read_i64()?),
        0x43 => F32Const(r.read_f32()?),
        0x44 => F64Const(r.read_f64()?),

        // i32 cmp
        0x45 => I32Eqz, 0x46 => I32Eq,  0x47 => I32Ne,
        0x48 => I32LtS, 0x49 => I32LtU, 0x4A => I32GtS, 0x4B => I32GtU,
        0x4C => I32LeS, 0x4D => I32LeU, 0x4E => I32GeS, 0x4F => I32GeU,

        // i64 cmp
        0x50 => I64Eqz, 0x51 => I64Eq,  0x52 => I64Ne,
        0x53 => I64LtS, 0x54 => I64LtU, 0x55 => I64GtS, 0x56 => I64GtU,
        0x57 => I64LeS, 0x58 => I64LeU, 0x59 => I64GeS, 0x5A => I64GeU,

        // f32 cmp  (0x5B-0x60 per spec)
        0x5B => F32Eq, 0x5C => F32Ne, 0x5D => F32Lt,
        0x5E => F32Gt, 0x5F => F32Le, 0x60 => F32Ge,

        // f64 cmp  (0x61-0x66 per spec)
        0x61 => F64Eq, 0x62 => F64Ne, 0x63 => F64Lt,
        0x64 => F64Gt, 0x65 => F64Le, 0x66 => F64Ge,

        // i32 numeric
        0x67 => I32Clz,  0x68 => I32Ctz,  0x69 => I32Popcnt,
        0x6A => I32Add,  0x6B => I32Sub,  0x6C => I32Mul,
        0x6D => I32DivS, 0x6E => I32DivU, 0x6F => I32RemS, 0x70 => I32RemU,
        0x71 => I32And,  0x72 => I32Or,   0x73 => I32Xor,
        0x74 => I32Shl,  0x75 => I32ShrS, 0x76 => I32ShrU,
        0x77 => I32Rotl, 0x78 => I32Rotr,

        // i64 numeric
        0x79 => I64Clz,  0x7A => I64Ctz,  0x7B => I64Popcnt,
        0x7C => I64Add,  0x7D => I64Sub,  0x7E => I64Mul,
        0x7F => I64DivS, 0x80 => I64DivU, 0x81 => I64RemS, 0x82 => I64RemU,
        0x83 => I64And,  0x84 => I64Or,   0x85 => I64Xor,
        0x86 => I64Shl,  0x87 => I64ShrS, 0x88 => I64ShrU,
        0x89 => I64Rotl, 0x8A => I64Rotr,

        // f32 numeric
        0x8B => F32Abs, 0x8C => F32Neg,  0x8D => F32Ceil, 0x8E => F32Floor,
        0x8F => F32Trunc, 0x90 => F32Nearest, 0x91 => F32Sqrt,
        0x92 => F32Add, 0x93 => F32Sub, 0x94 => F32Mul, 0x95 => F32Div,
        0x96 => F32Min, 0x97 => F32Max, 0x98 => F32Copysign,

        // f64 numeric
        0x99 => F64Abs, 0x9A => F64Neg,  0x9B => F64Ceil, 0x9C => F64Floor,
        0x9D => F64Trunc, 0x9E => F64Nearest, 0x9F => F64Sqrt,
        0xA0 => F64Add, 0xA1 => F64Sub, 0xA2 => F64Mul, 0xA3 => F64Div,
        0xA4 => F64Min, 0xA5 => F64Max, 0xA6 => F64Copysign,

        // Conversions
        0xA7 => I32WrapI64,
        0xA8 => I32TruncF32S, 0xA9 => I32TruncF32U,
        0xAA => I32TruncF64S, 0xAB => I32TruncF64U,
        0xAC => I64ExtendI32S, 0xAD => I64ExtendI32U,
        0xAE => I64TruncF32S,  0xAF => I64TruncF32U,
        0xB0 => I64TruncF64S,  0xB1 => I64TruncF64U,
        0xB2 => F32ConvertI32S, 0xB3 => F32ConvertI32U,
        0xB4 => F32ConvertI64S, 0xB5 => F32ConvertI64U,
        0xB6 => F32DemoteF64,
        0xB7 => F64ConvertI32S, 0xB8 => F64ConvertI32U,
        0xB9 => F64ConvertI64S, 0xBA => F64ConvertI64U,
        0xBB => F64PromoteF32,
        0xBC => I32ReinterpretF32, 0xBD => I64ReinterpretF64,
        0xBE => F32ReinterpretI32, 0xBF => F64ReinterpretI64,

        other => return Err(WasmError::InvalidOpcode(other)),
    };
    Ok(Some(instr))
}

/// Evaluate a constant-expression (global init, data/elem offsets).
/// These are restricted to: i32.const, i64.const, f32.const, f64.const, end.
pub fn eval_const_expr(bytes: &[u8]) -> Result<Value, WasmError> {
    let mut r = Reader::new(bytes);
    let op    = r.read_byte()?;
    let val = match op {
        0x41 => Value::I32(r.read_i32()?),
        0x42 => Value::I64(r.read_i64()?),
        0x43 => Value::F32(r.read_f32()?),
        0x44 => Value::F64(r.read_f64()?),
        _    => return Err(WasmError::InvalidOpcode(op)),
    };
    // next byte should be 0x0B (End)
    Ok(val)
}
