//! Parity of the learned matcher against the Python reference fixtures
//! (`train/learned-matcher/export_parity.py` → data/learned-matcher/parity/<model>/)
//! and the runtime bundle (`bundle.py` → data/learned-matcher/bundle/<model>/).
//!
//!     cargo test -p inference --features matcher --release --test matcher_parity -- --nocapture
//!
//! Skips (passes with a note) when the gitignored fixture or bundle is absent.
//! Override locations with MATCHER_PARITY_DIR / MATCHER_BUNDLE_DIR.
#![cfg(feature = "matcher")]

use std::collections::HashMap;
use std::path::PathBuf;

use inference::matcher::{
    Matcher, Pair, feature_layout,
    features::{Features, PairFacts},
};
use serde_json::Value;

const MODEL: &str = "v17";

fn dirs() -> Option<(PathBuf, PathBuf)> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/learned-matcher");
    let parity = std::env::var("MATCHER_PARITY_DIR")
        .map(PathBuf::from)
        .unwrap_or(root.join("parity").join(MODEL));
    let bundle = std::env::var("MATCHER_BUNDLE_DIR")
        .map(PathBuf::from)
        .unwrap_or(root.join("bundle").join(MODEL));
    if parity.join("pairs.jsonl").exists() && bundle.join("model.txt").exists() {
        Some((parity, bundle))
    } else {
        eprintln!(
            "skipping: no fixture at {} or bundle at {}",
            parity.display(),
            bundle.display()
        );
        None
    }
}

struct Fixture {
    feature_names: Vec<String>,
    thresholds: HashMap<String, f64>,
    rows: Vec<Value>,
    entries: HashMap<i64, Vec<Pair>>,
    facts: Vec<PairFacts>,
    texts: Vec<String>,
    vectors: Vec<f32>,
}

fn lines(path: PathBuf) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn load(parity: &std::path::Path) -> Fixture {
    let meta: Value =
        serde_json::from_str(&std::fs::read_to_string(parity.join("meta.json")).unwrap()).unwrap();
    let entries = lines(parity.join("entries.jsonl"))
        .into_iter()
        .map(|e| {
            let pairs = e["pairs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| {
                    (
                        p[0].as_str().unwrap().to_owned(),
                        p[1].as_str().unwrap().to_owned(),
                    )
                })
                .collect();
            (e["entry_id"].as_i64().unwrap(), pairs)
        })
        .collect();
    let facts = std::fs::read_to_string(parity.join("facts.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let bytes = std::fs::read(parity.join("vectors.f32")).unwrap();
    Fixture {
        feature_names: serde_json::from_value(meta["feature_names"].clone()).unwrap(),
        thresholds: serde_json::from_value(meta["thresholds"].clone()).unwrap(),
        rows: lines(parity.join("pairs.jsonl")),
        entries,
        facts,
        texts: serde_json::from_str(&std::fs::read_to_string(parity.join("texts.json")).unwrap())
            .unwrap(),
        vectors: bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
            .collect(),
    }
}

fn val(v: &Value) -> f64 {
    v.as_f64().unwrap_or(f64::NAN)
}

/// Python's `policy.verdicts` for one row.
fn verdict(typ: &str, out: &[f64], thresholds: &HashMap<String, f64>) -> &'static str {
    let (same, rest) = (
        out[0],
        [
            ("RELATE", out[1]),
            ("SIBLING", out[2]),
            ("DISTINCT", out[3]),
        ],
    );
    if same >= *thresholds.get(typ).unwrap_or(&1.0) {
        return if out[4] > 0.0 { "DEFER" } else { "MERGE" };
    }
    let mut best = rest[0];
    for r in &rest[1..] {
        if r.1 > best.1 {
            best = *r;
        }
    }
    if same > best.1 {
        return "DEFER";
    }
    if best.0 == "RELATE" && !out[5].is_nan() && out[5] > 0.5 {
        return "SIBLING";
    }
    best.0
}

fn expected_outputs(row: &Value) -> Vec<f64> {
    let mut v: Vec<f64> = row["p"].as_array().unwrap().iter().map(val).collect();
    v.push(if row["guard"].as_bool().unwrap() {
        1.0
    } else {
        0.0
    });
    v.push(val(&row["p_sibling_structure"]));
    v.push(val(&row["p_a_derived"]));
    v.extend(row["kind_p"].as_array().unwrap().iter().map(val));
    v
}

fn python_features(fx: &Fixture, row: &Value) -> Features {
    let by_name: HashMap<&str, f64> = fx
        .feature_names
        .iter()
        .map(String::as_str)
        .zip(row["features"].as_array().unwrap().iter().map(val))
        .collect();
    Features(
        feature_layout()
            .into_iter()
            .map(|n| (n, by_name[n]))
            .collect(),
    )
}

fn same(a: f64, b: f64, tol: f64) -> bool {
    (a.is_nan() && b.is_nan()) || (a - b).abs() <= tol
}

fn open(bundle: &std::path::Path, fx: &Fixture, python_vectors: bool) -> Matcher {
    let mut m = Matcher::open(bundle).unwrap();
    for f in &fx.facts {
        m.put_facts(f.clone());
    }
    if python_vectors {
        for (i, t) in fx.texts.iter().enumerate() {
            m.insert_vector(t.clone(), fx.vectors[i * 256..(i + 1) * 256].to_vec());
        }
    }
    m
}

fn pairs_of<'a>(fx: &'a Fixture, row: &Value, side: &str) -> &'a [Pair] {
    &fx.entries[&row[side].as_i64().unwrap()]
}

#[test]
fn trees_match_python_on_python_features() {
    let Some((parity, bundle)) = dirs() else {
        return;
    };
    let fx = load(&parity);
    let m = open(&bundle, &fx, true);
    let mut worst = 0.0f64;
    for row in &fx.rows {
        let typ = row["type"].as_str().unwrap();
        let got = m.outputs(typ, &python_features(&fx, row));
        let want = expected_outputs(row);
        assert_eq!(got.len(), want.len());
        for (g, w) in got.iter().zip(&want) {
            assert!(same(*g, *w, 1e-9), "{row}: got {got:?}, want {want:?}");
            if !g.is_nan() {
                worst = worst.max((g - w).abs());
            }
        }
        assert_eq!(
            verdict(typ, &got, &fx.thresholds),
            row["verdict"].as_str().unwrap(),
            "{row}"
        );
    }
    eprintln!("trees: {} rows, max |Δp| {worst:.2e}", fx.rows.len());
}

#[test]
fn features_from_facts_match_python() {
    let Some((parity, bundle)) = dirs() else {
        return;
    };
    let fx = load(&parity);
    let mut m = open(&bundle, &fx, true);
    let mut bad: HashMap<&str, (usize, String)> = HashMap::new();
    let mut flips = Vec::new();
    for row in &fx.rows {
        let typ = row["type"].as_str().unwrap();
        let f = m
            .features(
                typ,
                pairs_of(&fx, row, "entry_a"),
                pairs_of(&fx, row, "entry_b"),
            )
            .unwrap()
            .unwrap();
        let want: HashMap<&str, f64> = fx
            .feature_names
            .iter()
            .map(String::as_str)
            .zip(row["features"].as_array().unwrap().iter().map(val))
            .collect();
        for (name, got) in &f.0 {
            let w = want[name];
            if !same(*got, w, 1e-5) {
                let e = bad.entry(name).or_insert((0, String::new()));
                e.0 += 1;
                if e.1.is_empty() {
                    e.1 = format!(
                        "{} × {}: got {got}, want {w}",
                        row["entry_a"], row["entry_b"]
                    );
                }
            }
        }
        let v = verdict(typ, &m.outputs(typ, &f), &fx.thresholds);
        if v != row["verdict"].as_str().unwrap() {
            let diffs: Vec<String> =
                f.0.iter()
                    .filter(|(n, g)| !same(*g, want[n], 0.0))
                    .map(|(n, g)| format!("{n} {g:.9} vs {:.9}", want[n]))
                    .collect();
            flips.push(format!(
                "{} × {} {typ}: {} → {v} [{}]",
                row["entry_a"],
                row["entry_b"],
                row["verdict"],
                diffs.join(", ")
            ));
        }
    }
    for (name, (n, example)) in &bad {
        eprintln!("feature {name}: {n} mismatches, e.g. {example}");
    }
    eprintln!(
        "features (Python vectors): {} rows, {} verdict flips",
        fx.rows.len(),
        flips.len()
    );
    for f in flips.iter().take(10) {
        eprintln!("  {f}");
    }
    assert!(bad.is_empty(), "{} features disagree", bad.len());
    assert!(flips.is_empty(), "{} verdicts differ", flips.len());
}

#[test]
fn encoder_matches_python_vectors() {
    let Some((parity, bundle)) = dirs() else {
        return;
    };
    let fx = load(&parity);
    let mut m = Matcher::open(&bundle).unwrap();
    let step = (fx.texts.len() / 600).max(1);
    let idx: Vec<usize> = (0..fx.texts.len()).step_by(step).collect();
    let texts: Vec<&str> = idx.iter().map(|&i| fx.texts[i].as_str()).collect();
    let got = m.vectors_for(&texts).unwrap();
    let mut worst = 1.0f32;
    for (k, &i) in idx.iter().enumerate() {
        let want = &fx.vectors[i * 256..(i + 1) * 256];
        let cos: f32 = got[k].iter().zip(want).map(|(a, b)| a * b).sum();
        if cos < worst {
            worst = cos;
            eprintln!("  lowest so far {cos:.6}: {:?}", fx.texts[i]);
        }
    }
    eprintln!("encoder: {} texts, min cosine {worst:.6}", idx.len());
    assert!(worst > 0.9999, "min cosine {worst}");
}

#[test]
fn end_to_end_with_rust_encoder() {
    let Some((parity, bundle)) = dirs() else {
        return;
    };
    let fx = load(&parity);
    let mut m = open(&bundle, &fx, false);
    let mut flips = Vec::new();
    let mut worst = 0.0f64;
    for row in &fx.rows {
        let typ = row["type"].as_str().unwrap();
        let f = m
            .features(
                typ,
                pairs_of(&fx, row, "entry_a"),
                pairs_of(&fx, row, "entry_b"),
            )
            .unwrap()
            .unwrap();
        let out = m.outputs(typ, &f);
        let want = expected_outputs(row);
        for k in 0..4 {
            worst = worst.max((out[k] - want[k]).abs());
        }
        let v = verdict(typ, &out, &fx.thresholds);
        if v != row["verdict"].as_str().unwrap() {
            flips.push(format!(
                "{} × {} {typ}: {} → {v} (p_same {:.4} vs {:.4})",
                row["entry_a"], row["entry_b"], row["verdict"], out[0], want[0]
            ));
        }
    }
    eprintln!(
        "end to end: {} rows, {} verdict flips, max |Δp| {worst:.2e}",
        fx.rows.len(),
        flips.len()
    );
    for f in &flips {
        eprintln!("  {f}");
    }
    // Encoder float noise (Rust vs onnxruntime) can tip a pair sitting on a
    // threshold; more than a handful means a real bug. The GPU backend's tanh
    // GELU and f16 kernels drift more (~0.3%, accepted for its speed).
    let per_mille = if m.encoder_on_gpu() { 6 } else { 2 };
    assert!(
        flips.len() * 1000 <= fx.rows.len() * per_mille,
        "{} flips",
        flips.len()
    );
}
