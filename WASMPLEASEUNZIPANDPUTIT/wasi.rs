// ============================================================
//  scrappy-wasm/src/wasi.rs
//
//  Maps wasi_snapshot_preview1 host imports to Scrappy OS
//  "Email" messages.  Every WASI syscall is turned into an
//  EmailMessage, handed to a kernel mailbox, and the response
//  is read back synchronously.
//
//  WASI functions implemented:
//    fd_write          → Email to kernel cell (stdout/stderr)
//    fd_read           → Email to kernel cell (stdin)
//    fd_close          → Email notify (fd release)
//    path_open         → Email to FS cell
//    proc_exit         → Email SIGTERM to scheduler cell
//    args_get          → Returns pre-loaded arg pointers
//    args_sizes_get    → Returns arg metadata
//    environ_get       → Returns env pointer pairs
//    environ_sizes_get → Returns env metadata
//    clock_time_get    → Returns tick count from timer cell
//    random_get        → Asks entropy cell for bytes
//    poll_oneoff       → Minimal implementation (immediate timeout)
// ============================================================

use alloc::{vec, vec::Vec, string::String, boxed::Box};
use crate::types::*;
use crate::interp::{Func, FuncKind, Memory};

// ── Kernel "well-known" cell addresses ───────────────────────

pub const CELL_SCHEDULER: CellId = CellId(0);
pub const CELL_FS:        CellId = CellId(1);
pub const CELL_STDIN:     CellId = CellId(2);
pub const CELL_STDOUT:    CellId = CellId(3);
pub const CELL_STDERR:    CellId = CellId(4);
pub const CELL_TIMER:     CellId = CellId(5);
pub const CELL_ENTROPY:   CellId = CellId(6);

// ── Mailbox trait (kernel provides the implementation) ───────

/// The kernel implements this trait and passes it into the WASI builder.
/// In Scrappy OS this is a lock-free ring-buffer mailbox per cell.
pub trait Mailbox: Send + Sync {
    /// Send a message and block until a reply arrives.
    fn send_recv(&self, msg: EmailMessage) -> Vec<u8>;

    /// Non-blocking peek: returns bytes available in the inbox.
    fn bytes_ready(&self, from: CellId) -> usize;

    /// Return the current monotonic tick (nanoseconds since boot).
    fn now_ns(&self) -> u64;
}

// ── WASI context passed to every host function ───────────────

pub struct WasiCtx {
    pub cell:    CellId,
    pub args:    Vec<Vec<u8>>,   // process arguments (null-terminated)
    pub env:     Vec<Vec<u8>>,   // "KEY=VALUE\0" pairs
    pub mailbox: Box<dyn Mailbox>,
}

impl WasiCtx {
    pub fn new(cell: CellId, args: Vec<Vec<u8>>, env: Vec<Vec<u8>>, mailbox: Box<dyn Mailbox>) -> Self {
        WasiCtx { cell, args, env, mailbox }
    }
}

// ── fd → cell mapping ─────────────────────────────────────────
// WASI fd 0 = stdin, 1 = stdout, 2 = stderr.
// Higher fds route to the FS cell.

fn fd_to_cell(fd: i32) -> CellId {
    match fd {
        0 => CELL_STDIN,
        1 => CELL_STDOUT,
        2 => CELL_STDERR,
        _ => CELL_FS,
    }
}

// ── Helper: read iovec list from WASM memory ─────────────────

/// Returns concatenated data from a WASM iovec array.
/// iovec: { ptr: i32, len: i32 }  (8 bytes each)
fn read_iovs(mem: &Memory, iov_ptr: u32, iov_cnt: u32) -> Result<Vec<u8>, WasmError> {
    let mut out = Vec::new();
    for i in 0..iov_cnt {
        let base   = iov_ptr + i * 8;
        let ptr    = mem.load_u32(base)?;
        let len    = mem.load_u32(base + 4)?;
        for j in 0..len {
            out.push(mem.load_u8(ptr + j)?);
        }
    }
    Ok(out)
}

fn write_u32_le(mem: &mut Memory, addr: u32, v: u32) -> Result<(), WasmError> {
    mem.store_u32(addr, v)
}

// ── Build the complete WASI host-function list ────────────────
//
//  Returns a Vec<(module_name, func_name, Func)> that the runner
//  splices into the module's import list.
//
//  We use Arc<Mutex<WasiCtx>>-style sharing but — since we are
//  no_std without std::sync — we pass a raw pointer wrapped in
//  a safe newtype.  In a real Scrappy OS kernel you'd use a
//  spinlock cell or message-passing to access context.

use core::cell::UnsafeCell;
use alloc::sync::Arc;

struct SharedCtx(UnsafeCell<WasiCtx>);
// Safety: Scrappy OS is cooperative/single-threaded per cell.
unsafe impl Send for SharedCtx {}
unsafe impl Sync for SharedCtx {}

pub type WasiFuncs = Vec<(&'static str, &'static str, Func)>;

pub fn build_wasi_imports(ctx: WasiCtx) -> WasiFuncs {
    let ctx = Arc::new(SharedCtx(UnsafeCell::new(ctx)));

    macro_rules! wasi_fn {
        ($params:expr, $results:expr, $body:expr) => {{
            let c = Arc::clone(&ctx);
            Func {
                ty: crate::types::FuncType {
                    params:  $params,
                    results: $results,
                },
                kind: FuncKind::Host {
                    func: Box::new(move |args: &[Value], mem: &mut Memory| {
                        // SAFETY: Scrappy OS cells run single-threaded.
                        let ctx: &mut WasiCtx = unsafe { &mut *c.0.get() };
                        $body(ctx, args, mem)
                    }),
                },
            }
        }};
    }

    use ValType::*;

    vec![
        // ── fd_write(fd, iov, iov_cnt, nwritten_ptr) → errno ──
        ("wasi_snapshot_preview1", "fd_write", wasi_fn!(
            vec![I32, I32, I32, I32],
            vec![I32],
            |ctx: &mut WasiCtx, args: &[Value], mem: &mut Memory| {
                let fd       = args[0].as_i32().unwrap_or(1);
                let iov_ptr  = args[1].as_i32().unwrap_or(0) as u32;
                let iov_cnt  = args[2].as_i32().unwrap_or(0) as u32;
                let nw_ptr   = args[3].as_i32().unwrap_or(0) as u32;

                let data = read_iovs(mem, iov_ptr, iov_cnt)?;
                let n    = data.len() as u32;

                let msg = EmailMessage {
                    from:    ctx.cell,
                    to:      fd_to_cell(fd),
                    subject: String::from("wasi::fd_write"),
                    body:    data,
                    reply:   None,
                };
                ctx.mailbox.send_recv(msg); // fire-and-forget for writes

                write_u32_le(mem, nw_ptr, n)?;
                Ok(vec![Value::I32(WasiErrno::Success as i32)])
            }
        )),

        // ── fd_read(fd, iov, iov_cnt, nread_ptr) → errno ──────
        ("wasi_snapshot_preview1", "fd_read", wasi_fn!(
            vec![I32, I32, I32, I32],
            vec![I32],
            |ctx: &mut WasiCtx, args: &[Value], mem: &mut Memory| {
                let fd      = args[0].as_i32().unwrap_or(0);
                let iov_ptr = args[1].as_i32().unwrap_or(0) as u32;
                let iov_cnt = args[2].as_i32().unwrap_or(0) as u32;
                let nr_ptr  = args[3].as_i32().unwrap_or(0) as u32;

                // Compute total capacity requested
                let mut cap = 0u32;
                for i in 0..iov_cnt {
                    let base = iov_ptr + i * 8;
                    cap += mem.load_u32(base + 4).unwrap_or(0);
                }

                let msg = EmailMessage {
                    from:    ctx.cell,
                    to:      fd_to_cell(fd),
                    subject: String::from("wasi::fd_read"),
                    body:    (cap as usize).to_le_bytes().to_vec(),
                    reply:   None,
                };
                let data = ctx.mailbox.send_recv(msg);

                // Scatter-write back into iovs
                let mut src = 0usize;
                for i in 0..iov_cnt {
                    let base = iov_ptr + i * 8;
                    let ptr  = mem.load_u32(base)?;
                    let len  = mem.load_u32(base + 4)? as usize;
                    let end  = core::cmp::min(src + len, data.len());
                    for (j, &b) in data[src..end].iter().enumerate() {
                        mem.store_u8(ptr + j as u32, b)?;
                    }
                    src = end;
                    if src >= data.len() { break; }
                }

                write_u32_le(mem, nr_ptr, data.len() as u32)?;
                Ok(vec![Value::I32(WasiErrno::Success as i32)])
            }
        )),

        // ── fd_close(fd) → errno ─────────────────────────────
        ("wasi_snapshot_preview1", "fd_close", wasi_fn!(
            vec![I32], vec![I32],
            |ctx: &mut WasiCtx, args: &[Value], _mem: &mut Memory| {
                let fd = args[0].as_i32().unwrap_or(-1);
                let msg = EmailMessage {
                    from:    ctx.cell,
                    to:      fd_to_cell(fd),
                    subject: String::from("wasi::fd_close"),
                    body:    (fd as u32).to_le_bytes().to_vec(),
                    reply:   None,
                };
                ctx.mailbox.send_recv(msg);
                Ok(vec![Value::I32(WasiErrno::Success as i32)])
            }
        )),

        // ── path_open(…) → (errno, fd) ───────────────────────
        // Simplified: passes path string to FS cell, receives back a new fd.
        ("wasi_snapshot_preview1", "path_open", wasi_fn!(
            vec![I32, I32, I32, I32, I32, I64, I64, I32, I32],
            vec![I32],
            |ctx: &mut WasiCtx, args: &[Value], mem: &mut Memory| {
                let path_ptr = args[2].as_i32().unwrap_or(0) as u32;
                let path_len = args[3].as_i32().unwrap_or(0) as u32;
                let fd_ptr   = args[8].as_i32().unwrap_or(0) as u32;

                let mut path_bytes = Vec::with_capacity(path_len as usize);
                for i in 0..path_len {
                    path_bytes.push(mem.load_u8(path_ptr + i)?);
                }

                let msg = EmailMessage {
                    from:    ctx.cell,
                    to:      CELL_FS,
                    subject: String::from("wasi::path_open"),
                    body:    path_bytes,
                    reply:   None,
                };
                let reply = ctx.mailbox.send_recv(msg);

                // Reply format: [errno u32 LE, new_fd u32 LE]
                let errno  = if reply.len() >= 4 { u32::from_le_bytes([reply[0],reply[1],reply[2],reply[3]]) } else { WasiErrno::Io as u32 };
                let new_fd = if reply.len() >= 8 { u32::from_le_bytes([reply[4],reply[5],reply[6],reply[7]]) } else { 0 };

                write_u32_le(mem, fd_ptr, new_fd)?;
                Ok(vec![Value::I32(errno as i32)])
            }
        )),

        // ── proc_exit(code) → ! ──────────────────────────────
        ("wasi_snapshot_preview1", "proc_exit", wasi_fn!(
            vec![I32], vec![],
            |ctx: &mut WasiCtx, args: &[Value], _mem: &mut Memory| {
                let code = args[0].as_i32().unwrap_or(0);
                let msg = EmailMessage {
                    from:    ctx.cell,
                    to:      CELL_SCHEDULER,
                    subject: String::from("wasi::proc_exit"),
                    body:    (code as u32).to_le_bytes().to_vec(),
                    reply:   None,
                };
                ctx.mailbox.send_recv(msg);
                // In a real kernel this would not return.
                Err(WasmError::WasiError(WasiErrno::Success))
            }
        )),

        // ── args_sizes_get(argc_ptr, argv_buf_size_ptr) → errno
        ("wasi_snapshot_preview1", "args_sizes_get", wasi_fn!(
            vec![I32, I32], vec![I32],
            |ctx: &mut WasiCtx, args: &[Value], mem: &mut Memory| {
                let argc_ptr    = args[0].as_i32().unwrap_or(0) as u32;
                let argbuf_ptr  = args[1].as_i32().unwrap_or(0) as u32;
                let argc        = ctx.args.len() as u32;
                let buf_sz: u32 = ctx.args.iter().map(|a| a.len() as u32).sum();
                write_u32_le(mem, argc_ptr, argc)?;
                write_u32_le(mem, argbuf_ptr, buf_sz)?;
                Ok(vec![Value::I32(WasiErrno::Success as i32)])
            }
        )),

        // ── args_get(argv_ptr, argv_buf_ptr) → errno ─────────
        ("wasi_snapshot_preview1", "args_get", wasi_fn!(
            vec![I32, I32], vec![I32],
            |ctx: &mut WasiCtx, args: &[Value], mem: &mut Memory| {
                let argv_ptr    = args[0].as_i32().unwrap_or(0) as u32;
                let argbuf_ptr  = args[1].as_i32().unwrap_or(0) as u32;

                let mut buf_off = argbuf_ptr;
                for (i, arg) in ctx.args.iter().enumerate() {
                    // Write pointer into argv array
                    write_u32_le(mem, argv_ptr + (i as u32) * 4, buf_off)?;
                    // Write the string bytes
                    for (j, &b) in arg.iter().enumerate() {
                        mem.store_u8(buf_off + j as u32, b)?;
                    }
                    buf_off += arg.len() as u32;
                }
                Ok(vec![Value::I32(WasiErrno::Success as i32)])
            }
        )),

        // ── environ_sizes_get(envc_ptr, env_buf_size_ptr) → errno
        ("wasi_snapshot_preview1", "environ_sizes_get", wasi_fn!(
            vec![I32, I32], vec![I32],
            |ctx: &mut WasiCtx, args: &[Value], mem: &mut Memory| {
                let envc_ptr   = args[0].as_i32().unwrap_or(0) as u32;
                let envbuf_ptr = args[1].as_i32().unwrap_or(0) as u32;
                let envc       = ctx.env.len() as u32;
                let buf_sz: u32= ctx.env.iter().map(|e| e.len() as u32).sum();
                write_u32_le(mem, envc_ptr, envc)?;
                write_u32_le(mem, envbuf_ptr, buf_sz)?;
                Ok(vec![Value::I32(WasiErrno::Success as i32)])
            }
        )),

        // ── environ_get(env_ptr, env_buf_ptr) → errno ────────
        ("wasi_snapshot_preview1", "environ_get", wasi_fn!(
            vec![I32, I32], vec![I32],
            |ctx: &mut WasiCtx, args: &[Value], mem: &mut Memory| {
                let env_ptr    = args[0].as_i32().unwrap_or(0) as u32;
                let envbuf_ptr = args[1].as_i32().unwrap_or(0) as u32;
                let mut buf_off = envbuf_ptr;
                for (i, kv) in ctx.env.iter().enumerate() {
                    write_u32_le(mem, env_ptr + (i as u32) * 4, buf_off)?;
                    for (j, &b) in kv.iter().enumerate() {
                        mem.store_u8(buf_off + j as u32, b)?;
                    }
                    buf_off += kv.len() as u32;
                }
                Ok(vec![Value::I32(WasiErrno::Success as i32)])
            }
        )),

        // ── clock_time_get(clock_id, precision, time_ptr) → errno
        ("wasi_snapshot_preview1", "clock_time_get", wasi_fn!(
            vec![I32, I64, I32], vec![I32],
            |ctx: &mut WasiCtx, args: &[Value], mem: &mut Memory| {
                let time_ptr = args[2].as_i32().unwrap_or(0) as u32;
                // Ask timer cell via Email
                let msg = EmailMessage {
                    from:    ctx.cell,
                    to:      CELL_TIMER,
                    subject: String::from("wasi::clock_time_get"),
                    body:    vec![],
                    reply:   None,
                };
                let reply = ctx.mailbox.send_recv(msg);
                let ns = if reply.len() >= 8 {
                    u64::from_le_bytes(reply[..8].try_into().unwrap())
                } else {
                    ctx.mailbox.now_ns()
                };
                // WASI returns u64 nanoseconds at time_ptr
                mem.store_u64(time_ptr, ns)?;
                Ok(vec![Value::I32(WasiErrno::Success as i32)])
            }
        )),

        // ── random_get(buf_ptr, buf_len) → errno ─────────────
        ("wasi_snapshot_preview1", "random_get", wasi_fn!(
            vec![I32, I32], vec![I32],
            |ctx: &mut WasiCtx, args: &[Value], mem: &mut Memory| {
                let buf_ptr = args[0].as_i32().unwrap_or(0) as u32;
                let buf_len = args[1].as_i32().unwrap_or(0) as u32;

                let msg = EmailMessage {
                    from:    ctx.cell,
                    to:      CELL_ENTROPY,
                    subject: String::from("wasi::random_get"),
                    body:    buf_len.to_le_bytes().to_vec(),
                    reply:   None,
                };
                let rand_bytes = ctx.mailbox.send_recv(msg);

                let n = core::cmp::min(rand_bytes.len(), buf_len as usize);
                for i in 0..n {
                    mem.store_u8(buf_ptr + i as u32, rand_bytes[i])?;
                }
                Ok(vec![Value::I32(WasiErrno::Success as i32)])
            }
        )),

        // ── poll_oneoff(in, out, nsubscriptions, nevents_ptr) → errno
        // Minimal: immediately marks all events as timed-out.
        ("wasi_snapshot_preview1", "poll_oneoff", wasi_fn!(
            vec![I32, I32, I32, I32], vec![I32],
            |_ctx: &mut WasiCtx, args: &[Value], mem: &mut Memory| {
                let nevents_ptr = args[3].as_i32().unwrap_or(0) as u32;
                write_u32_le(mem, nevents_ptr, 0)?; // 0 events fired
                Ok(vec![Value::I32(WasiErrno::Success as i32)])
            }
        )),

        // ── fd_seek(fd, offset, whence, newoffset_ptr) → errno
        ("wasi_snapshot_preview1", "fd_seek", wasi_fn!(
            vec![I32, I64, I32, I32], vec![I32],
            |ctx: &mut WasiCtx, args: &[Value], mem: &mut Memory| {
                let fd          = args[0].as_i32().unwrap_or(0);
                let offset      = args[1].as_i64().unwrap_or(0);
                let whence      = args[2].as_i32().unwrap_or(0);
                let newoff_ptr  = args[3].as_i32().unwrap_or(0) as u32;

                let mut body = Vec::with_capacity(13);
                body.extend_from_slice(&(fd as u32).to_le_bytes());
                body.extend_from_slice(&offset.to_le_bytes());
                body.push(whence as u8);

                let msg = EmailMessage {
                    from:    ctx.cell,
                    to:      CELL_FS,
                    subject: String::from("wasi::fd_seek"),
                    body,
                    reply:   None,
                };
                let reply = ctx.mailbox.send_recv(msg);

                let new_off = if reply.len() >= 8 {
                    u64::from_le_bytes(reply[..8].try_into().unwrap())
                } else { 0 };
                mem.store_u64(newoff_ptr, new_off)?;
                Ok(vec![Value::I32(WasiErrno::Success as i32)])
            }
        )),
    ]
}
