//! C ABI for the learned matcher (feature `matcher`).
//!
//! ```text
//! h = inference_matcher_open(bundle_dir)            // NULL on error → inference_matcher_last_error()
//! n = inference_matcher_n_outputs(h)                // width of a score row
//!     inference_matcher_output_name(h, i)           // "p_same", …, "p_kind_cover" (static per handle)
//! rc = inference_matcher_put_facts(h, json)         // one musiclib-pair-facts/1 object or an array of them
//! s = inference_matcher_missing(h, pairs_json)      // JSON array of [source, identifier] without facts
//! rc = inference_matcher_score(h, type, a_json, b_json, out_f64[n])
//!                                                   // 0 ok, 1 error, 2 facts missing (see _missing)
//! rc = inference_matcher_embed_batch(h, texts, n, &flat, &dim)  // encoder vectors, free_float_array
//! rc = inference_matcher_prepare(h, entries_json, out_i64[n])  // view handles for [[type, pairs], …]
//!                                                   // 0 ok, 1 error, 2 facts missing (see _missing)
//! rc = inference_matcher_score_views(h, va, vb, heads, out_f64[n])  // row of two handles; heads:
//!                                                   // 1 main + guard, 2 relation heads, 3 all (rest NaN)
//! s = inference_matcher_encoder_id(h)               // embedding-cache model id ("" without manifest)
//! s = inference_matcher_entry_text(h, type, title, pairs_json)  // blocking text; NULL if facts missing
//!      inference_free_string(s); inference_matcher_close(h)
//! ```
//!
//! Pair lists are JSON arrays of `{"source", "identifier"}` maps (what Rhai's
//! `to_json(entry.pairs)` gives) or of `[source, identifier]` arrays.

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::path::Path;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use serde::Deserialize;

use crate::matcher::{Matcher, Pair, Score, features::PairFacts};

/// What a matcher handle points at. Scoring prepared views only reads it, so
/// a host may call `_score_views` from several threads at once; everything
/// that adds facts, views or vectors takes the lock exclusively.
pub type Handle = RwLock<Matcher>;

unsafe fn read<'a>(h: *const Handle) -> Option<RwLockReadGuard<'a, Matcher>> {
    unsafe { h.as_ref() }.map(|l| l.read().unwrap_or_else(|e| e.into_inner()))
}

unsafe fn write<'a>(h: *const Handle) -> Option<RwLockWriteGuard<'a, Matcher>> {
    unsafe { h.as_ref() }.map(|l| l.write().unwrap_or_else(|e| e.into_inner()))
}

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

fn set_error(msg: impl std::fmt::Display) {
    let s = CString::new(msg.to_string().replace('\0', " ")).unwrap_or_default();
    LAST_ERROR.with(|e| *e.borrow_mut() = s);
}

unsafe fn str_arg<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(p) }.to_str().ok()
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PairJson {
    Map { source: String, identifier: String },
    Tuple(String, String),
}

impl From<PairJson> for Pair {
    fn from(p: PairJson) -> Self {
        match p {
            PairJson::Map { source, identifier } => (source, identifier),
            PairJson::Tuple(s, i) => (s, i),
        }
    }
}

fn parse_pairs(json: &str) -> Result<Vec<Pair>, serde_json::Error> {
    let v: Vec<PairJson> = serde_json::from_str(json)?;
    Ok(v.into_iter().map(Pair::from).collect())
}

#[derive(Deserialize)]
#[serde(untagged)]
enum FactsJson {
    One(Box<PairFacts>),
    Many(Vec<PairFacts>),
}

/// Last error message on this thread ("" if none). Static until the next call.
#[unsafe(no_mangle)]
pub extern "C" fn inference_matcher_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().as_ptr())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn inference_matcher_open(bundle_dir: *const c_char) -> *mut Handle {
    let Some(dir) = (unsafe { str_arg(bundle_dir) }) else {
        set_error("inference_matcher_open: bundle_dir is null or not UTF-8");
        return std::ptr::null_mut();
    };
    match Matcher::open(Path::new(dir)) {
        Ok(m) => Box::into_raw(Box::new(RwLock::new(m))),
        Err(e) => {
            set_error(format!("{e:#}"));
            std::ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn inference_matcher_close(h: *mut Handle) {
    if !h.is_null() {
        drop(unsafe { Box::from_raw(h) });
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn inference_matcher_n_outputs(h: *const Handle) -> i64 {
    match unsafe { read(h) } {
        Some(m) => m.output_names().len() as i64,
        None => 0,
    }
}

thread_local! {
    static OUTPUT_NAMES: RefCell<Vec<CString>> = const { RefCell::new(Vec::new()) };
}

/// Name of output column `i`; valid until the next call on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inference_matcher_output_name(h: *const Handle, i: i64) -> *const c_char {
    let Some(m) = (unsafe { read(h) }) else {
        return std::ptr::null();
    };
    let Some(name) = m.output_names().get(i as usize) else {
        return std::ptr::null();
    };
    OUTPUT_NAMES.with(|v| {
        let mut v = v.borrow_mut();
        v.clear();
        v.push(CString::new(name.as_str()).unwrap_or_default());
        v[0].as_ptr()
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn inference_matcher_put_facts(
    h: *const Handle,
    json: *const c_char,
) -> c_int {
    let (Some(mut m), Some(json)) = (unsafe { write(h) }, unsafe { str_arg(json) }) else {
        set_error("inference_matcher_put_facts: null handle or json");
        return 1;
    };
    match serde_json::from_str::<FactsJson>(json) {
        Ok(FactsJson::One(f)) => m.put_facts(*f),
        Ok(FactsJson::Many(fs)) => fs.into_iter().for_each(|f| m.put_facts(f)),
        Err(e) => {
            set_error(format!("inference_matcher_put_facts: {e}"));
            return 1;
        }
    }
    0
}

fn to_c_json(v: &impl serde::Serialize) -> *mut c_char {
    CString::new(serde_json::to_string(v).unwrap_or_else(|_| "[]".into()))
        .unwrap_or_default()
        .into_raw()
}

/// JSON array of the given pairs that have no facts yet; free with `inference_free_string`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inference_matcher_missing(
    h: *const Handle,
    pairs_json: *const c_char,
) -> *mut c_char {
    let (Some(m), Some(json)) = (unsafe { read(h) }, unsafe { str_arg(pairs_json) }) else {
        set_error("inference_matcher_missing: null handle or json");
        return std::ptr::null_mut();
    };
    match parse_pairs(json) {
        Ok(pairs) => to_c_json(&m.missing(&pairs)),
        Err(e) => {
            set_error(format!("inference_matcher_missing: {e}"));
            std::ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn inference_matcher_score(
    h: *const Handle,
    entry_type: *const c_char,
    a_pairs_json: *const c_char,
    b_pairs_json: *const c_char,
    out: *mut f64,
) -> c_int {
    let m = unsafe { write(h) };
    let (typ, a, b) = unsafe {
        (
            str_arg(entry_type),
            str_arg(a_pairs_json),
            str_arg(b_pairs_json),
        )
    };
    let (Some(mut m), Some(typ), Some(a), Some(b)) = (m, typ, a, b) else {
        set_error("inference_matcher_score: null argument");
        return 1;
    };
    if out.is_null() {
        set_error("inference_matcher_score: null output buffer");
        return 1;
    }
    let (a, b) = match (parse_pairs(a), parse_pairs(b)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => {
            set_error(format!("inference_matcher_score: pairs: {e}"));
            return 1;
        }
    };
    match m.score(typ, &a, &b) {
        Ok(Score::Outputs(row)) => {
            unsafe { std::ptr::copy_nonoverlapping(row.as_ptr(), out, row.len()) };
            0
        }
        Ok(Score::Missing(_)) => 2,
        Err(e) => {
            set_error(format!("{e:#}"));
            1
        }
    }
}

/// Prepare a batch of entries (`[[entry_type, pairs], …]`): build their views
/// and feature sides, encoding all new texts in one batch, and write one view
/// handle per entry to `out` (i64). Handles stay valid until `_close`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inference_matcher_prepare(
    h: *const Handle,
    entries_json: *const c_char,
    out: *mut i64,
) -> c_int {
    let (Some(mut m), Some(json)) = (unsafe { write(h) }, unsafe { str_arg(entries_json) }) else {
        set_error("inference_matcher_prepare: null handle or json");
        return 1;
    };
    let entries: Vec<(String, Vec<PairJson>)> = match serde_json::from_str(json) {
        Ok(e) => e,
        Err(e) => {
            set_error(format!("inference_matcher_prepare: {e}"));
            return 1;
        }
    };
    if out.is_null() && !entries.is_empty() {
        set_error("inference_matcher_prepare: null output buffer");
        return 1;
    }
    let entries: Vec<(String, Vec<Pair>)> = entries
        .into_iter()
        .map(|(t, ps)| (t, ps.into_iter().map(Pair::from).collect()))
        .collect();
    let refs: Vec<(&str, &[Pair])> = entries
        .iter()
        .map(|(t, ps)| (t.as_str(), ps.as_slice()))
        .collect();
    match m.prepare(&refs) {
        Ok(Ok(handles)) => {
            for (k, h) in handles.into_iter().enumerate() {
                unsafe { *out.add(k) = h as i64 };
            }
            0
        }
        Ok(Err(missing)) => {
            set_error(format!(
                "inference_matcher_prepare: facts missing for {} pairs",
                missing.len()
            ));
            2
        }
        Err(e) => {
            set_error(format!("{e:#}"));
            1
        }
    }
}

/// `inference_matcher_score` for two `_prepare` handles, computing only the
/// `heads` asked for (bit 1: main classifier + guard, bit 2: relation heads;
/// other columns NaN).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inference_matcher_score_views(
    h: *const Handle,
    a: i64,
    b: i64,
    heads: i64,
    out: *mut f64,
) -> c_int {
    let Some(m) = (unsafe { read(h) }) else {
        set_error("inference_matcher_score_views: null handle");
        return 1;
    };
    if out.is_null() || a < 0 || b < 0 {
        set_error("inference_matcher_score_views: null output buffer or negative handle");
        return 1;
    }
    match m.score_views(a as usize, b as usize, heads as u32) {
        Ok(row) => {
            unsafe { std::ptr::copy_nonoverlapping(row.as_ptr(), out, row.len()) };
            0
        }
        Err(e) => {
            set_error(format!("{e:#}"));
            1
        }
    }
}

/// The encoder's identity (bundle `manifest.json`, "" if absent): the model id
/// an embedding cache should key vectors on. Free with `inference_free_string`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inference_matcher_encoder_id(h: *const Handle) -> *mut c_char {
    let Some(m) = (unsafe { read(h) }) else {
        set_error("inference_matcher_encoder_id: null handle");
        return std::ptr::null_mut();
    };
    CString::new(m.encoder_id()).unwrap_or_default().into_raw()
}

/// Semantic-blocking text for one entry (`title [A] artist` for tracks, see
/// `Matcher::entry_text`). NULL on error, including facts missing for a pair:
/// `put_facts` them first. Free with `inference_free_string`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inference_matcher_entry_text(
    h: *const Handle,
    entry_type: *const c_char,
    title: *const c_char,
    pairs_json: *const c_char,
) -> *mut c_char {
    let m = unsafe { read(h) };
    let (typ, title, pairs) = unsafe { (str_arg(entry_type), str_arg(title), str_arg(pairs_json)) };
    let (Some(m), Some(typ), Some(title), Some(pairs)) = (m, typ, title, pairs) else {
        set_error("inference_matcher_entry_text: null argument");
        return std::ptr::null_mut();
    };
    let pairs = match parse_pairs(pairs) {
        Ok(p) => p,
        Err(e) => {
            set_error(format!("inference_matcher_entry_text: pairs: {e}"));
            return std::ptr::null_mut();
        }
    };
    match m.entry_text(typ, title, &pairs) {
        Ok(text) => CString::new(text.replace('\0', " "))
            .unwrap_or_default()
            .into_raw(),
        Err(missing) => {
            set_error(format!(
                "inference_matcher_entry_text: facts missing for {missing:?}"
            ));
            std::ptr::null_mut()
        }
    }
}

/// Encoder vectors (256-d, L2-normalised) for semantic blocking. Same memory
/// contract as `inference_embed_batch`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inference_matcher_embed_batch(
    h: *const Handle,
    texts: *const *const c_char,
    n_texts: usize,
    out_flat: *mut *mut f32,
    out_dim: *mut usize,
) -> c_int {
    let Some(mut m) = (unsafe { write(h) }) else {
        set_error("inference_matcher_embed_batch: null handle");
        return 1;
    };
    if out_flat.is_null() || out_dim.is_null() || (texts.is_null() && n_texts > 0) {
        set_error("inference_matcher_embed_batch: null argument");
        return 1;
    }
    unsafe {
        *out_flat = std::ptr::null_mut();
        *out_dim = 0;
    }
    if n_texts == 0 {
        return 0;
    }
    let strings: Vec<String> = (0..n_texts)
        .map(|i| unsafe { str_arg(*texts.add(i)) }.unwrap_or("").to_owned())
        .collect();
    let refs: Vec<&str> = strings.iter().map(String::as_str).collect();
    match m.vectors_for(&refs) {
        Ok(vecs) => {
            let dim = vecs.first().map_or(0, Vec::len);
            let mut flat: Vec<f32> = vecs.into_iter().flatten().collect();
            flat.shrink_to_fit();
            unsafe {
                *out_dim = dim;
                *out_flat = flat.as_mut_ptr();
            }
            std::mem::forget(flat);
            0
        }
        Err(e) => {
            set_error(format!("{e:#}"));
            1
        }
    }
}
