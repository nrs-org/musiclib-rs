//! Native helpers for the soft-dedup scripts, packaged as a plain C-ABI cdylib.
//!
//! The cdylib exports a stable, language-independent C API; the host binds it
//! at runtime through its generic `ffi` Rhai module (`config/match.learned.rhai`
//! and `config/jev.rhai`). The crate has no dependency on rhai — values cross
//! the boundary only as plain C types, so there's no version/TypeId coupling
//! between host and plugin.
//!
//!   * `inference_matcher_*`  -> learned pair matcher (`matcher` feature; see
//!     `matcher_abi.rs`).
//!   * `inference_typesafe_*` -> TypeSafe System One client (`typesafe`
//!     feature; see `typesafe.rs`).
//!   * `inference_free_string` / `inference_free_float_array` -> free buffers
//!     handed back by the calls above.

#[cfg(feature = "matcher")]
pub mod matcher;
#[cfg(feature = "matcher")]
mod matcher_abi;
#[cfg(feature = "typesafe")]
pub mod typesafe;

use std::ffi::CString;
use std::os::raw::c_char;

/// Frees a flat `f32` buffer of `len` values returned by
/// `inference_matcher_embed_batch`.
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
