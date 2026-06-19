//! Regression tests pinning the pure-Rust inference to the reference outputs
//! captured from the Python/MediaPipe stack (`gen_groundtruth.py`,
//! `gen_romaji_gt.py`). These lock the bit-exact parity the port was built for.

use serde_json::Value;
use std::time::Instant;

fn load(name: &str) -> Vec<Value> {
    let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn langid_argmax_matches_reference() {
    let gt = load("groundtruth.json");
    let mut compared = 0usize;
    for entry in &gt {
        let text = entry["text"].as_str().unwrap();
        let scores = entry["scores"].as_object().unwrap();
        if scores.contains_key("__error__") {
            continue;
        }
        let ref_top = scores
            .iter()
            .max_by(|a, b| {
                a.1.as_f64()
                    .unwrap()
                    .partial_cmp(&b.1.as_f64().unwrap())
                    .unwrap()
            })
            .map(|(k, _)| k.as_str())
            .unwrap();
        let got = inference::detect_language(text);
        assert_eq!(got, ref_top, "language argmax mismatch for {text:?}");
        compared += 1;
    }
    assert!(compared > 100, "expected a sizeable corpus, got {compared}");
}

/// Requires the ~470MB MiniLM model (downloaded/cached from HF) and is gated
/// behind `--ignored` so the default `cargo test` stays fast and offline. Run:
///   cargo test -p inference --release --features minilm -- --ignored embed_matches_reference
#[cfg(feature = "minilm")]
#[test]
#[ignore]
fn embed_matches_reference() {
    let gt = load("embed_gt.json");
    let texts: Vec<String> = gt
        .iter()
        .map(|e| e["text"].as_str().unwrap().to_string())
        .collect();
    let got = inference::embed_batch(&texts).expect("embed_batch");

    let mut max_abs = 0f32;
    let mut min_cos = 1f32;
    for (i, e) in gt.iter().enumerate() {
        let want: Vec<f32> = e["vec"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap() as f32)
            .collect();
        let g = &got[i];
        assert_eq!(g.len(), want.len(), "dim mismatch for {:?}", texts[i]);
        let mut dot = 0f32;
        for (a, b) in g.iter().zip(&want) {
            max_abs = max_abs.max((a - b).abs());
            dot += a * b;
        }
        min_cos = min_cos.min(dot); // both already L2-normalized
    }
    eprintln!("[embed parity] max |Δ| = {max_abs:.6}, min cosine = {min_cos:.6}");
    // candle CPU vs torch CPU: tiny float differences only.
    assert!(min_cos > 0.999, "cosine too low: {min_cos}");
    assert!(max_abs < 1e-2, "abs diff too high: {max_abs}");
}

/// Microbenchmark of the LangID forward pass (dominated by the 3 int8 FCs).
/// Run with `--ignored --nocapture`, with and without `--features mkl-langid`:
///   cargo test -p inference --release -- --ignored --nocapture bench_langid
#[test]
#[ignore]
fn bench_langid() {
    let gt = load("groundtruth.json");
    let texts: Vec<String> = gt
        .iter()
        .filter(|e| !e["scores"].as_object().unwrap().contains_key("__error__"))
        .map(|e| e["text"].as_str().unwrap().to_string())
        .collect();

    // Warm up (loads weights once).
    let mut sink = 0usize;
    for t in &texts {
        sink ^= inference::detect_language(t).len();
    }

    let iters = 50usize;
    let t0 = Instant::now();
    for _ in 0..iters {
        for t in &texts {
            sink ^= inference::detect_language(t).len();
        }
    }
    let dt = t0.elapsed();
    let calls = iters * texts.len();
    let backend = if cfg!(feature = "mkl-langid") {
        "MKL s8u8s32"
    } else {
        "pure-Rust"
    };
    eprintln!(
        "[langid bench / {backend}] {calls} calls in {dt:?} = {:.3} µs/call (sink={sink})",
        dt.as_secs_f64() * 1e6 / calls as f64
    );
}

/// Benchmark of the batched LangID workload via `detect_romaji` (each call
/// issues one batched forward pass over all token/n-gram queries). Run with
/// `--ignored --nocapture`, with and without `--features mkl-langid`.
#[test]
#[ignore]
fn bench_romaji() {
    let gt = load("romaji_gt.json");
    let texts: Vec<String> = gt
        .iter()
        .map(|e| e["text"].as_str().unwrap().to_string())
        .collect();

    let mut sink = 0usize;
    for t in &texts {
        sink ^= inference::detect_romaji(t).len();
    }
    let iters = 200usize;
    let t0 = Instant::now();
    for _ in 0..iters {
        for t in &texts {
            sink ^= inference::detect_romaji(t).len();
        }
    }
    let dt = t0.elapsed();
    let calls = iters * texts.len();
    let backend = if cfg!(feature = "mkl-langid") {
        "MKL s8u8s32"
    } else {
        "pure-Rust"
    };
    eprintln!(
        "[romaji bench / {backend}] {calls} detect_romaji calls in {dt:?} = {:.2} µs/call (sink={sink})",
        dt.as_secs_f64() * 1e6 / calls as f64
    );
}

/// Parity of the default Model2Vec static-embedding backend against the
/// `model2vec` Python reference (`gen_m2v_gt.py`). Gated behind `--ignored`
/// (needs the ~256MB static model). Run:
///   cargo test -p inference --release -- --ignored embed_static_matches_reference
#[cfg(not(feature = "minilm"))]
#[test]
#[ignore]
fn embed_static_matches_reference() {
    let gt = load("m2v_gt.json");
    let texts: Vec<String> = gt
        .iter()
        .map(|e| e["text"].as_str().unwrap().to_string())
        .collect();
    let got = inference::embed_batch(&texts).expect("embed_batch");

    let mut max_abs = 0f32;
    let mut min_cos = 1f32;
    for (i, e) in gt.iter().enumerate() {
        let want: Vec<f32> = e["vec"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap() as f32)
            .collect();
        let g = &got[i];
        assert_eq!(g.len(), want.len(), "dim mismatch for {:?}", texts[i]);
        let mut dot = 0f32;
        for (a, b) in g.iter().zip(&want) {
            max_abs = max_abs.max((a - b).abs());
            dot += a * b;
        }
        // Empty/unk-only texts produce a zero vector in both impls; cosine is
        // undefined there, so judge those by abs diff only.
        let want_norm = want.iter().map(|x| x * x).sum::<f32>().sqrt();
        if want_norm > 0.5 {
            min_cos = min_cos.min(dot); // both already L2-normalized
        }
    }
    eprintln!("[m2v parity] max |Δ| = {max_abs:.6}, min cosine = {min_cos:.6}");
    // f16 table upcast + f32 mean vs numpy: tiny float differences only.
    assert!(min_cos > 0.9999, "cosine too low: {min_cos}");
    assert!(max_abs < 1e-3, "abs diff too high: {max_abs}");
}

/// Benchmark: default Model2Vec static backend single-text latency.
/// Run with `-- --ignored --nocapture bench_static`.
#[cfg(not(feature = "minilm"))]
#[test]
#[ignore]
fn bench_static() {
    let gt = load("m2v_gt.json");
    let texts: Vec<String> = gt
        .iter()
        .map(|e| e["text"].as_str().unwrap().to_string())
        .collect();

    let mut sink = 0usize;
    sink ^= inference::embed_batch(&texts).expect("warmup").len();

    let iters = 2000usize;
    let t0 = Instant::now();
    for _ in 0..iters {
        for t in &texts {
            sink ^= inference::embed(t).expect("embed").len();
        }
    }
    let dt = t0.elapsed();
    let calls = iters * texts.len();
    eprintln!(
        "[m2v bench] single: {calls} embed() calls in {dt:?} = {:.2} µs/call (sink={sink})",
        dt.as_secs_f64() * 1e6 / calls as f64
    );
}

/// Benchmark of the MiniLM sentence-embedding forward pass. Needs the ~470MB
/// model (cached from HF) so it's `--ignored`. Run with and without MKL:
///   cargo test -p inference --release --features minilm -- --ignored --nocapture bench_embed
///   cargo test -p inference --release --features minilm,mkl -- --ignored --nocapture bench_embed
#[cfg(feature = "minilm")]
#[test]
#[ignore]
fn bench_embed() {
    let gt = load("embed_gt.json");
    let texts: Vec<String> = gt
        .iter()
        .map(|e| e["text"].as_str().unwrap().to_string())
        .collect();
    let backend = if cfg!(feature = "mkl") {
        "MKL"
    } else {
        "pure-Rust (gemm)"
    };

    // Warm up (downloads/loads the model + tokenizer once).
    let mut sink = 0usize;
    sink ^= inference::embed_batch(&texts).expect("embed_batch").len();

    // Per-text latency (sequential single-text calls).
    let iters = 20usize;
    let t0 = Instant::now();
    for _ in 0..iters {
        for t in &texts {
            sink ^= inference::embed(t).expect("embed").len();
        }
    }
    let dt = t0.elapsed();
    let calls = iters * texts.len();
    eprintln!(
        "[embed bench / {backend}] single: {calls} embed() calls in {dt:?} = {:.1} µs/call",
        dt.as_secs_f64() * 1e6 / calls as f64
    );

    // Batched throughput (whole corpus per forward pass).
    let t1 = Instant::now();
    for _ in 0..iters {
        sink ^= inference::embed_batch(&texts).expect("embed_batch").len();
    }
    let dt1 = t1.elapsed();
    let total = iters * texts.len();
    eprintln!(
        "[embed bench / {backend}] batch({}): {total} texts in {dt1:?} = {:.1} µs/text (sink={sink})",
        texts.len(),
        dt1.as_secs_f64() * 1e6 / total as f64
    );
}

#[test]
fn detect_romaji_matches_reference() {
    let gt = load("romaji_gt.json");
    for entry in &gt {
        let text = entry["text"].as_str().unwrap();
        let want: Vec<String> = entry["final"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let got = inference::detect_romaji(text);
        assert_eq!(got, want, "detect_romaji mismatch for {text:?}");
    }
}
