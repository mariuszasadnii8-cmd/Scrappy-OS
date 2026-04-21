// ============================================================
//  scrappy-wasm/src/interp.rs
//  Stack-machine interpreter — pure no_std tree-walking eval.
//  Executes decoded WASM functions, manages locals/globals,
//  linear memory, and the call stack.
// ============================================================

use alloc::{vec, vec::Vec, boxed::Box};
use libm::{
    ceilf, floorf, truncf, roundf, sqrtf, fabsf, copysignf,
    ceil, floor, trunc, round, sqrt, fabs, copysign,
    fminf, fmaxf, fmin, fmax,
};

use crate::decode::{self, WasmModule, Instr, BlockType, eval_const_expr, Reader};
use crate::types::*;

pub const PAGE_SIZE: usize = 65_536;
const CALL_STACK_LIMIT: usize = 512;

// ── Linear memory ─────────────────────────────────────────────

pub struct Memory {
    pub data: Vec<u8>,
    pub max:  Option<u32>, // in pages
}

impl Memory {
    pub fn new(limits: &crate::types::Limits) -> Self {
        let pages = limits.min as usize;
        Memory {
            data: vec![0u8; pages * PAGE_SIZE],
            max:  limits.max,
        }
    }

    pub fn size_pages(&self) -> u32 { (self.data.len() / PAGE_SIZE) as u32 }

    pub fn grow(&mut self, delta: u32) -> i32 {
        let old  = self.size_pages();
        let new  = match old.checked_add(delta) { Some(n) => n, None => return -1 };
        if let Some(max) = self.max { if new > max { return -1; } }
        self.data.resize((new as usize) * PAGE_SIZE, 0);
        old as i32
    }

    fn check(&self, addr: u32, size: u32) -> Result<(), WasmError> {
        let end = (addr as usize).checked_add(size as usize)
            .ok_or(WasmError::MemoryOutOfBounds)?;
        if end > self.data.len() { Err(WasmError::Trap(TrapCode::MemoryOutOfBounds)) }
        else { Ok(()) }
    }

    pub fn load_u8(&self, addr: u32) -> Result<u8, WasmError> {
        self.check(addr, 1)?; Ok(self.data[addr as usize])
    }
    pub fn load_u16(&self, addr: u32) -> Result<u16, WasmError> {
        self.check(addr, 2)?;
        let a = addr as usize;
        Ok(u16::from_le_bytes([self.data[a], self.data[a+1]]))
    }
    pub fn load_u32(&self, addr: u32) -> Result<u32, WasmError> {
        self.check(addr, 4)?;
        let a = addr as usize;
        Ok(u32::from_le_bytes([self.data[a],self.data[a+1],self.data[a+2],self.data[a+3]]))
    }
    pub fn load_u64(&self, addr: u32) -> Result<u64, WasmError> {
        self.check(addr, 8)?;
        let a = addr as usize;
        Ok(u64::from_le_bytes(self.data[a..a+8].try_into().unwrap()))
    }
    pub fn store_u8(&mut self, addr: u32, v: u8) -> Result<(), WasmError> {
        self.check(addr, 1)?; self.data[addr as usize] = v; Ok(())
    }
    pub fn store_u16(&mut self, addr: u32, v: u16) -> Result<(), WasmError> {
        self.check(addr, 2)?;
        let a = addr as usize;
        let b = v.to_le_bytes();
        self.data[a] = b[0]; self.data[a+1] = b[1];
        Ok(())
    }
    pub fn store_u32(&mut self, addr: u32, v: u32) -> Result<(), WasmError> {
        self.check(addr, 4)?;
        let a = addr as usize;
        let b = v.to_le_bytes();
        self.data[a..a+4].copy_from_slice(&b); Ok(())
    }
    pub fn store_u64(&mut self, addr: u32, v: u64) -> Result<(), WasmError> {
        self.check(addr, 8)?;
        let a = addr as usize;
        self.data[a..a+8].copy_from_slice(&v.to_le_bytes()); Ok(())
    }
}

// ── Function representation ───────────────────────────────────

pub enum FuncKind {
    Local {
        ty_idx:  u32,
        body_idx: usize,        // index into WasmModule::codes
    },
    Host {
        /// Called by the interpreter. Returns result values or error.
        func: Box<dyn Fn(&[Value], &mut Memory) -> Result<Vec<Value>, WasmError>>,
    },
}

pub struct Func {
    pub ty:   FuncType,
    pub kind: FuncKind,
}

// ── Table ─────────────────────────────────────────────────────

pub struct Table {
    pub elems: Vec<Option<u32>>, // function indices (funcref)
}

impl Table {
    pub fn new(limits: &Limits) -> Self {
        Table { elems: vec![None; limits.min as usize] }
    }
}

// ── Module instance ───────────────────────────────────────────

pub struct ModuleInstance {
    pub module:  WasmModule,       // parsed module (owns the bytecode)
    pub funcs:   Vec<Func>,
    pub tables:  Vec<Table>,
    pub memories: Vec<Memory>,
    pub globals: Vec<GlobalInst>,
}

impl ModuleInstance {
    /// Instantiate a parsed module.  Host functions are provided via `imports`.
    pub fn instantiate(
        module: WasmModule,
        mut imports: Vec<Func>,
    ) -> Result<Self, WasmError> {
        // Build full function list: imports first, then local funcs
        let _import_func_count = module.imports.iter()
            .filter(|i| matches!(i.desc, ImportDesc::Func(_)))
            .count();

        let mut funcs: Vec<Func> = Vec::with_capacity(imports.len() + module.funcs.len());
        funcs.append(&mut imports);

        for (i, &ty_idx) in module.funcs.iter().enumerate() {
            let ty = module.types.get(ty_idx as usize)
                .ok_or(WasmError::InvalidType)?.clone();
            funcs.push(Func { ty, kind: FuncKind::Local { ty_idx, body_idx: i } });
        }

        // Tables
        let mut tables: Vec<Table> = module.tables.iter()
            .map(|t| Table::new(&t.limits))
            .collect();

        // Memories
        let mut memories: Vec<Memory> = module.mems.iter()
            .map(|m| Memory::new(&m.0))
            .collect();

        // Globals
        let mut globals: Vec<GlobalInst> = Vec::with_capacity(module.globals.len());
        for (gt, expr) in &module.globals {
            let value = eval_const_expr(expr)?;
            globals.push(GlobalInst { ty: *gt, value });
        }

        // Data segments
        for seg in &module.data {
            let offset = eval_const_expr(&seg.offset)?
                .as_i32().ok_or(WasmError::InvalidType)? as u32;
            let mem = memories.get_mut(seg.mem_idx as usize)
                .ok_or(WasmError::MemoryOutOfBounds)?;
            let end = (offset as usize) + seg.init.len();
            if end > mem.data.len() { return Err(WasmError::MemoryOutOfBounds); }
            mem.data[offset as usize..end].copy_from_slice(&seg.init);
        }

        // Element segments
        for seg in &module.elems {
            let offset = eval_const_expr(&seg.offset)?
                .as_i32().ok_or(WasmError::InvalidType)? as usize;
            let tab = tables.get_mut(seg.table_idx as usize)
                .ok_or(WasmError::MemoryOutOfBounds)?;
            for (j, &fi) in seg.indices.iter().enumerate() {
                let slot = offset + j;
                if slot >= tab.elems.len() { return Err(WasmError::MemoryOutOfBounds); }
                tab.elems[slot] = Some(fi);
            }
        }

        let mut inst = ModuleInstance { module, funcs, tables, memories, globals };

        // Run start function if present
        if let Some(start_idx) = inst.module.start {
            inst.invoke(start_idx as usize, &[])?;
        }

        Ok(inst)
    }

    /// Find a function by export name and invoke it.
    pub fn call_export(&mut self, name: &str, args: &[Value]) -> Result<Vec<Value>, WasmError> {
        let func_idx = self.module.exports.iter().find_map(|e| {
            if e.name == name { if let ExportDesc::Func(i) = e.desc { return Some(i); } }
            None
        }).ok_or(WasmError::ExportNotFound)?;
        self.invoke(func_idx as usize, args)
    }

    pub fn invoke(&mut self, func_idx: usize, args: &[Value]) -> Result<Vec<Value>, WasmError> {
        let (ty, body_idx) = match &self.funcs[func_idx].kind {
            FuncKind::Host { func: _ } => {
                let _mem = self.memories.get_mut(0);
                let dummy_mem: &mut Memory;
                // We need to call the host func; borrow-checker dance:
                // Since we need &mut Memory separately, we clone the Box briefly.
                // In a real kernel you'd use an unsafe cell or split borrows.
                return unsafe {
                    let f = &*(&self.funcs[func_idx].kind as *const FuncKind);
                    if let FuncKind::Host { func } = f {
                        if let Some(m) = self.memories.get_mut(0) {
                            func(args, m)
                        } else {
                            // no memory — still valid for host funcs
                            let mut dummy = Memory { data: vec![], max: None };
                            func(args, &mut dummy)
                        }
                    } else { unreachable!() }
                };
            }
            FuncKind::Local { ty_idx: _, body_idx: bi } => {
                (self.funcs[func_idx].ty.clone(), *bi)
            }
        };

        // Build initial locals: params + zeroed locals
        let body = &self.module.codes[body_idx];
        let mut locals: Vec<Value> = args.iter().copied().collect();
        // Pad missing params with defaults (shouldn't happen with well-typed modules)
        for (i, pt) in ty.params.iter().enumerate() {
            if i >= locals.len() { locals.push(Value::default_for(*pt)); }
        }
        for entry in &body.locals {
            for _ in 0..entry.count {
                locals.push(Value::default_for(entry.ty));
            }
        }

        // Run
        let code = body.code.clone(); // clone to avoid borrow of self
        let mut exec = Executor::new(locals, &ty.results);
        exec.run(self, &code, 0)
    }
}

// ── Control-flow stack ────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum CtrlOp { Block, Loop, If }

struct CtrlFrame {
    op:         CtrlOp,
    /// stack height at entry (we trim to this on break)
    stack_base: usize,
    /// result arity
    arity:      usize,
    /// byte offset of the `else` or `end` opcode (for `If`)
    else_pos:   Option<usize>,
    /// byte offset to jump to on `Br` (start of loop, or end of block)
    br_target:  usize,
}

// ── Executor ──────────────────────────────────────────────────

struct Executor {
    locals:    Vec<Value>,
    stack:     Vec<Value>,
    ctrl:      Vec<CtrlFrame>,
    result_ty: Vec<ValType>,
}

impl Executor {
    fn new(locals: Vec<Value>, result_ty: &[ValType]) -> Self {
        Executor {
            locals,
            stack: Vec::with_capacity(32),
            ctrl:  Vec::with_capacity(16),
            result_ty: result_ty.to_vec(),
        }
    }

    fn push(&mut self, v: Value) { self.stack.push(v); }

    fn pop(&mut self) -> Result<Value, WasmError> {
        self.stack.pop().ok_or(WasmError::StackUnderflow)
    }

    fn pop_i32(&mut self) -> Result<i32, WasmError> {
        self.pop()?.as_i32().ok_or(WasmError::TypeMismatch)
    }
    fn pop_i64(&mut self) -> Result<i64, WasmError> {
        self.pop()?.as_i64().ok_or(WasmError::TypeMismatch)
    }
    fn pop_f32(&mut self) -> Result<f32, WasmError> {
        self.pop()?.as_f32().ok_or(WasmError::TypeMismatch)
    }
    fn pop_f64(&mut self) -> Result<f64, WasmError> {
        self.pop()?.as_f64().ok_or(WasmError::TypeMismatch)
    }

    fn mem<'a>(inst: &'a mut ModuleInstance) -> Result<&'a mut Memory, WasmError> {
        inst.memories.get_mut(0).ok_or(WasmError::MemoryOutOfBounds)
    }

    // Break to label depth `depth`; returns true if the enclosing function should return.
    fn do_break(&mut self, depth: u32, code: &[u8], pc: &mut usize) -> Result<bool, WasmError> {
        let target_idx = self.ctrl.len().checked_sub(1 + depth as usize)
            .ok_or(WasmError::StackUnderflow)?;
        let frame = &self.ctrl[target_idx];
        let arity  = frame.arity;
        let base   = frame.stack_base;
        let is_loop = matches!(frame.op, CtrlOp::Loop);
        let target  = frame.br_target;

        // Preserve top `arity` values
        let top: Vec<Value> = self.stack[self.stack.len().saturating_sub(arity)..].to_vec();
        self.stack.truncate(base);
        self.stack.extend(top);

        if is_loop {
            // Jump back to start of loop body
            *pc = target;
            self.ctrl.truncate(target_idx + 1);
        } else {
            // Exit block; pop the frame
            *pc = target; // past the End
            self.ctrl.truncate(target_idx);
        }
        Ok(false)
    }

    /// Main interpreter loop.  We use a single flat code buffer and a
    /// `pc` cursor, avoiding recursion for blocks (only Call recurses).
    pub fn run(
        &mut self,
        inst: &mut ModuleInstance,
        code: &[u8],
        mut pc: usize,
    ) -> Result<Vec<Value>, WasmError> {

        loop {
            let mut r = Reader { data: code, pos: pc };
            let instr_opt = decode::decode_instr(&mut r)?;
            pc = r.pos;

            let instr = match instr_opt {
                None => break, // End of function body
                Some(i) => i,
            };

            use Instr::*;
            match instr {
                Unreachable => return Err(WasmError::Trap(TrapCode::Unreachable)),
                Nop => {}

                // ── Control ─────────────────────────────────────────
                Block(bt) => {
                    let arity = block_arity(&bt, &inst.module);
                    self.ctrl.push(CtrlFrame {
                        op: CtrlOp::Block,
                        stack_base: self.stack.len(),
                        arity,
                        else_pos: None,
                        br_target: find_end(code, pc)?,
                    });
                }
                Loop(bt) => {
                    let arity = 0; // loops break to their start with no results
                    self.ctrl.push(CtrlFrame {
                        op: CtrlOp::Loop,
                        stack_base: self.stack.len(),
                        arity,
                        else_pos: None,
                        br_target: pc,    // break → restart loop
                    });
                }
                If(bt) => {
                    let cond = self.pop_i32()?;
                    let arity = block_arity(&bt, &inst.module);
                    let (else_pos, end_pos) = find_else_end(code, pc)?;
                    self.ctrl.push(CtrlFrame {
                        op: CtrlOp::If,
                        stack_base: self.stack.len(),
                        arity,
                        else_pos,
                        br_target: end_pos,
                    });
                    if cond == 0 {
                        // Jump to else or end
                        pc = else_pos.unwrap_or(end_pos);
                    }
                }
                Else => {
                    // Reached the `else` opcode from the true branch — jump past end
                    if let Some(frame) = self.ctrl.last() {
                        pc = frame.br_target;
                    }
                }
                Instr::End => {
                    // Structured block ended naturally
                    if let Some(frame) = self.ctrl.pop() {
                        let arity = frame.arity;
                        let base  = frame.stack_base;
                        let top: Vec<Value> = self.stack[self.stack.len().saturating_sub(arity)..].to_vec();
                        self.stack.truncate(base);
                        self.stack.extend(top);
                    } else {
                        break; // end of function
                    }
                }

                Br(depth)    => { self.do_break(depth, code, &mut pc)?; }
                BrIf(depth)  => {
                    let cond = self.pop_i32()?;
                    if cond != 0 { self.do_break(depth, code, &mut pc)?; }
                }
                BrTable(labels, default) => {
                    let idx = self.pop_i32()? as usize;
                    let depth = if idx < labels.len() { labels[idx] } else { default };
                    self.do_break(depth, code, &mut pc)?;
                }
                Return => {
                    let arity = self.result_ty.len();
                    let top: Vec<Value> = self.stack[self.stack.len().saturating_sub(arity)..].to_vec();
                    return Ok(top);
                }

                Call(fi) => {
                    let ft = inst.funcs.get(fi as usize)
                        .map(|f| f.ty.clone())
                        .ok_or(WasmError::UndefinedFunc(fi))?;
                    let n = ft.params.len();
                    let args_start = self.stack.len().checked_sub(n)
                        .ok_or(WasmError::StackUnderflow)?;
                    let args: Vec<Value> = self.stack.drain(args_start..).collect();
                    let results = inst.invoke(fi as usize, &args)?;
                    self.stack.extend(results);
                }

                CallIndirect(ti, tab_idx) => {
                    let idx    = self.pop_i32()? as usize;
                    let tab    = inst.tables.get(tab_idx as usize)
                        .ok_or(WasmError::UndefinedFunc(tab_idx))?;
                    let fi     = tab.elems.get(idx)
                        .and_then(|x| *x)
                        .ok_or(WasmError::Trap(TrapCode::IndirectCallTypeMismatch))?;
                    let expected_ty = inst.module.types.get(ti as usize)
                        .ok_or(WasmError::InvalidType)?;
                    let actual_ty = &inst.funcs[fi as usize].ty;
                    if actual_ty != expected_ty {
                        return Err(WasmError::Trap(TrapCode::IndirectCallTypeMismatch));
                    }
                    let n = expected_ty.params.len();
                    let args_start = self.stack.len().checked_sub(n)
                        .ok_or(WasmError::StackUnderflow)?;
                    let args: Vec<Value> = self.stack.drain(args_start..).collect();
                    let results = inst.invoke(fi as usize, &args)?;
                    self.stack.extend(results);
                }

                // ── Parametric ──────────────────────────────────────
                Drop => { self.pop()?; }
                Select => {
                    let cond = self.pop_i32()?;
                    let b    = self.pop()?;
                    let a    = self.pop()?;
                    self.push(if cond != 0 { a } else { b });
                }

                // ── Variables ───────────────────────────────────────
                LocalGet(i) => {
                    let v = *self.locals.get(i as usize)
                        .ok_or(WasmError::UndefinedLocal(i))?;
                    self.push(v);
                }
                LocalSet(i) => {
                    let v = self.pop()?;
                    *self.locals.get_mut(i as usize)
                        .ok_or(WasmError::UndefinedLocal(i))? = v;
                }
                LocalTee(i) => {
                    let v = *self.stack.last().ok_or(WasmError::StackUnderflow)?;
                    *self.locals.get_mut(i as usize)
                        .ok_or(WasmError::UndefinedLocal(i))? = v;
                }
                GlobalGet(i) => {
                    let g = inst.globals.get(i as usize)
                        .ok_or(WasmError::UndefinedGlobal(i))?;
                    self.push(g.value);
                }
                GlobalSet(i) => {
                    let v = self.pop()?;
                    let g = inst.globals.get_mut(i as usize)
                        .ok_or(WasmError::UndefinedGlobal(i))?;
                    g.value = v;
                }

                // ── Memory loads ────────────────────────────────────
                I32Load(ma)    => { let a = ea(self.pop_i32()?, &ma); let v = Self::mem(inst)?.load_u32(a)?; self.push(Value::I32(v as i32)); }
                I64Load(ma)    => { let a = ea(self.pop_i32()?, &ma); let v = Self::mem(inst)?.load_u64(a)?; self.push(Value::I64(v as i64)); }
                F32Load(ma)    => { let a = ea(self.pop_i32()?, &ma); let v = Self::mem(inst)?.load_u32(a)?; self.push(Value::F32(v)); }
                F64Load(ma)    => { let a = ea(self.pop_i32()?, &ma); let v = Self::mem(inst)?.load_u64(a)?; self.push(Value::F64(v)); }
                I32Load8S(ma)  => { let a = ea(self.pop_i32()?, &ma); let v = Self::mem(inst)?.load_u8(a)?  as i8  as i32; self.push(Value::I32(v)); }
                I32Load8U(ma)  => { let a = ea(self.pop_i32()?, &ma); let v = Self::mem(inst)?.load_u8(a)?  as i32; self.push(Value::I32(v)); }
                I32Load16S(ma) => { let a = ea(self.pop_i32()?, &ma); let v = Self::mem(inst)?.load_u16(a)? as i16 as i32; self.push(Value::I32(v)); }
                I32Load16U(ma) => { let a = ea(self.pop_i32()?, &ma); let v = Self::mem(inst)?.load_u16(a)? as i32; self.push(Value::I32(v)); }
                I64Load8S(ma)  => { let a = ea(self.pop_i32()?, &ma); let v = Self::mem(inst)?.load_u8(a)?  as i8  as i64; self.push(Value::I64(v)); }
                I64Load8U(ma)  => { let a = ea(self.pop_i32()?, &ma); let v = Self::mem(inst)?.load_u8(a)?  as i64; self.push(Value::I64(v)); }
                I64Load16S(ma) => { let a = ea(self.pop_i32()?, &ma); let v = Self::mem(inst)?.load_u16(a)? as i16 as i64; self.push(Value::I64(v)); }
                I64Load16U(ma) => { let a = ea(self.pop_i32()?, &ma); let v = Self::mem(inst)?.load_u16(a)? as i64; self.push(Value::I64(v)); }
                I64Load32S(ma) => { let a = ea(self.pop_i32()?, &ma); let v = Self::mem(inst)?.load_u32(a)? as i32 as i64; self.push(Value::I64(v)); }
                I64Load32U(ma) => { let a = ea(self.pop_i32()?, &ma); let v = Self::mem(inst)?.load_u32(a)? as i64; self.push(Value::I64(v)); }

                // ── Memory stores ───────────────────────────────────
                I32Store(ma)   => { let v = self.pop_i32()? as u32;  let a = ea(self.pop_i32()?, &ma); Self::mem(inst)?.store_u32(a, v)?; }
                I64Store(ma)   => { let v = self.pop_i64()? as u64;  let a = ea(self.pop_i32()?, &ma); Self::mem(inst)?.store_u64(a, v)?; }
                F32Store(ma)   => { let v = self.pop()?.as_f32b().ok_or(WasmError::TypeMismatch)?; let a = ea(self.pop_i32()?, &ma); Self::mem(inst)?.store_u32(a, v)?; }
                F64Store(ma)   => { let v = self.pop()?.as_f64b().ok_or(WasmError::TypeMismatch)?; let a = ea(self.pop_i32()?, &ma); Self::mem(inst)?.store_u64(a, v)?; }
                I32Store8(ma)  => { let v = self.pop_i32()? as u8;   let a = ea(self.pop_i32()?, &ma); Self::mem(inst)?.store_u8(a, v)?; }
                I32Store16(ma) => { let v = self.pop_i32()? as u16;  let a = ea(self.pop_i32()?, &ma); Self::mem(inst)?.store_u16(a, v)?; }
                I64Store8(ma)  => { let v = self.pop_i64()? as u8;   let a = ea(self.pop_i32()?, &ma); Self::mem(inst)?.store_u8(a, v)?; }
                I64Store16(ma) => { let v = self.pop_i64()? as u16;  let a = ea(self.pop_i32()?, &ma); Self::mem(inst)?.store_u16(a, v)?; }
                I64Store32(ma) => { let v = self.pop_i64()? as u32;  let a = ea(self.pop_i32()?, &ma); Self::mem(inst)?.store_u32(a, v)?; }

                MemorySize => { let p = inst.memories.get(0).map(|m| m.size_pages()).unwrap_or(0); self.push(Value::I32(p as i32)); }
                MemoryGrow => { let d = self.pop_i32()? as u32; let r = inst.memories.get_mut(0).map(|m| m.grow(d)).unwrap_or(-1); self.push(Value::I32(r)); }

                // ── Constants ───────────────────────────────────────
                I32Const(v) => self.push(Value::I32(v)),
                I64Const(v) => self.push(Value::I64(v)),
                F32Const(v) => self.push(Value::F32(v)),
                F64Const(v) => self.push(Value::F64(v)),

                // ── i32 compare ─────────────────────────────────────
                I32Eqz => { let a = self.pop_i32()?; self.push(Value::I32((a == 0) as i32)); }
                I32Eq  => { let (a,b) = pop2i32(self)?; self.push(Value::I32((a==b) as i32)); }
                I32Ne  => { let (a,b) = pop2i32(self)?; self.push(Value::I32((a!=b) as i32)); }
                I32LtS => { let (a,b) = pop2i32(self)?; self.push(Value::I32((a< b) as i32)); }
                I32LtU => { let (a,b) = pop2u32(self)?; self.push(Value::I32((a< b) as i32)); }
                I32GtS => { let (a,b) = pop2i32(self)?; self.push(Value::I32((a> b) as i32)); }
                I32GtU => { let (a,b) = pop2u32(self)?; self.push(Value::I32((a> b) as i32)); }
                I32LeS => { let (a,b) = pop2i32(self)?; self.push(Value::I32((a<=b) as i32)); }
                I32LeU => { let (a,b) = pop2u32(self)?; self.push(Value::I32((a<=b) as i32)); }
                I32GeS => { let (a,b) = pop2i32(self)?; self.push(Value::I32((a>=b) as i32)); }
                I32GeU => { let (a,b) = pop2u32(self)?; self.push(Value::I32((a>=b) as i32)); }

                // ── i64 compare ─────────────────────────────────────
                I64Eqz => { let a = self.pop_i64()?; self.push(Value::I32((a==0) as i32)); }
                I64Eq  => { let (a,b) = pop2i64(self)?; self.push(Value::I32((a==b) as i32)); }
                I64Ne  => { let (a,b) = pop2i64(self)?; self.push(Value::I32((a!=b) as i32)); }
                I64LtS => { let (a,b) = pop2i64(self)?; self.push(Value::I32((a< b) as i32)); }
                I64LtU => { let (a,b) = pop2u64(self)?; self.push(Value::I32((a< b) as i32)); }
                I64GtS => { let (a,b) = pop2i64(self)?; self.push(Value::I32((a> b) as i32)); }
                I64GtU => { let (a,b) = pop2u64(self)?; self.push(Value::I32((a> b) as i32)); }
                I64LeS => { let (a,b) = pop2i64(self)?; self.push(Value::I32((a<=b) as i32)); }
                I64LeU => { let (a,b) = pop2u64(self)?; self.push(Value::I32((a<=b) as i32)); }
                I64GeS => { let (a,b) = pop2i64(self)?; self.push(Value::I32((a>=b) as i32)); }
                I64GeU => { let (a,b) = pop2u64(self)?; self.push(Value::I32((a>=b) as i32)); }

                // ── f32 compare ─────────────────────────────────────
                F32Eq => { let (a,b) = pop2f32(self)?; self.push(Value::I32((a==b) as i32)); }
                F32Ne => { let (a,b) = pop2f32(self)?; self.push(Value::I32((a!=b) as i32)); }
                F32Lt => { let (a,b) = pop2f32(self)?; self.push(Value::I32((a< b) as i32)); }
                F32Gt => { let (a,b) = pop2f32(self)?; self.push(Value::I32((a> b) as i32)); }
                F32Le => { let (a,b) = pop2f32(self)?; self.push(Value::I32((a<=b) as i32)); }
                F32Ge => { let (a,b) = pop2f32(self)?; self.push(Value::I32((a>=b) as i32)); }

                // ── f64 compare ─────────────────────────────────────
                F64Eq => { let (a,b) = pop2f64(self)?; self.push(Value::I32((a==b) as i32)); }
                F64Ne => { let (a,b) = pop2f64(self)?; self.push(Value::I32((a!=b) as i32)); }
                F64Lt => { let (a,b) = pop2f64(self)?; self.push(Value::I32((a< b) as i32)); }
                F64Gt => { let (a,b) = pop2f64(self)?; self.push(Value::I32((a> b) as i32)); }
                F64Le => { let (a,b) = pop2f64(self)?; self.push(Value::I32((a<=b) as i32)); }
                F64Ge => { let (a,b) = pop2f64(self)?; self.push(Value::I32((a>=b) as i32)); }

                // ── i32 numeric ─────────────────────────────────────
                I32Clz    => { let a = self.pop_i32()?; self.push(Value::I32(a.leading_zeros()  as i32)); }
                I32Ctz    => { let a = self.pop_i32()?; self.push(Value::I32(a.trailing_zeros() as i32)); }
                I32Popcnt => { let a = self.pop_i32()?; self.push(Value::I32(a.count_ones()     as i32)); }
                I32Add  => { let (a,b) = pop2i32(self)?; self.push(Value::I32(a.wrapping_add(b))); }
                I32Sub  => { let (a,b) = pop2i32(self)?; self.push(Value::I32(a.wrapping_sub(b))); }
                I32Mul  => { let (a,b) = pop2i32(self)?; self.push(Value::I32(a.wrapping_mul(b))); }
                I32DivS => { let (a,b) = pop2i32(self)?; if b==0 { return Err(WasmError::Trap(TrapCode::DivisionByZero)); } if a==i32::MIN && b==-1 { return Err(WasmError::Trap(TrapCode::IntegerOverflow)); } self.push(Value::I32(a/b)); }
                I32DivU => { let (a,b) = pop2u32(self)?; if b==0 { return Err(WasmError::Trap(TrapCode::DivisionByZero)); } self.push(Value::I32((a/b) as i32)); }
                I32RemS => { let (a,b) = pop2i32(self)?; if b==0 { return Err(WasmError::Trap(TrapCode::DivisionByZero)); } self.push(Value::I32(a.wrapping_rem(b))); }
                I32RemU => { let (a,b) = pop2u32(self)?; if b==0 { return Err(WasmError::Trap(TrapCode::DivisionByZero)); } self.push(Value::I32((a%b) as i32)); }
                I32And  => { let (a,b) = pop2i32(self)?; self.push(Value::I32(a&b)); }
                I32Or   => { let (a,b) = pop2i32(self)?; self.push(Value::I32(a|b)); }
                I32Xor  => { let (a,b) = pop2i32(self)?; self.push(Value::I32(a^b)); }
                I32Shl  => { let (a,b) = pop2i32(self)?; self.push(Value::I32(a.wrapping_shl(b as u32))); }
                I32ShrS => { let (a,b) = pop2i32(self)?; self.push(Value::I32(a.wrapping_shr(b as u32))); }
                I32ShrU => { let (a,b) = pop2u32(self)?; self.push(Value::I32(a.wrapping_shr(b) as i32)); }
                I32Rotl => { let (a,b) = pop2u32(self)?; self.push(Value::I32(a.rotate_left(b) as i32)); }
                I32Rotr => { let (a,b) = pop2u32(self)?; self.push(Value::I32(a.rotate_right(b) as i32)); }

                // ── i64 numeric ─────────────────────────────────────
                I64Clz    => { let a = self.pop_i64()?; self.push(Value::I64(a.leading_zeros()  as i64)); }
                I64Ctz    => { let a = self.pop_i64()?; self.push(Value::I64(a.trailing_zeros() as i64)); }
                I64Popcnt => { let a = self.pop_i64()?; self.push(Value::I64(a.count_ones()     as i64)); }
                I64Add  => { let (a,b) = pop2i64(self)?; self.push(Value::I64(a.wrapping_add(b))); }
                I64Sub  => { let (a,b) = pop2i64(self)?; self.push(Value::I64(a.wrapping_sub(b))); }
                I64Mul  => { let (a,b) = pop2i64(self)?; self.push(Value::I64(a.wrapping_mul(b))); }
                I64DivS => { let (a,b) = pop2i64(self)?; if b==0 { return Err(WasmError::Trap(TrapCode::DivisionByZero)); } if a==i64::MIN && b==-1 { return Err(WasmError::Trap(TrapCode::IntegerOverflow)); } self.push(Value::I64(a/b)); }
                I64DivU => { let (a,b) = pop2u64(self)?; if b==0 { return Err(WasmError::Trap(TrapCode::DivisionByZero)); } self.push(Value::I64((a/b) as i64)); }
                I64RemS => { let (a,b) = pop2i64(self)?; if b==0 { return Err(WasmError::Trap(TrapCode::DivisionByZero)); } self.push(Value::I64(a.wrapping_rem(b))); }
                I64RemU => { let (a,b) = pop2u64(self)?; if b==0 { return Err(WasmError::Trap(TrapCode::DivisionByZero)); } self.push(Value::I64((a%b) as i64)); }
                I64And  => { let (a,b) = pop2i64(self)?; self.push(Value::I64(a&b)); }
                I64Or   => { let (a,b) = pop2i64(self)?; self.push(Value::I64(a|b)); }
                I64Xor  => { let (a,b) = pop2i64(self)?; self.push(Value::I64(a^b)); }
                I64Shl  => { let (a,b) = pop2i64(self)?; self.push(Value::I64(a.wrapping_shl(b as u32))); }
                I64ShrS => { let (a,b) = pop2i64(self)?; self.push(Value::I64(a.wrapping_shr(b as u32))); }
                I64ShrU => { let (a,b) = pop2u64(self)?; self.push(Value::I64(a.wrapping_shr(b as u32) as i64)); }
                I64Rotl => { let (a,b) = pop2u64(self)?; self.push(Value::I64(a.rotate_left(b as u32) as i64)); }
                I64Rotr => { let (a,b) = pop2u64(self)?; self.push(Value::I64(a.rotate_right(b as u32) as i64)); }

                // ── f32 numeric ─────────────────────────────────────
                F32Abs  => { let a = self.pop_f32()?; self.push(Value::F32(fabsf(a).to_bits())); }
                F32Neg  => { let a = self.pop_f32()?; self.push(Value::F32((-a).to_bits())); }
                F32Ceil => { let a = self.pop_f32()?; self.push(Value::F32(ceilf(a).to_bits())); }
                F32Floor=> { let a = self.pop_f32()?; self.push(Value::F32(floorf(a).to_bits())); }
                F32Trunc=> { let a = self.pop_f32()?; self.push(Value::F32(truncf(a).to_bits())); }
                F32Nearest=>{ let a = self.pop_f32()?; self.push(Value::F32(roundf(a).to_bits())); }
                F32Sqrt => { let a = self.pop_f32()?; self.push(Value::F32(sqrtf(a).to_bits())); }
                F32Add  => { let (a,b) = pop2f32(self)?; self.push(Value::F32((a+b).to_bits())); }
                F32Sub  => { let (a,b) = pop2f32(self)?; self.push(Value::F32((a-b).to_bits())); }
                F32Mul  => { let (a,b) = pop2f32(self)?; self.push(Value::F32((a*b).to_bits())); }
                F32Div  => { let (a,b) = pop2f32(self)?; self.push(Value::F32((a/b).to_bits())); }
                F32Min  => { let (a,b) = pop2f32(self)?; self.push(Value::F32(fminf(a,b).to_bits())); }
                F32Max  => { let (a,b) = pop2f32(self)?; self.push(Value::F32(fmaxf(a,b).to_bits())); }
                F32Copysign => { let (a,b) = pop2f32(self)?; self.push(Value::F32(copysignf(a,b).to_bits())); }

                // ── f64 numeric ─────────────────────────────────────
                F64Abs  => { let a = self.pop_f64()?; self.push(Value::F64(fabs(a).to_bits())); }
                F64Neg  => { let a = self.pop_f64()?; self.push(Value::F64((-a).to_bits())); }
                F64Ceil => { let a = self.pop_f64()?; self.push(Value::F64(ceil(a).to_bits())); }
                F64Floor=> { let a = self.pop_f64()?; self.push(Value::F64(floor(a).to_bits())); }
                F64Trunc=> { let a = self.pop_f64()?; self.push(Value::F64(trunc(a).to_bits())); }
                F64Nearest=>{ let a = self.pop_f64()?; self.push(Value::F64(round(a).to_bits())); }
                F64Sqrt => { let a = self.pop_f64()?; self.push(Value::F64(sqrt(a).to_bits())); }
                F64Add  => { let (a,b) = pop2f64(self)?; self.push(Value::F64((a+b).to_bits())); }
                F64Sub  => { let (a,b) = pop2f64(self)?; self.push(Value::F64((a-b).to_bits())); }
                F64Mul  => { let (a,b) = pop2f64(self)?; self.push(Value::F64((a*b).to_bits())); }
                F64Div  => { let (a,b) = pop2f64(self)?; self.push(Value::F64((a/b).to_bits())); }
                F64Min  => { let (a,b) = pop2f64(self)?; self.push(Value::F64(fmin(a,b).to_bits())); }
                F64Max  => { let (a,b) = pop2f64(self)?; self.push(Value::F64(fmax(a,b).to_bits())); }
                F64Copysign => { let (a,b) = pop2f64(self)?; self.push(Value::F64(copysign(a,b).to_bits())); }

                // ── Conversions ─────────────────────────────────────
                I32WrapI64     => { let a = self.pop_i64()?; self.push(Value::I32(a as i32)); }
                I32TruncF32S   => { let a = self.pop_f32()?; self.push(Value::I32(trunc_f32_i32s(a)?)); }
                I32TruncF32U   => { let a = self.pop_f32()?; self.push(Value::I32(trunc_f32_u32(a)? as i32)); }
                I32TruncF64S   => { let a = self.pop_f64()?; self.push(Value::I32(trunc_f64_i32s(a)?)); }
                I32TruncF64U   => { let a = self.pop_f64()?; self.push(Value::I32(trunc_f64_u32(a)? as i32)); }
                I64ExtendI32S  => { let a = self.pop_i32()?; self.push(Value::I64(a as i64)); }
                I64ExtendI32U  => { let a = self.pop_i32()?; self.push(Value::I64((a as u32) as i64)); }
                I64TruncF32S   => { let a = self.pop_f32()?; self.push(Value::I64(a as i64)); }
                I64TruncF32U   => { let a = self.pop_f32()?; self.push(Value::I64((a as u64) as i64)); }
                I64TruncF64S   => { let a = self.pop_f64()?; self.push(Value::I64(a as i64)); }
                I64TruncF64U   => { let a = self.pop_f64()?; self.push(Value::I64((a as u64) as i64)); }
                F32ConvertI32S => { let a = self.pop_i32()?; self.push(Value::F32((a as f32).to_bits())); }
                F32ConvertI32U => { let a = self.pop_i32()?; self.push(Value::F32((a as u32 as f32).to_bits())); }
                F32ConvertI64S => { let a = self.pop_i64()?; self.push(Value::F32((a as f32).to_bits())); }
                F32ConvertI64U => { let a = self.pop_i64()?; self.push(Value::F32((a as u64 as f32).to_bits())); }
                F32DemoteF64   => { let a = self.pop_f64()?; self.push(Value::F32((a as f32).to_bits())); }
                F64ConvertI32S => { let a = self.pop_i32()?; self.push(Value::F64((a as f64).to_bits())); }
                F64ConvertI32U => { let a = self.pop_i32()?; self.push(Value::F64((a as u32 as f64).to_bits())); }
                F64ConvertI64S => { let a = self.pop_i64()?; self.push(Value::F64((a as f64).to_bits())); }
                F64ConvertI64U => { let a = self.pop_i64()?; self.push(Value::F64((a as u64 as f64).to_bits())); }
                F64PromoteF32  => { let a = self.pop_f32()?; self.push(Value::F64((a as f64).to_bits())); }
                I32ReinterpretF32 => { let v = self.pop()?.as_f32b().ok_or(WasmError::TypeMismatch)?; self.push(Value::I32(v as i32)); }
                I64ReinterpretF64 => { let v = self.pop()?.as_f64b().ok_or(WasmError::TypeMismatch)?; self.push(Value::I64(v as i64)); }
                F32ReinterpretI32 => { let v = self.pop_i32()? as u32; self.push(Value::F32(v)); }
                F64ReinterpretI64 => { let v = self.pop_i64()? as u64; self.push(Value::F64(v)); }
            }
        }

        // Function ended — collect results
        let arity = self.result_ty.len();
        let top: Vec<Value> = self.stack[self.stack.len().saturating_sub(arity)..].to_vec();
        Ok(top)
    }
}

// ── Helpers ───────────────────────────────────────────────────

fn ea(base: i32, ma: &crate::decode::MemArg) -> u32 {
    (base as u32).wrapping_add(ma.offset)
}

fn block_arity(bt: &BlockType, module: &WasmModule) -> usize {
    match bt {
        BlockType::Empty => 0,
        BlockType::Val(_) => 1,
        BlockType::TypeIdx(i) => {
            module.types.get(*i as usize).map(|t| t.results.len()).unwrap_or(0)
        }
    }
}

/// Scan forward from `pc` to find the matching `End` opcode (0x0B),
/// returning its position (the byte *after* 0x0B = where to continue).
fn find_end(code: &[u8], mut pc: usize) -> Result<usize, WasmError> {
    let mut depth = 1usize;
    while pc < code.len() {
        match code[pc] {
            0x02 | 0x03 | 0x04 => { depth += 1; pc += 1; skip_blocktype(code, &mut pc); }
            0x0B => {
                pc += 1;
                depth -= 1;
                if depth == 0 { return Ok(pc); }
            }
            b => { pc += 1; skip_imm(b, code, &mut pc); }
        }
    }
    Err(WasmError::UnexpectedEof)
}

/// Returns (else_byte_pos, end_byte_pos) for an `if` block.
fn find_else_end(code: &[u8], mut pc: usize) -> Result<(Option<usize>, usize), WasmError> {
    let mut depth  = 1usize;
    let mut else_p = None;
    while pc < code.len() {
        match code[pc] {
            0x02 | 0x03 | 0x04 => { depth += 1; pc += 1; skip_blocktype(code, &mut pc); }
            0x05 if depth == 1 => { else_p = Some(pc + 1); pc += 1; } // Else
            0x0B => {
                pc += 1;
                depth -= 1;
                if depth == 0 { return Ok((else_p, pc)); }
            }
            b => { pc += 1; skip_imm(b, code, &mut pc); }
        }
    }
    Err(WasmError::UnexpectedEof)
}

fn skip_blocktype(code: &[u8], pc: &mut usize) {
    if *pc < code.len() {
        let b = code[*pc];
        if b == 0x40 || b == 0x7F || b == 0x7E || b == 0x7D || b == 0x7C {
            *pc += 1;
        } else {
            // signed LEB128 type index
            skip_leb(code, pc);
        }
    }
}

fn skip_leb(code: &[u8], pc: &mut usize) {
    while *pc < code.len() {
        let b = code[*pc]; *pc += 1;
        if b & 0x80 == 0 { break; }
    }
}

/// Skip the immediate operands of opcode `b` (after the opcode byte).
fn skip_imm(b: u8, code: &[u8], pc: &mut usize) {
    match b {
        0x0C | 0x0D | 0x10 | 0x20..=0x24 => skip_leb(code, pc),
        0x0E => {
            // br_table: count LEB + that many LEBs + default LEB
            let mut c = 0u32; let mut sh = 0;
            while *pc < code.len() { let x = code[*pc]; *pc+=1; c |= ((x&0x7F) as u32)<<sh; sh+=7; if x&0x80==0 { break; } }
            for _ in 0..=c { skip_leb(code, pc); }
        }
        0x11 => { skip_leb(code, pc); skip_leb(code, pc); }
        0x28..=0x3E => { skip_leb(code, pc); skip_leb(code, pc); } // memarg
        0x3F | 0x40 => { *pc += 1; }   // memory.size / memory.grow
        0x41 => skip_leb(code, pc),    // i32.const
        0x42 => skip_leb(code, pc),    // i64.const
        0x43 => *pc += 4,              // f32.const
        0x44 => *pc += 8,              // f64.const
        _ => {}
    }
}

// Pop helpers
fn pop2i32(e: &mut Executor) -> Result<(i32,i32), WasmError> {
    let b = e.pop_i32()?; let a = e.pop_i32()?; Ok((a,b))
}
fn pop2u32(e: &mut Executor) -> Result<(u32,u32), WasmError> {
    let b = e.pop_i32()? as u32; let a = e.pop_i32()? as u32; Ok((a,b))
}
fn pop2i64(e: &mut Executor) -> Result<(i64,i64), WasmError> {
    let b = e.pop_i64()?; let a = e.pop_i64()?; Ok((a,b))
}
fn pop2u64(e: &mut Executor) -> Result<(u64,u64), WasmError> {
    let b = e.pop_i64()? as u64; let a = e.pop_i64()? as u64; Ok((a,b))
}
fn pop2f32(e: &mut Executor) -> Result<(f32,f32), WasmError> {
    let b = e.pop_f32()?; let a = e.pop_f32()?; Ok((a,b))
}
fn pop2f64(e: &mut Executor) -> Result<(f64,f64), WasmError> {
    let b = e.pop_f64()?; let a = e.pop_f64()?; Ok((a,b))
}

// Checked float→int conversions per WASM spec
fn trunc_f32_i32s(v: f32) -> Result<i32, WasmError> {
    if v.is_nan() || v >= 2147483648.0f32 || v < -2147483648.0f32 {
        Err(WasmError::Trap(TrapCode::InvalidConversionToInt))
    } else { Ok(v as i32) }
}
fn trunc_f32_u32(v: f32) -> Result<u32, WasmError> {
    if v.is_nan() || v >= 4294967296.0f32 || v < 0.0 {
        Err(WasmError::Trap(TrapCode::InvalidConversionToInt))
    } else { Ok(v as u32) }
}
fn trunc_f64_i32s(v: f64) -> Result<i32, WasmError> {
    if v.is_nan() || v >= 2147483648.0f64 || v < -2147483648.0f64 {
        Err(WasmError::Trap(TrapCode::InvalidConversionToInt))
    } else { Ok(v as i32) }
}
fn trunc_f64_u32(v: f64) -> Result<u32, WasmError> {
    if v.is_nan() || v >= 4294967296.0f64 || v < 0.0 {
        Err(WasmError::Trap(TrapCode::InvalidConversionToInt))
    } else { Ok(v as u32) }
}
