//! Pure-CPU, from-scratch reimplementation of the inference in
//! `src/bin/embedding_server.py`, packaged as a plain C-ABI cdylib.
//!
//! The model assets are embedded into the cdylib (`include_bytes!`), so the
//! library is self-contained: no Python, no TFLite runtime, no GPU.
//!
//! The cdylib exports a stable, language-independent C API (see the
//! "Stable C ABI" section below); the host binds it at runtime through its
//! generic `ffi` Rhai module. The crate has no dependency on rhai — values
//! cross the boundary only as plain C types, so there's no version/TypeId
//! coupling between host and plugin.
//!
//!   * `inference_detect_romaji(text)`   -> tokens classified as romanised JP.
//!   * `inference_detect_language(text)` -> top BCP-47 language code (LangID).
//!   * `inference_embed_batch(texts)`    -> sentence embeddings (MiniLM 384-d,
//!     `minilm` feature only; without it every call fails).
//!   * `inference_matcher_*`             -> learned pair matcher (`matcher`
//!     feature; see `matcher_abi.rs`).
//!   * `inference_typesafe_*`            -> TypeSafe System One client
//!     (`typesafe` feature; see `typesafe.rs`).

#[cfg(feature = "minilm")]
mod embed;
#[cfg(feature = "matcher")]
pub mod matcher;
#[cfg(feature = "matcher")]
mod matcher_abi;
mod model;
mod romaji;
mod tables;
mod tflite;
#[cfg(feature = "typesafe")]
pub mod typesafe;
mod utf;

use std::sync::OnceLock;

#[cfg(feature = "minilm")]
use embed::Embedder;
use model::Weights;
use romaji::Romaji;

// candle-core's MKL backend references `hgemm_` (half-precision GEMM), but the
// Intel MKL build pulled in by `intel-mkl-src` (2020.1) doesn't export it. We
// only ever run F32 matmuls, so the f16 path is unreachable — this stub just
// satisfies the linker. The parameter list is irrelevant to C symbol resolution.
#[cfg(feature = "mkl")]
#[unsafe(no_mangle)]
pub extern "C" fn hgemm_() {
    unreachable!("f16 GEMM is never invoked: all inference tensors are F32");
}

// ── Embedded model assets ───────────────────────────────────────────────────
const MODEL_TFLITE: &[u8] = include_bytes!("../assets/language_detector.tflite");
const LABELS: &str = include_str!("../assets/labels.txt");
const EN_ZIPF: &str = include_str!("../assets/en_zipf.tsv");

/// Lazily-initialised, shared inference state (loaded once per process).
struct Engine {
    weights: Weights,
    romaji: Romaji,
}

fn engine() -> &'static Engine {
    static E: OnceLock<Engine> = OnceLock::new();
    E.get_or_init(|| Engine {
        weights: Weights::load_tflite(MODEL_TFLITE, LABELS),
        romaji: Romaji::new(EN_ZIPF),
    })
}

/// Lazily-loaded MiniLM embedder (downloads/caches the model on first use).
/// Stored as a `Result` so a load failure surfaces to the caller rather than
/// poisoning the process.
#[cfg(feature = "minilm")]
fn embedder() -> Result<&'static Embedder, String> {
    static E: OnceLock<Result<Embedder, String>> = OnceLock::new();
    E.get_or_init(|| Embedder::load().map_err(|e| format!("{e:#}")))
        .as_ref()
        .map_err(|e| e.clone())
}

#[cfg(not(feature = "minilm"))]
const NO_EMBEDDER: &str = "no embedding backend compiled in (build with --features minilm)";

// Public accessors so the crate can also be used as an `rlib` (tests, direct
// host linking) without going through the dylib boundary.

/// Tokens of `text` classified as romanised Japanese (`ja-Latn`).
pub fn detect_romaji(text: &str) -> Vec<String> {
    let e = engine();
    e.romaji.detect(text, &e.weights)
}

/// Index of the top language label for `text` per the LangID model.
fn detect_language_idx(text: &str) -> usize {
    let e = engine();
    let probs = model::predict(text, &e.weights);
    probs
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
        .map(|(mi, _)| mi)
        .unwrap()
}

/// Top language code for `text` per the LangID model.
pub fn detect_language(text: &str) -> &'static str {
    &engine().weights.labels[detect_language_idx(text)]
}

/// Top language code for `text` as a NUL-terminated C string, for the C ABI.
fn detect_language_cstr(text: &str) -> &'static std::ffi::CStr {
    &engine().weights.labels_c[detect_language_idx(text)]
}

/// Sentence embedding for `text` (L2-normalized), using the active backend.
#[cfg(feature = "minilm")]
pub fn embed(text: &str) -> Result<Vec<f32>, String> {
    embedder()?.embed(text).map_err(|e| format!("{e:#}"))
}

/// Always fails: no embedding backend without the `minilm` feature.
#[cfg(not(feature = "minilm"))]
pub fn embed(_text: &str) -> Result<Vec<f32>, String> {
    Err(NO_EMBEDDER.to_owned())
}

/// Sentence embeddings for a batch of texts, using the active backend.
#[cfg(feature = "minilm")]
pub fn embed_batch(texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    embedder()?.embed_batch(texts).map_err(|e| format!("{e:#}"))
}

/// Always fails: no embedding backend without the `minilm` feature.
#[cfg(not(feature = "minilm"))]
pub fn embed_batch(_texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    Err(NO_EMBEDDER.to_owned())
}

// ── Stable C ABI (version 1) ──────────────────────────────────────────────────
//
// The library's entire public surface. Language-independent and rhai-free; the
// host binds these symbols at runtime via its generic `ffi` Rhai module.
//
// Memory contract
// ───────────────
// • `inference_detect_romaji` – returns a `*mut *mut c_char` of `*out_len`
//   independently-heap-allocated strings; free with `inference_free_string_array`.
// • `inference_embed_batch`   – returns a flat `*mut f32` buffer of
//   `n_texts * *out_dim` values; free with `inference_free_float_array`.
// • `inference_detect_language` – returns a NUL-terminated pointer into static
//   storage (the interned per-label `CString`s); always valid, no free needed.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn inference_detect_romaji(
    text: *const c_char,
    out_len: *mut usize,
) -> *mut *mut c_char {
    unsafe {
        if text.is_null() || out_len.is_null() {
            return std::ptr::null_mut();
        }
        let s = match CStr::from_ptr(text).to_str() {
            Ok(s) => s,
            Err(_) => return std::ptr::null_mut(),
        };
        let tokens = detect_romaji(s);
        let mut ptrs: Vec<*mut c_char> = tokens
            .into_iter()
            .map(|tok| CString::new(tok).unwrap_or_default().into_raw())
            .collect();
        ptrs.shrink_to_fit();
        *out_len = ptrs.len();
        let raw = ptrs.as_mut_ptr();
        std::mem::forget(ptrs);
        raw
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn inference_free_string_array(arr: *mut *mut c_char, len: usize) {
    unsafe {
        if arr.is_null() {
            return;
        }
        let ptrs = Vec::from_raw_parts(arr, len, len);
        for p in &ptrs {
            if !p.is_null() {
                drop(CString::from_raw(*p));
            }
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn inference_detect_language(text: *const c_char) -> *const c_char {
    unsafe {
        const FALLBACK: &[u8] = b"und\0";
        if text.is_null() {
            return FALLBACK.as_ptr() as *const c_char;
        }
        match CStr::from_ptr(text).to_str() {
            Ok(s) => detect_language_cstr(s).as_ptr(),
            Err(_) => FALLBACK.as_ptr() as *const c_char,
        }
    }
}

/// Embed `n_texts` strings.  On success `*out_flat` points to a flat
/// `f32` buffer of `n_texts × *out_dim` values (row-major); free with
/// `inference_free_float_array(ptr, n_texts * dim)`.
/// Empty input (`n_texts == 0`) succeeds with `*out_flat = null, *out_dim = 0`.
/// Returns 0 on success, 1 on error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inference_embed_batch(
    texts: *const *const c_char,
    n_texts: usize,
    out_flat: *mut *mut f32,
    out_dim: *mut usize,
) -> c_int {
    unsafe {
        if out_flat.is_null() || out_dim.is_null() {
            return 1;
        }
        *out_flat = std::ptr::null_mut();
        *out_dim = 0;
        if n_texts == 0 {
            return 0;
        }
        if texts.is_null() {
            return 1;
        }
        let strings: Vec<String> = (0..n_texts)
            .map(|i| {
                let p = *texts.add(i);
                if p.is_null() {
                    String::new()
                } else {
                    CStr::from_ptr(p).to_str().unwrap_or("").to_string()
                }
            })
            .collect();
        match embed_batch(&strings) {
            Err(_) => 1,
            Ok(vecs) => {
                let dim = vecs.first().map_or(0, Vec::len);
                *out_dim = dim;
                if dim == 0 {
                    return 0;
                }
                let mut flat: Vec<f32> = vecs.into_iter().flatten().collect();
                flat.shrink_to_fit();
                let raw = flat.as_mut_ptr();
                std::mem::forget(flat);
                *out_flat = raw;
                0
            }
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn inference_free_float_array(ptr: *mut f32, len: usize) {
    unsafe {
        if !ptr.is_null() && len > 0 {
            drop(Vec::from_raw_parts(ptr, len, len));
        }
    }
}

/// Frees a string returned by any `inference_*` call that hands back an owned
/// `char*` (matcher, TypeSafe client).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inference_free_string(p: *mut c_char) {
    if !p.is_null() {
        drop(unsafe { CString::from_raw(p) });
    }
}
