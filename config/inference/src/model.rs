//! From-scratch, CPU-only reimplementation of MediaPipe's language_detector
//! tflite graph:
//!   NGramHash -> 4x KmeansEmbeddingLookup -> concat -> 3x FullyConnected -> Softmax
//! Matches the reference numerically (float dequant of the int8 hybrid weights).

use std::ffi::CString;

use crate::utf;

// ── Fixed architecture constants (from the tflite NGramHash custom_options) ──
const SEED: u64 = 10911;
const NGRAM_LENGTHS: [usize; 4] = [1, 2, 3, 4];
const VOCAB_SIZES: [u64; 4] = [5500, 5500, 13000, 13000];
const MAX_SPLITS: usize = 128;
const ENCODING_SIZE: usize = 6; // c_matrix cols
const BLOCK_SIZE: usize = 4; // b_matrix cols
const EMB_PER_NGRAM: usize = ENCODING_SIZE * BLOCK_SIZE; // 24

// Per-tensor symmetric int8 weight scales (zero_point = 0).
const FC1_SCALE: f32 = 0.027866341173648834;
const FC2_SCALE: f32 = 0.011496654711663723;
const FC3_SCALE: f32 = 0.014155043289065361;

// ── MurmurHash64WithSeed (exact port) ──────────────────────────────────────
const KMUL: u64 = 0xc6a4a7935bd1e995;

#[inline]
fn shift_mix(v: u64) -> u64 {
    v ^ (v >> 47)
}

fn murmur64(buf: &[u8], seed: u64) -> u64 {
    let len = buf.len();
    let len_aligned = len & !0x7;
    let mut hash = seed ^ (len as u64).wrapping_mul(KMUL);
    let mut p = 0;
    while p < len_aligned {
        let data = u64::from_le_bytes(buf[p..p + 8].try_into().unwrap());
        hash ^= shift_mix(data.wrapping_mul(KMUL)).wrapping_mul(KMUL);
        hash = hash.wrapping_mul(KMUL);
        p += 8;
    }
    let rem = len & 0x7;
    if rem != 0 {
        let mut data = 0u64;
        for (i, &b) in buf[len_aligned..len].iter().enumerate() {
            data |= (b as u64) << (8 * i);
        }
        hash ^= data;
        hash = hash.wrapping_mul(KMUL);
    }
    hash = shift_mix(hash).wrapping_mul(KMUL);
    shift_mix(hash)
}

// ── Tokenizer (exact port of Tokenize, exclude_nonalphaspace = true) ────────
struct Tokenized {
    str: Vec<u8>,
    tokens: Vec<(usize, usize)>, // (start, len)
}

fn tokenize(bytes: &[u8], len_bound: usize, max_tokens: usize) -> Tokenized {
    let mut out = Tokenized {
        str: Vec::with_capacity(len_bound + 2),
        tokens: Vec::with_capacity(len_bound + 2),
    };
    let mut token_start = 0usize;
    out.str.push(b'^');
    out.tokens.push((token_start, 1));
    token_start += 1;

    let limit = len_bound.min(bytes.len());
    let mut i = 0usize;
    while i < len_bound && out.tokens.len() + 1 < max_tokens {
        if i >= limit {
            break;
        }
        let (rune, br) = utf::charntorune(&bytes[i..limit]);
        if br == 0 {
            break;
        }
        if !utf::isalpharune(rune) {
            // non-alphanumeric -> replacement token " "
            out.str.push(b' ');
            out.tokens.push((token_start, 1));
            token_start += 1;
            i += br;
            continue;
        }
        out.str.extend_from_slice(&bytes[i..i + br]);
        out.tokens.push((token_start, br));
        token_start += br;
        i += br;
    }
    out.str.push(b'$');
    out.tokens.push((token_start, 1));
    out
}

// ── NGramHash op ────────────────────────────────────────────────────────────
// Returns indices[ngram][token] for the 4 ngram lengths.
fn ngram_hash(text: &str) -> Vec<Vec<i32>> {
    let input = text.as_bytes();
    let mut lower = utf::lowercase_unicode(input);
    // The C op calls Tokenize with the ORIGINAL byte length as the bound while
    // reading from the lowercased std::string. When lowercasing shrinks the
    // byte length (e.g. İ→i, ẞ→ß), the tokenizer reads past the lowercased
    // content into the c_str's NUL terminator, which becomes a replacement
    // (space) token. Zero-padding to the original length reproduces this.
    if lower.len() < input.len() {
        lower.resize(input.len(), 0);
    }
    let tok = tokenize(&lower, input.len(), MAX_SPLITS);
    let num_tokens = tok.tokens.len();

    let mut out = vec![vec![0i32; num_tokens]; NGRAM_LENGTHS.len()];
    for (g, (&ngram_length, &vocab_size)) in
        NGRAM_LENGTHS.iter().zip(VOCAB_SIZES.iter()).enumerate()
    {
        for start in 0..num_tokens {
            let mut num_bytes = 0usize;
            let end = (start + ngram_length).min(num_tokens);
            for t in start..end {
                num_bytes += tok.tokens[t].1;
            }
            let off = tok.tokens[start].0;
            let h = murmur64(&tok.str[off..off + num_bytes], SEED);
            out[g][start] = ((h % vocab_size) + 1) as i32;
        }
    }
    out
}

// ── KmeansEmbeddingLookup op ────────────────────────────────────────────────
fn kmeans_lookup(indices: &[i32], encoding_table: &[u8], codebook: &[f32]) -> Vec<f32> {
    let mut acc = vec![0f32; EMB_PER_NGRAM];
    let mut num = 0i32;
    for &token in indices {
        if token == 0 {
            break;
        }
        num += 1;
        let t = token as usize;
        for enc in 0..ENCODING_SIZE {
            let cb_idx = encoding_table[t * ENCODING_SIZE + enc] as usize;
            for bo in 0..BLOCK_SIZE {
                acc[enc * BLOCK_SIZE + bo] += codebook[cb_idx * BLOCK_SIZE + bo];
            }
        }
    }
    let denom = num.max(1) as f32;
    for v in acc.iter_mut() {
        *v /= denom;
    }
    acc
}

// ── Weights ─────────────────────────────────────────────────────────────────
pub struct Weights {
    // codebook (b_matrix, f32 [256,4]) and encoding table (c_matrix, u8 [V+1,6]) per ngram
    pub b: [Vec<f32>; 4],
    pub c: [Vec<u8>; 4],
    pub fc1_w: Vec<i8>, // [160,96]
    pub fc1_b: Vec<f32>,
    pub fc2_w: Vec<i8>, // [200,160]
    pub fc2_b: Vec<f32>,
    pub fc3_w: Vec<i8>, // [111,200]
    pub fc3_b: Vec<f32>,
    // Per-output-row sums of the int8 weights, precomputed for the input
    // zero-point correction (`acc - offset * row_sum`).
    pub fc1_ws: Vec<i32>,
    pub fc2_ws: Vec<i32>,
    pub fc3_ws: Vec<i32>,
    pub labels: Vec<String>,
    /// NUL-terminated copies of `labels`, so the C ABI can hand out a valid
    /// C string pointer without over-reading past the `str` end.
    pub labels_c: Vec<CString>,
}

/// Per-output-row sums of an `[n_out, n_in]` int8 weight matrix.
fn row_sums(w: &[i8], n_in: usize, n_out: usize) -> Vec<i32> {
    (0..n_out)
        .map(|o| w[o * n_in..(o + 1) * n_in].iter().map(|&x| x as i32).sum())
        .collect()
}

/// Weight layout consumed by `int8_matvec`. The pure-Rust path keeps the
/// natural `[n_out, n_in]` row-major layout; the MKL path needs the transpose
/// `[n_in, n_out]` so the weights can serve as the int8 (second) operand of
/// `cblas_gemm_s8u8s32` without a runtime transpose.
#[cfg(not(feature = "mkl-langid"))]
fn matvec_weights(w: Vec<i8>, _n_in: usize, _n_out: usize) -> Vec<i8> {
    w
}
#[cfg(feature = "mkl-langid")]
fn matvec_weights(w: Vec<i8>, n_in: usize, n_out: usize) -> Vec<i8> {
    let mut t = vec![0i8; w.len()];
    for o in 0..n_out {
        for i in 0..n_in {
            t[i * n_out + o] = w[o * n_in + i];
        }
    }
    t
}

impl Weights {
    /// Load weights straight out of the bundled `language_detector.tflite`
    /// flatbuffer. Buffer indices are fixed for this model (buffer = tensor+1).
    pub fn load_tflite(model: &[u8], labels_txt: &str) -> Self {
        let bufs = crate::tflite::buffers(model);
        let f32v = |i: usize| -> Vec<f32> {
            bufs[i]
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
                .collect()
        };
        let i8v = |i: usize| -> Vec<i8> { bufs[i].iter().map(|&b| b as i8).collect() };
        let u8v = |i: usize| -> Vec<u8> { bufs[i].to_vec() };
        let fc1_w0 = i8v(13);
        let fc2_w0 = i8v(14);
        let fc3_w0 = i8v(15);
        let fc1_ws = row_sums(&fc1_w0, 96, 160);
        let fc2_ws = row_sums(&fc2_w0, 160, 200);
        let fc3_ws = row_sums(&fc3_w0, 200, 111);
        // Layout for int8_matvec (transposed under the MKL backend).
        let fc1_w = matvec_weights(fc1_w0, 96, 160);
        let fc2_w = matvec_weights(fc2_w0, 160, 200);
        let fc3_w = matvec_weights(fc3_w0, 200, 111);
        Weights {
            b: [f32v(2), f32v(4), f32v(6), f32v(8)],
            c: [u8v(3), u8v(5), u8v(7), u8v(9)],
            fc1_b: f32v(10),
            fc2_b: f32v(11),
            fc3_b: f32v(12),
            fc1_w,
            fc2_w,
            fc3_w,
            fc1_ws,
            fc2_ws,
            fc3_ws,
            labels: labels_txt.lines().map(|s| s.to_string()).collect(),
            labels_c: labels_txt
                .lines()
                .map(|s| CString::new(s).expect("label contains interior NUL"))
                .collect(),
        }
    }
}

/// AsymmetricQuantizeFloats (exact port — note the deliberate f64/f32
/// boundaries). Returns `(input_scale, zero_point_offset, q)` where `q` is the
/// asymmetric int8 quantization of `x` (already includes the offset).
fn quantize_asym(x: &[f32]) -> (f32, i32, Vec<i8>) {
    let n_in = x.len();
    let xmin = x.iter().copied().fold(f32::INFINITY, f32::min);
    let xmax = x.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let rmin = (xmin.min(0.0)) as f64;
    let rmax = (xmax.max(0.0)) as f64;
    if rmin == rmax {
        return (1.0, 0, vec![0i8; n_in]);
    }
    let scale = (rmax - rmin) / 255.0; // (qmax - qmin) = 127 - (-128)
    let zp_from_min = -128.0 - rmin / scale;
    let zp_from_max = 127.0 - rmax / scale;
    let err_min = 128.0 + (rmin / scale).abs();
    let err_max = 127.0 + (rmax / scale).abs();
    let zp_double = if err_min < err_max {
        zp_from_min
    } else {
        zp_from_max
    };
    let offset = if zp_double <= -128.0 {
        -128
    } else if zp_double >= 127.0 {
        127
    } else {
        zp_double.round() as i32
    };
    let scale_f32 = scale as f32;
    let inv = 1.0f32 / scale_f32;
    let q = x
        .iter()
        .map(|&v| (((v * inv).round() as i32) + offset).clamp(-128, 127) as i8)
        .collect();
    (scale_f32, offset, q)
}

/// Exact int32 dot products `acc[b,o] = Σ_i w[o,i]·qx[b,i]` for an
/// `[n_out, n_in]` signed-int8 weight matrix against a batch of `batch`
/// signed-int8 activation vectors (`qx` is `[batch, n_in]` row-major; the result
/// is `[batch, n_out]`). Pure-Rust reference implementation. (`row_sum` is
/// unused here; the MKL backend needs it for its uint8-shift correction.)
#[cfg(not(feature = "mkl-langid"))]
fn int8_matmul(
    w: &[i8],
    qx: &[i8],
    _row_sum: &[i32],
    batch: usize,
    n_in: usize,
    n_out: usize,
) -> Vec<i32> {
    let mut acc = vec![0i32; batch * n_out];
    for b in 0..batch {
        let qrow = &qx[b * n_in..(b + 1) * n_in];
        for o in 0..n_out {
            let wrow = &w[o * n_in..(o + 1) * n_in];
            let mut s: i32 = 0;
            for i in 0..n_in {
                s += (wrow[i] as i32) * (qrow[i] as i32);
            }
            acc[b * n_out + o] = s;
        }
    }
    acc
}

/// MKL int8 GEMM (`cblas_gemm_s8u8s32`) backend, returning the same true
/// `acc[b,o] = Σ_i w[o,i]·qx[b,i]` as the Rust path. `w` is the transposed
/// weights `Wt[n_in, n_out]` (see `matvec_weights`).
///
/// `cblas_gemm_s8u8s32`'s first operand is uint8 and its second is int8, so the
/// uint8 activations are operand A and the int8 weights are operand B. `qx` is
/// signed int8 and the routine's int8 offsets can't express the needed +128
/// shift, so we feed `qx+128` (valid uint8) with zero offsets and undo the
/// shift exactly afterwards:
///   Σ (qx+128)·w = Σ qx·w + 128·Σ w  ⟹  acc = raw − 128·row_sum.
/// Pure integer arithmetic ⇒ bit-identical to the Rust loop.
#[cfg(feature = "mkl-langid")]
fn int8_matmul(
    w: &[i8],
    qx: &[i8],
    row_sum: &[i32],
    batch: usize,
    n_in: usize,
    n_out: usize,
) -> Vec<i32> {
    let a_u8: Vec<u8> = qx.iter().map(|&v| (v as i16 + 128) as u8).collect();
    let mut acc = vec![0i32; batch * n_out];
    // C[batch, n_out] = A_u8[batch, n_in] · Wt_s8[n_in, n_out]
    mkl_gemm_u8s8s32(&a_u8, w, &mut acc, batch, n_out, n_in);
    for b in 0..batch {
        for o in 0..n_out {
            acc[b * n_out + o] -= 128 * row_sum[o];
        }
    }
    acc
}

/// Thin wrapper over `cblas_gemm_s8u8s32` (row-major, no transpose, fixed zero
/// offsets): `c[m,n] = a_u8[m,k] · b_s8[k,n]`. Note MKL's operand order for this
/// routine is (uint8, int8) despite the `s8u8` in the name.
#[cfg(feature = "mkl-langid")]
fn mkl_gemm_u8s8s32(a_u8: &[u8], b_s8: &[i8], c: &mut [i32], m: usize, n: usize, k: usize) {
    use std::os::raw::{c_float, c_int, c_void};
    const ROW_MAJOR: c_int = 101;
    const NO_TRANS: c_int = 111;
    const FIX_OFFSET: c_int = 173;
    unsafe extern "C" {
        fn cblas_gemm_s8u8s32(
            layout: c_int,
            transa: c_int,
            transb: c_int,
            offsetc: c_int,
            m: c_int,
            n: c_int,
            k: c_int,
            alpha: c_float,
            a: *const c_void,
            lda: c_int,
            ao: i8,
            b: *const c_void,
            ldb: c_int,
            bo: i8,
            beta: c_float,
            c: *mut i32,
            ldc: c_int,
            co: *const i32,
        );
    }
    let co = [0i32];
    unsafe {
        cblas_gemm_s8u8s32(
            ROW_MAJOR,
            NO_TRANS,
            NO_TRANS,
            FIX_OFFSET,
            m as c_int,
            n as c_int,
            k as c_int,
            1.0,
            a_u8.as_ptr() as *const c_void,
            k as c_int,
            0,
            b_s8.as_ptr() as *const c_void,
            n as c_int,
            0,
            0.0,
            c.as_mut_ptr(),
            n as c_int,
            co.as_ptr(),
        );
    }
}

#[cfg(all(test, feature = "mkl-langid"))]
mod mkl_tests {
    use super::*;
    #[test]
    fn gemm_matches_naive() {
        // A [2,3] uint8, B [3,2] int8 (with negatives to confirm signedness).
        let a: Vec<u8> = vec![1, 200, 3, 250, 5, 60];
        let b: Vec<i8> = vec![10, -20, -30, 40, 50, -60];
        let mut c = vec![0i32; 4];
        mkl_gemm_u8s8s32(&a, &b, &mut c, 2, 2, 3);
        let mut want = vec![0i32; 4];
        for i in 0..2 {
            for j in 0..2 {
                let mut s = 0i32;
                for k in 0..3 {
                    s += a[i * 3 + k] as i32 * b[k * 2 + j] as i32;
                }
                want[i * 2 + j] = s;
            }
        }
        assert_eq!(c, want, "mkl u8s8s32 disagrees with naive int matmul");
    }
}

/// Batched TFLite hybrid FullyConnected with `asymmetric_quantize_inputs=true`.
/// `xs` is `[batch, n_in]` row-major; the result is `[batch, n_out]`. Each row
/// is quantized independently (its own scale/offset), an exact int32 GEMM is
/// taken against the int8 weights, the per-row input zero-point is corrected via
/// the precomputed per-output weight sum, then each output is rescaled by
/// input_scale·weight_scale and biased.
fn fully_connected_batch(
    xs: &[f32],
    batch: usize,
    w: &[i8],
    bias: &[f32],
    row_sum: &[i32],
    wscale: f32,
    relu: bool,
) -> Vec<f32> {
    let n_out = bias.len();
    let n_in = xs.len() / batch;

    let mut scales = vec![0f32; batch];
    let mut offsets = vec![0i32; batch];
    let mut qx = vec![0i8; batch * n_in];
    for b in 0..batch {
        let (scale_f32, offset, q) = quantize_asym(&xs[b * n_in..(b + 1) * n_in]);
        scales[b] = scale_f32;
        offsets[b] = offset;
        qx[b * n_in..(b + 1) * n_in].copy_from_slice(&q);
    }

    let acc = int8_matmul(w, &qx, row_sum, batch, n_in, n_out);

    let mut y = vec![0f32; batch * n_out];
    for b in 0..batch {
        let combined = scales[b] * wscale;
        let offset = offsets[b];
        for o in 0..n_out {
            let corrected = acc[b * n_out + o] - offset * row_sum[o];
            let mut v = (corrected as f32) * combined + bias[o];
            if relu && v < 0.0 {
                v = 0.0;
            }
            y[b * n_out + o] = v;
        }
    }
    y
}

fn softmax(x: &[f32]) -> Vec<f32> {
    let max = x.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let mut e: Vec<f32> = x.iter().map(|&v| (v - max).exp()).collect();
    let sum: f32 = e.iter().sum();
    for v in e.iter_mut() {
        *v /= sum;
    }
    e
}

/// Batched full forward pass: a slice of texts -> probabilities per label for
/// each. Sharing one GEMM per layer across the batch is where the MKL int8
/// backend pays off (vs. per-text single-vector calls).
pub fn predict_batch(texts: &[&str], w: &Weights) -> Vec<Vec<f32>> {
    let batch = texts.len();
    if batch == 0 {
        return Vec::new();
    }
    let mut concat = Vec::with_capacity(batch * 4 * EMB_PER_NGRAM);
    for text in texts {
        let indices = ngram_hash(text);
        for g in 0..4 {
            concat.extend(kmeans_lookup(&indices[g], &w.c[g], &w.b[g]));
        }
    }
    let h1 = fully_connected_batch(
        &concat, batch, &w.fc1_w, &w.fc1_b, &w.fc1_ws, FC1_SCALE, true,
    );
    let h2 = fully_connected_batch(&h1, batch, &w.fc2_w, &w.fc2_b, &w.fc2_ws, FC2_SCALE, true);
    let logits = fully_connected_batch(&h2, batch, &w.fc3_w, &w.fc3_b, &w.fc3_ws, FC3_SCALE, false);
    let n_out = w.fc3_b.len();
    (0..batch)
        .map(|b| softmax(&logits[b * n_out..(b + 1) * n_out]))
        .collect()
}

/// Full forward pass: raw text -> probability per label.
pub fn predict(text: &str, w: &Weights) -> Vec<f32> {
    predict_batch(&[text], w).pop().unwrap()
}
