//! Generic C-ABI foreign-function interface exposed to Rhai.
//!
//! Unlike a bespoke plugin protocol, this puts the binding in the *script*: a
//! plugin is just a vanilla shared library with whatever idiomatic C API it
//! likes (in principle anything — `libinference.so`, `libGL.so`, …), and the
//! script declares how to call into it. Think LWJGL's `org.lwjgl.system`.
//!
//! ```rhai
//! let lib = ffi::open("../target/release/libinference.so");
//! // bind a symbol: ffi::open → lib.func(name, ret_type, [arg_types])
//! let detect = lib.func("inference_detect_language", "ptr", ["ptr"]);
//! let in_ptr = ffi::cstr("にちか");           // malloc'd NUL-terminated copy
//! let out    = detect.invoke([in_ptr]);        // returns a `ptr`
//! let lang   = ffi::read_cstr(out);
//! ffi::free(in_ptr);
//! ffi::free(out);                              // if the C API hands back owned memory
//! ```
//!
//! ## Types
//!
//! A type descriptor is either a scalar tag (string, case-insensitive) or a
//! struct handle from [`ffi::struct_type`]. Scalar tags:
//! `void`, `i8`/`u8`, `i16`/`u16`, `i32`/`u32`, `i64`/`u64`, `f32`, `f64`,
//! `ptr`. Integers cross as Rhai `INT` (i64), floats as `FLOAT` (f64), `ptr`
//! as an opaque `Ptr`, `void` as `()`. A by-value struct crosses as a Rhai
//! array of its field values (in declaration order), e.g.
//!
//! ```rhai
//! let div_t = ffi::struct_type(["i32", "i32"]);     // struct { int; int; }
//! let div   = libc.func("div", div_t, ["i32", "i32"]);
//! let r     = div.invoke([17, 5]);                  // [3, 2]  (quot, rem)
//! ```
//!
//! ## Callbacks
//!
//! [`ffi::callback`] turns a Rhai function pointer into a C-callable function
//! pointer (a libffi closure), e.g. a `qsort` comparator. **It only works for
//! callbacks invoked synchronously, on the calling thread, during an
//! `invoke(...)`** — the Rhai engine is single-threaded, so a callback that C
//! stores and fires later (or from another thread) finds no live call context
//! and returns zero with a warning.
//!
//! ## Not supported
//!
//! - **Variadic functions** (`printf`-style). The CIF is fixed-arity.
//! - **Asynchronous / cross-thread callbacks** — see the Callbacks note above;
//!   only callbacks fired synchronously on the calling thread work.
//! - **`f32`/`f64`/`long double` callback *arguments*** beyond what the natural
//!   in-memory read covers, and **big-endian targets** — scalar marshalling
//!   reads/writes native-endian bytes and assumes little-endian register/return
//!   widening (fine on x86-64 / aarch64-LE).
//! - **Packed or non-default-ABI structs**, bitfields, unions, and `long
//!   double` / SIMD-vector struct fields — `struct_type` only models naturally
//!   aligned scalar/pointer/nested-struct fields.
//! - **Passing a struct by *pointer* is fine** (use `ptr` + the memory
//!   helpers); only by-*value* structs go through `struct_type`, and their
//!   return path has only been exercised for small two-word structs.
//! - **Non-default calling conventions** (e.g. Windows `stdcall`) — always the
//!   platform default ABI.
//! - **Automatic memory management** — every `malloc`/`cstr` and every buffer a
//!   C API hands back is the script's to `free`.
//!
//! # Safety
//!
//! This is an unrestricted FFI escape hatch. A wrong signature, a dangling
//! pointer, or a bad length is undefined behaviour and will likely crash the
//! whole importer — exactly as it would in C. The host cannot validate any of
//! it. Scripts that use `ffi::` are trusted, same as native plugins.

use std::cell::Cell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::path::PathBuf;
use std::sync::Arc;

use libffi::middle::{Arg, Cif, Closure, CodePtr, Type};
use libffi::{low, raw};
use rhai::{
    Dynamic, EvalAltResult, FLOAT, FnNamespace, FnPtr, FuncRegistration, INT, Module,
    NativeCallContext,
};
use tracing::warn;

type RhaiResult = Result<Dynamic, Box<EvalAltResult>>;

fn err(msg: impl Into<String>) -> Box<EvalAltResult> {
    Box::new(EvalAltResult::ErrorRuntime(
        Dynamic::from(msg.into()),
        rhai::Position::NONE,
    ))
}

// ── Scalar type vocabulary ──────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tag {
    Void,
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
    Ptr,
}

impl Tag {
    fn parse(s: &str) -> Result<Tag, Box<EvalAltResult>> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "void" | "()" => Tag::Void,
            "i8" | "char" | "schar" => Tag::I8,
            "u8" | "uchar" | "byte" => Tag::U8,
            "i16" | "short" => Tag::I16,
            "u16" | "ushort" => Tag::U16,
            "i32" | "int" => Tag::I32,
            "u32" | "uint" => Tag::U32,
            "i64" | "long" | "isize" => Tag::I64,
            "u64" | "ulong" | "usize" | "size_t" => Tag::U64,
            "f32" | "float" => Tag::F32,
            "f64" | "double" => Tag::F64,
            "ptr" | "pointer" | "void*" | "char*" => Tag::Ptr,
            other => return Err(err(format!("ffi: unknown type tag {other:?}"))),
        })
    }

    fn ffi_type(self) -> Type {
        match self {
            Tag::Void => Type::void(),
            Tag::I8 => Type::i8(),
            Tag::U8 => Type::u8(),
            Tag::I16 => Type::i16(),
            Tag::U16 => Type::u16(),
            Tag::I32 => Type::i32(),
            Tag::U32 => Type::u32(),
            Tag::I64 => Type::i64(),
            Tag::U64 => Type::u64(),
            Tag::F32 => Type::f32(),
            Tag::F64 => Type::f64(),
            Tag::Ptr => Type::pointer(),
        }
    }

    /// Size and alignment in bytes (natural alignment == size for these).
    fn size(self) -> usize {
        match self {
            Tag::Void => 0,
            Tag::I8 | Tag::U8 => 1,
            Tag::I16 | Tag::U16 => 2,
            Tag::I32 | Tag::U32 | Tag::F32 => 4,
            Tag::I64 | Tag::U64 | Tag::F64 => 8,
            Tag::Ptr => std::mem::size_of::<usize>(),
        }
    }

    /// Read a scalar of this tag from the start of `buf` into a `Dynamic`.
    fn read(self, buf: &[u8]) -> Dynamic {
        macro_rules! n {
            ($t:ty) => {{
                let mut b = [0u8; std::mem::size_of::<$t>()];
                b.copy_from_slice(&buf[..std::mem::size_of::<$t>()]);
                <$t>::from_ne_bytes(b)
            }};
        }
        match self {
            Tag::Void => Dynamic::UNIT,
            Tag::I8 => Dynamic::from(buf[0] as i8 as INT),
            Tag::U8 => Dynamic::from(buf[0] as INT),
            Tag::I16 => Dynamic::from(n!(i16) as INT),
            Tag::U16 => Dynamic::from(n!(u16) as INT),
            Tag::I32 => Dynamic::from(n!(i32) as INT),
            Tag::U32 => Dynamic::from(n!(u32) as INT),
            Tag::I64 => Dynamic::from(n!(i64)),
            Tag::U64 => Dynamic::from(n!(u64) as INT),
            Tag::F32 => Dynamic::from(n!(f32) as FLOAT),
            Tag::F64 => Dynamic::from(n!(f64)),
            Tag::Ptr => Dynamic::from(Ptr(n!(usize) as *mut c_void)),
        }
    }

    /// Write the scalar value of `d` (per this tag) into the start of `buf`.
    fn write(self, d: &Dynamic, buf: &mut [u8]) -> Result<(), Box<EvalAltResult>> {
        let i = || -> Result<i64, Box<EvalAltResult>> {
            d.as_int()
                .map_err(|_| err(format!("ffi: expected integer, got {}", d.type_name())))
        };
        let f = || -> Result<f64, Box<EvalAltResult>> {
            d.as_float().map_err(|_| err("ffi: expected float"))
        };
        macro_rules! put {
            ($v:expr) => {{
                let b = $v.to_ne_bytes();
                buf[..b.len()].copy_from_slice(&b);
            }};
        }
        match self {
            Tag::Void => {}
            Tag::I8 => put!((i()? as i8)),
            Tag::U8 => put!((i()? as u8)),
            Tag::I16 => put!((i()? as i16)),
            Tag::U16 => put!((i()? as u16)),
            Tag::I32 => put!((i()? as i32)),
            Tag::U32 => put!((i()? as u32)),
            Tag::I64 => put!((i()?)),
            Tag::U64 => put!((i()? as u64)),
            Tag::F32 => put!((f()? as f32)),
            Tag::F64 => put!((f()?)),
            Tag::Ptr => {
                let p = d
                    .clone()
                    .try_cast::<Ptr>()
                    .ok_or_else(|| err("ffi: expected a `ptr`"))?;
                put!((p.0 as usize));
            }
        }
        Ok(())
    }
}

// ── Composite type system (scalars + by-value structs) ──────────────────────

/// A C type: a scalar, or a struct described by its fields. Recursive so
/// structs can nest.
#[derive(Clone)]
enum CType {
    Scalar(Tag),
    Struct(Arc<StructDef>),
}

/// A by-value struct's field types plus the standard-ABI layout we compute for
/// it (field byte offsets, total size, alignment). Non-packed, natural
/// alignment — matches libffi's default struct layout on the platforms we target.
struct StructDef {
    fields: Vec<CType>,
    offsets: Vec<usize>,
    size: usize,
}

impl StructDef {
    fn new(fields: Vec<CType>) -> StructDef {
        let mut offsets = Vec::with_capacity(fields.len());
        let mut off = 0usize;
        let mut max_align = 1usize;
        for f in &fields {
            let (sz, al) = (f.size(), f.align());
            off = off.div_ceil(al) * al;
            offsets.push(off);
            off += sz;
            max_align = max_align.max(al);
        }
        let size = off.div_ceil(max_align) * max_align;
        StructDef {
            fields,
            offsets,
            size,
        }
    }
}

impl CType {
    fn size(&self) -> usize {
        match self {
            CType::Scalar(t) => t.size(),
            CType::Struct(s) => s.size,
        }
    }

    fn align(&self) -> usize {
        match self {
            CType::Scalar(t) => t.size().max(1),
            // A struct's alignment is its widest field's alignment.
            CType::Struct(s) => s.fields.iter().map(CType::align).max().unwrap_or(1),
        }
    }

    fn ffi_type(&self) -> Type {
        match self {
            CType::Scalar(t) => t.ffi_type(),
            CType::Struct(s) => {
                let fields: Vec<Type> = s.fields.iter().map(CType::ffi_type).collect();
                Type::structure(fields)
            }
        }
    }

    /// Decode a value of this type from `buf` (which must be at least
    /// `self.size()` bytes) into a `Dynamic`. Structs become arrays.
    fn read(&self, buf: &[u8]) -> Dynamic {
        match self {
            CType::Scalar(t) => t.read(buf),
            CType::Struct(s) => {
                let mut arr = rhai::Array::with_capacity(s.fields.len());
                for (f, &off) in s.fields.iter().zip(&s.offsets) {
                    arr.push(f.read(&buf[off..]));
                }
                Dynamic::from(arr)
            }
        }
    }

    /// Encode `d` into `buf` per this type. Structs expect a Rhai array.
    fn write(&self, d: &Dynamic, buf: &mut [u8]) -> Result<(), Box<EvalAltResult>> {
        match self {
            CType::Scalar(t) => t.write(d, buf),
            CType::Struct(s) => {
                let arr = d
                    .read_lock::<rhai::Array>()
                    .ok_or_else(|| err("ffi: expected an array for a struct value"))?;
                if arr.len() != s.fields.len() {
                    return Err(err(format!(
                        "ffi: struct expects {} fields, got {}",
                        s.fields.len(),
                        arr.len()
                    )));
                }
                for ((f, &off), v) in s.fields.iter().zip(&s.offsets).zip(arr.iter()) {
                    f.write(v, &mut buf[off..])?;
                }
                Ok(())
            }
        }
    }
}

/// Parse a type descriptor `Dynamic`: a string scalar tag, or a `StructType`
/// handle from `ffi::struct_type`.
fn parse_ctype(d: &Dynamic) -> Result<CType, Box<EvalAltResult>> {
    if let Some(s) = d.read_lock::<rhai::ImmutableString>() {
        return Ok(CType::Scalar(Tag::parse(&s)?));
    }
    if let Some(st) = d.read_lock::<StructType>() {
        return Ok(CType::Struct(st.0.clone()));
    }
    Err(err(format!(
        "ffi: type descriptor must be a tag string or a struct type, got {}",
        d.type_name()
    )))
}

fn parse_ctypes(arr: &rhai::Array) -> Result<Vec<CType>, Box<EvalAltResult>> {
    arr.iter().map(parse_ctype).collect()
}

// ── Rhai-visible handles ────────────────────────────────────────────────────

/// A loaded shared library. Cloneable; keeps the library mapped while any
/// clone (or any `Func` bound from it) is alive.
#[derive(Clone)]
struct Lib(Arc<libloading::Library>);

/// An opaque C pointer. Stored as a machine word; printed as hex.
#[derive(Clone, Copy)]
struct Ptr(*mut c_void);

// SAFETY: `Ptr` is an inert address the script juggles explicitly; any actual
// dereference goes through the unsafe read/write helpers. Marking it Send+Sync
// lets it live in a `Dynamic` regardless of Rhai's `sync` feature.
unsafe impl Send for Ptr {}
unsafe impl Sync for Ptr {}

/// A reusable by-value struct type descriptor (from `ffi::struct_type`).
#[derive(Clone)]
struct StructType(Arc<StructDef>);

/// A symbol bound to a concrete signature, ready to call.
#[derive(Clone)]
struct Func {
    _lib: Lib, // keep the library mapped
    code: CodePtr,
    cif: Cif,
    args: Vec<CType>,
    ret: CType,
}

// SAFETY: a `Func` is a code address plus an immutable CIF describing its
// signature; both are stable for the life of the mapped library.
unsafe impl Send for Func {}
unsafe impl Sync for Func {}

// ── Argument materialisation ────────────────────────────────────────────────

/// Owned storage for one outgoing argument. libffi's `Arg` borrows a pointer to
/// the value, so the value must outlive the call — we keep these in a Vec.
enum Slot {
    I8(i8),
    U8(u8),
    I16(i16),
    U16(u16),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    F32(f32),
    F64(f64),
    Ptr(*mut c_void),
    /// A by-value struct (or any composite), pre-laid-out in a byte buffer.
    Bytes(Vec<u8>),
}

impl Slot {
    fn as_arg(&self) -> Arg {
        match self {
            Slot::I8(v) => Arg::new(v),
            Slot::U8(v) => Arg::new(v),
            Slot::I16(v) => Arg::new(v),
            Slot::U16(v) => Arg::new(v),
            Slot::I32(v) => Arg::new(v),
            Slot::U32(v) => Arg::new(v),
            Slot::I64(v) => Arg::new(v),
            Slot::U64(v) => Arg::new(v),
            Slot::F32(v) => Arg::new(v),
            Slot::F64(v) => Arg::new(v),
            Slot::Ptr(v) => Arg::new(v),
            // Point libffi at the struct's bytes (the Arg stores the address).
            Slot::Bytes(v) => Arg::new(unsafe { &*v.as_ptr() }),
        }
    }
}

/// Materialise one outgoing argument from a `Dynamic` per its `CType`.
fn slot_for(ty: &CType, d: &Dynamic) -> Result<Slot, Box<EvalAltResult>> {
    let tag = match ty {
        CType::Scalar(t) => *t,
        CType::Struct(s) => {
            let mut buf = vec![0u8; s.size];
            ty.write(d, &mut buf)?;
            return Ok(Slot::Bytes(buf));
        }
    };
    let i = || -> Result<i64, Box<EvalAltResult>> {
        d.as_int().map_err(|_| {
            err(format!(
                "ffi: expected integer argument, got {}",
                d.type_name()
            ))
        })
    };
    Ok(match tag {
        Tag::Void => return Err(err("ffi: `void` is not a valid argument type")),
        Tag::I8 => Slot::I8(i()? as i8),
        Tag::U8 => Slot::U8(i()? as u8),
        Tag::I16 => Slot::I16(i()? as i16),
        Tag::U16 => Slot::U16(i()? as u16),
        Tag::I32 => Slot::I32(i()? as i32),
        Tag::U32 => Slot::U32(i()? as u32),
        Tag::I64 => Slot::I64(i()?),
        Tag::U64 => Slot::U64(i()? as u64),
        Tag::F32 => Slot::F32(
            d.as_float()
                .map_err(|_| err("ffi: expected float argument"))? as f32,
        ),
        Tag::F64 => Slot::F64(
            d.as_float()
                .map_err(|_| err("ffi: expected float argument"))?,
        ),
        Tag::Ptr => Slot::Ptr(
            d.clone()
                .try_cast::<Ptr>()
                .ok_or_else(|| err("ffi: expected a `ptr` argument"))?
                .0,
        ),
    })
}

// ── Function call ───────────────────────────────────────────────────────────

fn do_call(func: &Func, raw_args: rhai::Array) -> RhaiResult {
    if raw_args.len() != func.args.len() {
        return Err(err(format!(
            "ffi: call expected {} args, got {}",
            func.args.len(),
            raw_args.len()
        )));
    }

    let slots: Vec<Slot> = func
        .args
        .iter()
        .zip(&raw_args)
        .map(|(ty, d)| slot_for(ty, d))
        .collect::<Result<_, _>>()?;
    let args: Vec<Arg> = slots.iter().map(Slot::as_arg).collect();

    // One uniform call path for scalars and structs: libffi writes the return
    // value into a buffer we own. Integer returns are widened to `ffi_arg`, so
    // the buffer is at least that wide; on little-endian targets the low bytes
    // hold the value, which is what `CType::read` decodes.
    let ret_size = func.ret.size().max(std::mem::size_of::<raw::ffi_arg>());
    let mut rbuf = vec![0u8; ret_size];

    // SAFETY: the script asserts that `func`'s declared signature matches the
    // real symbol. If it doesn't, this is UB — documented at the module level.
    unsafe {
        raw::ffi_call(
            func.cif.as_raw_ptr(),
            Some(std::mem::transmute::<*mut c_void, unsafe extern "C" fn()>(
                func.code.0,
            )),
            rbuf.as_mut_ptr() as *mut c_void,
            args.as_ptr() as *mut *mut c_void,
        );
    }
    Ok(func.ret.read(&rbuf))
}

// ── Callbacks (Rhai fn → C function pointer) ────────────────────────────────

// Raw pointer to the live `NativeCallContext` of the innermost `invoke(...)`
// on this thread, so a synchronously-fired C callback can re-enter Rhai.
// `null` outside any `invoke` (and on threads other than the caller's).
thread_local! {
    static CTX: Cell<*const ()> = const { Cell::new(std::ptr::null()) };
}

/// Userdata for a callback closure: the Rhai function to invoke and the
/// signature describing how to translate C arguments and the return value.
struct CallbackState {
    func: FnPtr,
    args: Vec<CType>,
    ret: CType,
}

/// libffi closure entry point. Decodes C args into `Dynamic`s, calls the Rhai
/// function via the thread-local call context, and writes back the return value.
unsafe extern "C" fn trampoline(
    _cif: &low::ffi_cif,
    result: &mut c_void,
    args: *const *const c_void,
    ud: &CallbackState,
) {
    unsafe {
        let out_len = ud.ret.size().max(std::mem::size_of::<raw::ffi_arg>());
        let out = std::slice::from_raw_parts_mut(result as *mut c_void as *mut u8, out_len);
        out.fill(0);

        let ctx_ptr = CTX.with(Cell::get);
        if ctx_ptr.is_null() {
            warn!(
                "ffi: callback fired with no active call context (stored/cross-thread?) — returning 0"
            );
            return;
        }
        let ctx = &*(ctx_ptr as *const NativeCallContext);

        let argv: Vec<Dynamic> = ud
            .args
            .iter()
            .enumerate()
            .map(|(i, ty)| {
                let p = *args.add(i) as *const u8;
                ty.read(std::slice::from_raw_parts(p, ty.size().max(1)))
            })
            .collect();

        match ud.func.call_raw(ctx, None, argv) {
            Ok(d) => {
                if let Err(e) = ud.ret.write(&d, out) {
                    warn!("ffi: callback return conversion failed: {e}");
                }
            }
            Err(e) => warn!("ffi: callback raised an error: {e}"),
        }
    }
}

/// A live callback: keeps the libffi closure (and its userdata) alive and
/// exposes the C-callable code pointer. Dropping it invalidates the pointer.
struct Callback {
    // Drop order matters: `_closure` references `_state`, so it must drop first
    // (struct fields drop in declaration order).
    _closure: Closure<'static>,
    _state: Box<CallbackState>,
    code: Ptr,
}

// SAFETY: `Callback` owns its closure + userdata; the code pointer is stable
// for its lifetime. Raw pointers inside force the manual marker.
unsafe impl Send for Callback {}
unsafe impl Sync for Callback {}

/// Rhai-visible, cloneable handle to a [`Callback`]. Clones share the same
/// closure (refcounted); the trampoline stays valid until the last clone drops.
#[derive(Clone)]
struct CallbackHandle(Arc<Callback>);

fn make_callback(func: FnPtr, ret: CType, args: Vec<CType>) -> Callback {
    let cif = Cif::new(
        args.iter().map(CType::ffi_type).collect::<Vec<_>>(),
        ret.ffi_type(),
    );
    let state = Box::new(CallbackState { func, args, ret });
    // SAFETY: `state` lives in this struct alongside the closure and outlives
    // it (closure drops first), so this 'static borrow never dangles.
    let state_ref: &'static CallbackState = unsafe { &*(state.as_ref() as *const CallbackState) };
    let closure = Closure::new(
        cif,
        trampoline as low::Callback<CallbackState, c_void>,
        state_ref,
    );
    let code = Ptr(*closure.code_ptr() as *mut c_void);
    Callback {
        _closure: closure,
        _state: state,
        code,
    }
}

// ── Module construction ─────────────────────────────────────────────────────

/// Build the `ffi` module. Registered as `ffi`, the script gets the full
/// surface: `ffi::open`, `Lib::func`, `Func::invoke`, `ffi::struct_type`,
/// `ffi::callback`, and the memory helpers.
/// Build the `ffi` static module. `base_dir` is the directory of the script
/// being run; relative paths passed to `ffi::open` are resolved against it so
/// the library location is independent of the process CWD (`dlopen` would
/// otherwise resolve them against the CWD). Absolute paths and bare library
/// names — for the platform loader's search path — are passed through unchanged.
pub fn module(base_dir: PathBuf) -> Module {
    let mut m = Module::new();

    // ffi::open(path) -> Lib
    m.set_native_fn("open", move |path: &str| -> RhaiResult {
        // Resolve a relative path against the script dir; leave absolute paths
        // and bare names (no separator → loader search path) untouched.
        let p = std::path::Path::new(path);
        let resolved = if p.is_relative() && path.contains(std::path::MAIN_SEPARATOR) {
            base_dir.join(p)
        } else {
            p.to_owned()
        };
        // SAFETY: dlopen of a user-named library; inherently trusted input.
        let lib = unsafe { libloading::Library::new(&resolved) }
            .map_err(|e| err(format!("ffi: could not open {resolved:?}: {e}")))?;
        Ok(Dynamic::from(Lib(Arc::new(lib))))
    });

    // ffi::struct_type([field_types]) -> StructType
    m.set_native_fn("struct_type", |fields: rhai::Array| -> RhaiResult {
        let fields = parse_ctypes(&fields)?;
        Ok(Dynamic::from(StructType(Arc::new(StructDef::new(fields)))))
    });

    // lib.func(name, ret_type, [arg_types]) -> Func
    // Method-style call, so it lives in the global namespace rather than `ffi::`.
    FuncRegistration::new("func")
        .with_namespace(FnNamespace::Global)
        .set_into_module(
            &mut m,
            |lib: &mut Lib, name: &str, ret: Dynamic, arg_types: rhai::Array| -> RhaiResult {
                let ret = parse_ctype(&ret)?;
                let args = parse_ctypes(&arg_types)?;

                // SAFETY: resolve the symbol; null/missing → error rather than UB.
                let sym: libloading::Symbol<*mut c_void> = unsafe {
                    lib.0
                        .get(name.as_bytes())
                        .map_err(|e| err(format!("ffi: no symbol {name:?}: {e}")))?
                };
                let code = CodePtr(*sym);
                let cif = Cif::new(
                    args.iter().map(CType::ffi_type).collect::<Vec<_>>(),
                    ret.ffi_type(),
                );
                Ok(Dynamic::from(Func {
                    _lib: lib.clone(),
                    code,
                    cif,
                    args,
                    ret,
                }))
            },
        );

    // f.invoke([args]) -> result   (not `call`: that name is reserved by Rhai
    // for `FnPtr` invocation and would shadow this method). Takes the call
    // context so synchronously-fired callbacks can re-enter the engine.
    FuncRegistration::new("invoke")
        .with_namespace(FnNamespace::Global)
        .set_into_module(
            &mut m,
            |ctx: NativeCallContext, f: &mut Func, args: rhai::Array| invoke(&ctx, f, args),
        );
    // f.invoke() -> result  (zero-arg convenience)
    FuncRegistration::new("invoke")
        .with_namespace(FnNamespace::Global)
        .set_into_module(&mut m, |ctx: NativeCallContext, f: &mut Func| {
            invoke(&ctx, f, rhai::Array::new())
        });

    // ffi::callback(fn_ptr, ret_type, [arg_types]) -> Callback
    m.set_native_fn(
        "callback",
        |func: FnPtr, ret: Dynamic, arg_types: rhai::Array| -> RhaiResult {
            let ret = parse_ctype(&ret)?;
            let args = parse_ctypes(&arg_types)?;
            Ok(Dynamic::from(CallbackHandle(Arc::new(make_callback(
                func, ret, args,
            )))))
        },
    );
    // cb.ptr() -> Ptr   (the C-callable code pointer to hand to a C function)
    FuncRegistration::new("ptr")
        .with_namespace(FnNamespace::Global)
        .set_into_module(&mut m, |cb: &mut CallbackHandle| -> Ptr { cb.0.code });

    register_memory(&mut m);
    register_ptr_methods(&mut m);
    m
}

/// Run a bound function, exposing the call context to any callback fired during
/// it. Restores the previous context afterwards (so nested `invoke`s work).
fn invoke(ctx: &NativeCallContext, f: &Func, args: rhai::Array) -> RhaiResult {
    let prev = CTX.with(|c| c.replace(ctx as *const NativeCallContext as *const ()));
    let r = do_call(f, args);
    CTX.with(|c| c.set(prev));
    r
}

/// `malloc`/`free`/`cstr` and typed read/write helpers.
fn register_memory(m: &mut Module) {
    // ffi::null() -> Ptr
    m.set_native_fn("null", || Ok(Dynamic::from(Ptr(std::ptr::null_mut()))));

    // ffi::malloc(n) -> Ptr   (libc allocator, free with ffi::free)
    m.set_native_fn("malloc", |n: INT| -> RhaiResult {
        if n < 0 {
            return Err(err("ffi: malloc size must be >= 0"));
        }
        // SAFETY: standard libc malloc.
        let p = unsafe { libc::malloc(n as usize) };
        if p.is_null() && n != 0 {
            return Err(err("ffi: malloc failed"));
        }
        Ok(Dynamic::from(Ptr(p)))
    });

    // ffi::free(ptr)
    m.set_native_fn("free", |p: Ptr| {
        // SAFETY: pointer must come from ffi::malloc / ffi::cstr or a C API that
        // documents libc-`free` ownership transfer. Misuse is the script's risk.
        unsafe { libc::free(p.0) };
        Ok(Dynamic::UNIT)
    });

    // ffi::cstr(s) -> Ptr   (malloc'd NUL-terminated copy; free with ffi::free)
    m.set_native_fn("cstr", |s: &str| -> RhaiResult {
        let cstring = CString::new(s).map_err(|_| err("ffi: string contains interior NUL"))?;
        let bytes = cstring.as_bytes_with_nul();
        // SAFETY: allocate len bytes and copy the NUL-terminated string in.
        unsafe {
            let p = libc::malloc(bytes.len()) as *mut u8;
            if p.is_null() {
                return Err(err("ffi: cstr malloc failed"));
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
            Ok(Dynamic::from(Ptr(p as *mut c_void)))
        }
    });

    // ffi::read_cstr(ptr) -> String
    m.set_native_fn("read_cstr", |p: Ptr| -> RhaiResult {
        if p.0.is_null() {
            return Err(err("ffi: read_cstr on null pointer"));
        }
        // SAFETY: assumes a valid NUL-terminated C string at `p`.
        let s = unsafe { CStr::from_ptr(p.0 as *const c_char) }
            .to_string_lossy()
            .into_owned();
        Ok(Dynamic::from(s))
    });

    // ffi::read_ptr(ptr, index) -> Ptr   (read a pointer-sized cell, e.g. an
    // out-param `*mut T*` or an element of a `T*[]` array)
    m.set_native_fn("read_ptr", |p: Ptr, index: INT| -> RhaiResult {
        if p.0.is_null() {
            return Err(err("ffi: read_ptr on null pointer"));
        }
        if index < 0 {
            return Err(err("ffi: read_ptr index must be >= 0"));
        }
        // SAFETY: trusts the script that a pointer is readable at this slot.
        let v = unsafe {
            (p.0 as *const *mut c_void)
                .add(index as usize)
                .read_unaligned()
        };
        Ok(Dynamic::from(Ptr(v)))
    });

    // ffi::write_ptr(ptr, index, value)   (store a pointer, e.g. building a
    // `char*[]` argument vector)
    m.set_native_fn(
        "write_ptr",
        |p: Ptr, index: INT, value: Ptr| -> RhaiResult {
            if p.0.is_null() {
                return Err(err("ffi: write_ptr to null pointer"));
            }
            if index < 0 {
                return Err(err("ffi: write_ptr index must be >= 0"));
            }
            // SAFETY: trusts the script that this slot is writable.
            unsafe {
                (p.0 as *mut *mut c_void)
                    .add(index as usize)
                    .write_unaligned(value.0)
            };
            Ok(Dynamic::UNIT)
        },
    );

    // Typed buffer reads: ffi::read_f32(ptr, count) -> [FLOAT], etc.
    read_array_fn::<f32>(m, "read_f32");
    read_array_fn::<f64>(m, "read_f64");
    read_int_array_fn::<i32>(m, "read_i32");
    read_int_array_fn::<i64>(m, "read_i64");
    read_int_array_fn::<u8>(m, "read_u8");

    // Typed scalar writes (for out-params / building input buffers):
    // ffi::write_f32(ptr, index, value)
    write_float_fn::<f32>(m, "write_f32");
    write_float_fn::<f64>(m, "write_f64");
    write_int_fn::<i32>(m, "write_i32");
    write_int_fn::<i64>(m, "write_i64");
    write_int_fn::<u8>(m, "write_u8");
}

/// Ptr accessors. These are invoked as methods (`p.is_null()`), so they're
/// registered in the global namespace rather than under `ffi::`.
fn register_ptr_methods(m: &mut Module) {
    // Infallible accessors — return plain values (not `Result`) so the
    // registration's fallible-vs-infallible overload resolves unambiguously.
    // ptr.is_null() -> bool
    FuncRegistration::new("is_null")
        .with_namespace(FnNamespace::Global)
        .set_into_module(m, |p: &mut Ptr| -> bool { p.0.is_null() });
    // ptr.addr() -> INT  (raw address, e.g. for logging)
    FuncRegistration::new("addr")
        .with_namespace(FnNamespace::Global)
        .set_into_module(m, |p: &mut Ptr| -> INT { p.0 as usize as INT });
    // ptr.offset(bytes) -> Ptr
    FuncRegistration::new("offset")
        .with_namespace(FnNamespace::Global)
        .set_into_module(m, |p: &mut Ptr, bytes: INT| -> Ptr {
            // SAFETY: byte offset; result is only valid if it stays within an
            // allocation the script owns. Not dereferenced here.
            Ptr(unsafe { (p.0 as *mut u8).offset(bytes as isize) as *mut c_void })
        });
    // to_string / debug for Ptr
    FuncRegistration::new("to_string")
        .with_namespace(FnNamespace::Global)
        .set_into_module(m, |p: &mut Ptr| -> String {
            format!("ptr(0x{:x})", p.0 as usize)
        });
    FuncRegistration::new("to_debug")
        .with_namespace(FnNamespace::Global)
        .set_into_module(m, |p: &mut Ptr| -> String {
            format!("ptr(0x{:x})", p.0 as usize)
        });
}

/// Register `name(ptr, count) -> [FLOAT]` reading `count` floats of type `T`.
fn read_array_fn<T>(m: &mut Module, name: &str)
where
    T: Copy + Into<f64> + 'static,
{
    m.set_native_fn(name, |p: Ptr, count: INT| -> RhaiResult {
        read_array_impl::<T>(p, count, |v| Dynamic::from(v.into() as FLOAT))
    });
}

/// Register `name(ptr, count) -> [INT]` reading `count` ints of type `T`.
fn read_int_array_fn<T>(m: &mut Module, name: &str)
where
    T: Copy + Into<i64> + 'static,
{
    m.set_native_fn(name, |p: Ptr, count: INT| -> RhaiResult {
        read_array_impl::<T>(p, count, |v| Dynamic::from(v.into() as INT))
    });
}

fn read_array_impl<T: Copy + 'static>(
    p: Ptr,
    count: INT,
    conv: impl Fn(T) -> Dynamic,
) -> RhaiResult {
    if p.0.is_null() {
        return Err(err("ffi: read on null pointer"));
    }
    if count < 0 {
        return Err(err("ffi: read count must be >= 0"));
    }
    let base = p.0 as *const T;
    let mut out = rhai::Array::with_capacity(count as usize);
    for i in 0..count as usize {
        // SAFETY: trusts the script's claim that `count` elements of T are
        // readable at `p`. Out-of-bounds is UB, per module docs.
        let v = unsafe { base.add(i).read_unaligned() };
        out.push(conv(v));
    }
    Ok(Dynamic::from(out))
}

/// Register `name(ptr, index, value)` writing a float `T` at element `index`.
fn write_float_fn<T>(m: &mut Module, name: &str)
where
    T: Copy + 'static,
    f64: AsCast<T>,
{
    m.set_native_fn(name, |p: Ptr, index: INT, value: FLOAT| -> RhaiResult {
        write_impl::<T>(p, index, <f64 as AsCast<T>>::cast(value))
    });
}

/// Register `name(ptr, index, value)` writing an int `T` at element `index`.
fn write_int_fn<T>(m: &mut Module, name: &str)
where
    T: Copy + 'static,
    i64: AsCast<T>,
{
    m.set_native_fn(name, |p: Ptr, index: INT, value: INT| -> RhaiResult {
        write_impl::<T>(p, index, <i64 as AsCast<T>>::cast(value))
    });
}

fn write_impl<T: Copy + 'static>(p: Ptr, index: INT, value: T) -> RhaiResult {
    if p.0.is_null() {
        return Err(err("ffi: write to null pointer"));
    }
    if index < 0 {
        return Err(err("ffi: write index must be >= 0"));
    }
    // SAFETY: trusts the script that element `index` of T is writable at `p`.
    unsafe { (p.0 as *mut T).add(index as usize).write_unaligned(value) };
    Ok(Dynamic::UNIT)
}

/// Narrowing cast helper so the write helpers can be generic over the C type.
trait AsCast<T> {
    fn cast(self) -> T;
}
macro_rules! impl_as_cast {
    ($from:ty => $($to:ty),*) => {$(
        impl AsCast<$to> for $from { fn cast(self) -> $to { self as $to } }
    )*};
}
impl_as_cast!(f64 => f32, f64);
impl_as_cast!(i64 => i32, i64, u8);

#[cfg(test)]
mod tests {
    use rhai::Engine;

    fn engine() -> Engine {
        let mut e = Engine::new();
        e.register_static_module("ffi", super::module(std::path::PathBuf::from(".")).into());
        e
    }

    /// End-to-end: bind and call the real `inference` cdylib's C ABI through the
    /// generic FFI surface. Skipped unless `target/release/libinference.so`
    /// has been built (`cargo build -p inference --release --lib`).
    #[test]
    fn calls_inference_cdylib() {
        let lib = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/target/release/libinference.so"
        );
        if !std::path::Path::new(lib).exists() {
            eprintln!("skipping: {lib} not built");
            return;
        }

        let engine = engine();

        // detect_language: ptr -> ptr (static return, no free).
        let script = format!(
            r#"
            let l = ffi::open("{lib}");
            let f = l.func("inference_detect_language", "ptr", ["ptr"]);
            let s = ffi::cstr("これは日本語のテストです");
            let lang = ffi::read_cstr(f.invoke([s]));
            ffi::free(s);
            lang
            "#
        );
        let lang: String = engine.eval(&script).expect("detect_language via ffi");
        assert!(!lang.is_empty() && lang != "und", "got language {lang:?}");

        // embed_batch: full out-param marshalling round-trip.
        let script = format!(
            r#"
            let l = ffi::open("{lib}");
            let embed = l.func("inference_embed_batch", "i32", ["ptr", "u64", "ptr", "ptr"]);
            let free_f = l.func("inference_free_float_array", "void", ["ptr", "u64"]);
            let texts = ["hello world"];
            let argv = ffi::malloc(8 * texts.len());
            let cs = ffi::cstr(texts[0]);
            ffi::write_ptr(argv, 0, cs);
            let out_flat = ffi::malloc(8);
            let out_dim = ffi::malloc(8);
            let rc = embed.invoke([argv, texts.len(), out_flat, out_dim]);
            let dim = ffi::read_i64(out_dim, 1)[0];
            let flat = ffi::read_ptr(out_flat, 0);
            let vec = ffi::read_f32(flat, dim);
            free_f.invoke([flat, dim]);
            ffi::free(cs); ffi::free(argv); ffi::free(out_flat); ffi::free(out_dim);
            vec.len()
            "#
        );
        let dim: i64 = engine.eval(&script).expect("embed_batch via ffi");
        assert!(dim > 0, "embedding dimension should be positive, got {dim}");
    }

    #[test]
    fn malloc_write_read_roundtrip() {
        let r: f64 = engine()
            .eval(
                r#"
                let p = ffi::malloc(16);
                ffi::write_f32(p, 0, 1.5);
                ffi::write_f32(p, 1, 2.5);
                let v = ffi::read_f32(p, 2);
                ffi::free(p);
                v[0] + v[1]
                "#,
            )
            .expect("roundtrip");
        assert_eq!(r, 4.0);
    }

    /// By-value struct return: glibc `div(int, int) -> div_t { int quot; int rem; }`.
    #[test]
    fn struct_return_via_libc_div() {
        let out: rhai::Array = engine()
            .eval(
                r#"
                let libc = ffi::open("libc.so.6");
                let div_t = ffi::struct_type(["i32", "i32"]);
                let div = libc.func("div", div_t, ["i32", "i32"]);
                div.invoke([17, 5])
                "#,
            )
            .expect("div via ffi");
        let quot = out[0].as_int().unwrap();
        let rem = out[1].as_int().unwrap();
        assert_eq!((quot, rem), (3, 2));
    }

    /// Callback into C: sort an int array with `qsort` and a Rhai comparator.
    #[test]
    fn callback_via_libc_qsort() {
        let sorted: rhai::Array = engine()
            .eval(
                r#"
                let libc = ffi::open("libc.so.6");
                let qsort = libc.func("qsort", "void", ["ptr", "u64", "u64", "ptr"]);

                let n = 5;
                let buf = ffi::malloc(4 * n);
                let data = [5, 2, 9, 1, 3];
                for i in 0..n { ffi::write_i32(buf, i, data[i]); }

                let cmp = ffi::callback(|a, b| {
                    let x = ffi::read_i32(a, 1)[0];
                    let y = ffi::read_i32(b, 1)[0];
                    if x < y { -1 } else if x > y { 1 } else { 0 }
                }, "i32", ["ptr", "ptr"]);

                qsort.invoke([buf, n, 4, cmp.ptr()]);
                let out = ffi::read_i32(buf, n);
                ffi::free(buf);
                out
                "#,
            )
            .expect("qsort via ffi");
        let got: Vec<i64> = sorted.iter().map(|d| d.as_int().unwrap()).collect();
        assert_eq!(got, vec![1, 2, 3, 5, 9]);
    }
}
