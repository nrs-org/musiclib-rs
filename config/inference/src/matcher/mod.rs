//! Learned pair matcher (v17 + rel-v8, docs/plan-v15-runtime.md): pair facts
//! in, class probabilities and relation-head outputs out. The verdict policy
//! (thresholds, DEFER band) stays in the calling Rhai script.
//!
//! A bundle directory (written by `train/learned-matcher/bundle.py`) holds
//! `model.txt`, `kind.txt`, `direction.txt`, `structure.txt`, `kinds.json` and
//! `encoder/{config.json,tokenizer.json,model.safetensors}`.
//!
//! Callers send each library pair's facts once (`put_facts`); an entry's view
//! is the union of its pairs' facts. Views and their per-side feature work
//! (`features::Side`) are cached per (type, sorted pairs), encoder vectors per
//! text. `prepare` builds many entries' sides at once, encoding all their new
//! texts in one batch, and hands back handles for `score_views`.

pub mod encoder;
pub mod features;
pub mod gbdt;
#[cfg(feature = "vulkan")]
pub mod gpu;
pub mod text;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use encoder::TitleEncoder;
use features::{
    Features, PairFacts, Side, View, build_view, pair_features, pair_features_sides, view_texts,
};
use gbdt::Gbdt;

pub type Pair = (String, String);

/// `outputs_heads` mask bits: the main classifier (`p_same` … `p_unrelated`)
/// plus `guard`, and the relation heads (`p_sibling_structure`,
/// `p_a_derived`, `p_kind_*`).
pub const HEAD_MAIN: u32 = 1;
pub const HEAD_RELATION: u32 = 2;

/// Fixed part of the output row; `p_kind_<kind>` columns follow.
pub const BASE_OUTPUTS: [&str; 7] = [
    "p_same",
    "p_related",
    "p_sibling",
    "p_unrelated",
    "guard",
    "p_sibling_structure",
    "p_a_derived",
];

/// Model inputs as positions into the `Features` row.
struct Head {
    model: Gbdt,
    /// (feature position, take absolute value) per model column.
    columns: Vec<(usize, bool)>,
    /// Model columns holding a signed (`s_`) feature: negated to swap sides.
    signed: Vec<usize>,
}

impl Head {
    fn new(model: Gbdt, positions: &HashMap<&'static str, usize>) -> Result<Self> {
        let mut columns = Vec::new();
        let mut signed = Vec::new();
        for (i, name) in model.feature_names().iter().enumerate() {
            let (base, abs) = match name.strip_prefix("abs_") {
                Some(b) => (b, true),
                None => (name.as_str(), false),
            };
            let pos = *positions.get(base).with_context(|| {
                format!("model feature {name:?} is not computed by this runtime")
            })?;
            columns.push((pos, abs));
            if !abs && base.starts_with("s_") {
                signed.push(i);
            }
        }
        Ok(Self {
            model,
            columns,
            signed,
        })
    }

    fn row(&self, f: &Features, flip: bool) -> Vec<f64> {
        let mut x: Vec<f64> = self
            .columns
            .iter()
            .map(|&(p, abs)| if abs { f.0[p].1.abs() } else { f.0[p].1 })
            .collect();
        if flip {
            for &i in &self.signed {
                x[i] = -x[i];
            }
        }
        x
    }
}

pub struct Matcher {
    main: Head,
    kind: Head,
    direction: Head,
    structure: Head,
    kinds: Vec<String>,
    output_names: Vec<String>,
    encoder: TitleEncoder,
    encoder_id: String,
    facts: HashMap<Pair, PairFacts>,
    /// Prepared entries; a handle is an index here.
    prepared: Vec<(Arc<View>, Side)>,
    prepared_by_key: HashMap<String, usize>,
    vectors: HashMap<String, Vec<f32>>,
}

/// `Vectors` over an empty table: for computing the feature layout only.
struct NoVectors;
impl features::Vectors for NoVectors {
    fn vector(&self, text: &str) -> &[f32] {
        unreachable!("layout views have no texts ({text:?})")
    }
}

fn feature_positions() -> HashMap<&'static str, usize> {
    let empty = View {
        typ: "track".into(),
        ..Default::default()
    };
    pair_features(&empty, &empty, &NoVectors)
        .0
        .iter()
        .enumerate()
        .map(|(i, (n, _))| (*n, i))
        .collect()
}

/// Feature names in `Features` row order.
pub fn feature_layout() -> Vec<&'static str> {
    let mut v: Vec<(&'static str, usize)> = feature_positions().into_iter().collect();
    v.sort_by_key(|&(_, i)| i);
    v.into_iter().map(|(n, _)| n).collect()
}

fn read_encoder_id(dir: &Path) -> Result<String> {
    let path = dir.join("manifest.json");
    if !path.exists() {
        return Ok(String::new());
    }
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).context("reading manifest.json")?)?;
    Ok(manifest["encoder"].as_str().unwrap_or_default().to_owned())
}

pub enum Score {
    Outputs(Vec<f64>),
    /// Facts are missing for these pairs; `put_facts` them and retry.
    Missing(Vec<Pair>),
}

impl Matcher {
    pub fn open(dir: &Path) -> Result<Self> {
        let positions = feature_positions();
        let head = |f: &str| -> Result<Head> { Head::new(Gbdt::load(&dir.join(f))?, &positions) };
        let kinds: Vec<String> = serde_json::from_str(
            &std::fs::read_to_string(dir.join("kinds.json")).context("reading kinds.json")?,
        )?;
        let main = head("model.txt")?;
        if main.model.n_outputs() != 4 {
            bail!(
                "model.txt: expected 4 classes, got {}",
                main.model.n_outputs()
            );
        }
        let kind = head("kind.txt")?;
        if kind.model.n_outputs() != kinds.len() {
            bail!(
                "kind.txt has {} classes but kinds.json lists {}",
                kind.model.n_outputs(),
                kinds.len()
            );
        }
        let output_names = BASE_OUTPUTS
            .iter()
            .map(|s| s.to_string())
            .chain(kinds.iter().map(|k| format!("p_kind_{k}")))
            .collect();
        Ok(Self {
            main,
            kind,
            direction: head("direction.txt")?,
            structure: head("structure.txt")?,
            kinds,
            output_names,
            encoder: TitleEncoder::load(&dir.join("encoder"))?,
            encoder_id: read_encoder_id(dir)?,
            facts: HashMap::new(),
            prepared: Vec::new(),
            prepared_by_key: HashMap::new(),
            vectors: HashMap::new(),
        })
    }

    pub fn output_names(&self) -> &[String] {
        &self.output_names
    }

    pub fn kinds(&self) -> &[String] {
        &self.kinds
    }

    /// The encoder's identity from `manifest.json` ("" when the bundle has no
    /// manifest): what an embedding cache keys its vectors on.
    pub fn encoder_id(&self) -> &str {
        &self.encoder_id
    }

    /// Semantic-blocking text for one entry. Tracks get `title [A] artist`, the
    /// input the encoder was trained on, with the primary artist of the pair
    /// that carries `title` (else of the first pair that has one); other types
    /// get the bare title. `Err` lists pairs whose facts are missing.
    pub fn entry_text(
        &self,
        typ: &str,
        title: &str,
        pairs: &[Pair],
    ) -> std::result::Result<String, Vec<Pair>> {
        if typ != "track" {
            return Ok(title.to_owned());
        }
        let missing = self.missing(pairs);
        if !missing.is_empty() {
            return Err(missing);
        }
        let mut sorted: Vec<&Pair> = pairs.iter().collect();
        sorted.sort();
        sorted.dedup();
        let facts: Vec<&PairFacts> = sorted.iter().map(|p| &self.facts[*p]).collect();
        let artist = facts
            .iter()
            .filter(|f| f.names.iter().any(|(n, _)| n == title))
            .chain(facts.iter())
            .find_map(|f| features::primary_artist_name(f).filter(|s| !s.is_empty()));
        Ok(match artist {
            Some(a) => format!("{title} [A] {a}"),
            None => title.to_owned(),
        })
    }

    pub fn put_facts(&mut self, f: PairFacts) {
        self.facts
            .insert((f.source.clone(), f.identifier.clone()), f);
    }

    pub fn missing(&self, pairs: &[Pair]) -> Vec<Pair> {
        pairs
            .iter()
            .filter(|p| !self.facts.contains_key(*p))
            .cloned()
            .collect()
    }

    /// Test hook: preload encoder vectors (e.g. the Python reference's).
    pub fn insert_vector(&mut self, text: String, v: Vec<f32>) {
        self.vectors.insert(text, v);
    }

    fn view_key<'p>(typ: &str, pairs: &'p [Pair]) -> (String, Vec<&'p Pair>) {
        let mut sorted: Vec<&Pair> = pairs.iter().collect();
        sorted.sort();
        sorted.dedup();
        let mut key = String::from(typ);
        for (s, i) in &sorted {
            key.push('\u{1f}');
            key.push_str(s);
            key.push('\u{1e}');
            key.push_str(i);
        }
        (key, sorted)
    }

    /// Handles for `(type, pairs)` entries, building and caching each one's
    /// view and feature side; the texts of all new views are encoded in one
    /// batch. `Err` lists pairs whose facts are missing.
    pub fn prepare(
        &mut self,
        entries: &[(&str, &[Pair])],
    ) -> Result<std::result::Result<Vec<usize>, Vec<Pair>>> {
        let missing: Vec<Pair> = entries
            .iter()
            .flat_map(|(_, pairs)| self.missing(pairs))
            .collect();
        if !missing.is_empty() {
            return Ok(Err(missing));
        }
        let mut handles = Vec::with_capacity(entries.len());
        let mut fresh: Vec<(String, Arc<View>)> = Vec::new();
        let mut fresh_by_key: HashMap<String, usize> = HashMap::new();
        for (typ, pairs) in entries {
            let (key, sorted) = Self::view_key(typ, pairs);
            if let Some(&h) = self.prepared_by_key.get(&key) {
                handles.push(h);
                continue;
            }
            let next = self.prepared.len() + fresh.len();
            let h = *fresh_by_key.entry(key.clone()).or_insert_with(|| {
                let facts: Vec<&PairFacts> = sorted.iter().map(|p| &self.facts[*p]).collect();
                fresh.push((key, Arc::new(build_view(&facts, typ))));
                next
            });
            handles.push(h);
        }
        let texts: Vec<String> = fresh
            .iter()
            .flat_map(|(_, v)| view_texts(v).cloned())
            .collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        self.embed(&refs)?;
        for (key, view) in fresh {
            let side = Side::new(&view, &self.vectors);
            self.prepared_by_key.insert(key, self.prepared.len());
            self.prepared.push((view, side));
        }
        Ok(Ok(handles))
    }

    pub fn embed(&mut self, texts: &[&str]) -> Result<()> {
        let mut todo: Vec<&str> = texts
            .iter()
            .copied()
            .filter(|t| !self.vectors.contains_key(*t))
            .collect();
        todo.sort_unstable();
        todo.dedup();
        if todo.is_empty() {
            return Ok(());
        }
        for (t, v) in todo.iter().zip(self.encoder.embed(&todo)?) {
            self.vectors.insert((*t).to_owned(), v);
        }
        Ok(())
    }

    /// Whether the encoder runs on the GPU backend (see `encoder`).
    pub fn encoder_on_gpu(&self) -> bool {
        self.encoder.on_gpu()
    }

    /// Encoder vectors for arbitrary texts (semantic blocking), cached.
    pub fn vectors_for(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.embed(texts)?;
        Ok(texts.iter().map(|t| self.vectors[*t].clone()).collect())
    }

    /// Features for one pair (both sides of `typ`).
    pub fn features(
        &mut self,
        typ: &str,
        a: &[Pair],
        b: &[Pair],
    ) -> Result<std::result::Result<Features, Vec<Pair>>> {
        Ok(self
            .prepare(&[(typ, a), (typ, b)])?
            .map(|h| self.features_of(h[0], h[1])))
    }

    fn features_of(&self, a: usize, b: usize) -> Features {
        let ((va, sa), (vb, sb)) = (&self.prepared[a], &self.prepared[b]);
        pair_features_sides(va, sa, vb, sb)
    }

    pub fn score(&mut self, typ: &str, a: &[Pair], b: &[Pair]) -> Result<Score> {
        let f = match self.features(typ, a, b)? {
            Ok(f) => f,
            Err(missing) => return Ok(Score::Missing(missing)),
        };
        Ok(Score::Outputs(self.outputs(typ, &f)))
    }

    /// Output row (`heads`, see `outputs_heads`) for two `prepare` handles of
    /// the same entry type.
    pub fn score_views(&self, a: usize, b: usize, heads: u32) -> Result<Vec<f64>> {
        let (Some((va, _)), Some((vb, _))) = (self.prepared.get(a), self.prepared.get(b)) else {
            bail!(
                "unknown view handle {a} or {b} ({} prepared)",
                self.prepared.len()
            );
        };
        if va.typ != vb.typ {
            bail!("views of different types ({} vs {})", va.typ, vb.typ);
        }
        Ok(self.outputs_heads(&va.typ, &self.features_of(a, b), heads))
    }

    /// Output row (see `output_names`) from computed features.
    pub fn outputs(&self, typ: &str, f: &Features) -> Vec<f64> {
        self.outputs_heads(typ, f, HEAD_MAIN | HEAD_RELATION)
    }

    /// `outputs` computing only the `heads` asked for; the other columns are
    /// NaN. The relation heads are ~80% of the tree walks and a policy only
    /// reads them for pairs the main head calls related.
    pub fn outputs_heads(&self, typ: &str, f: &Features, heads: u32) -> Vec<f64> {
        let mut out = vec![f64::NAN; self.output_names.len()];
        if heads & HEAD_MAIN != 0 {
            out[..4].copy_from_slice(&self.main.model.predict(&self.main.row(f, false)));
            let flag = |n: &str| f.get(n).is_some_and(|v| v > 0.0);
            out[4] = if flag("placeholder") || flag("generic_name") {
                1.0
            } else {
                0.0
            };
        }
        if heads & HEAD_RELATION != 0 {
            if typ == "track" {
                let p1 = self.structure.model.predict(&self.structure.row(f, false));
                let p2 = self.structure.model.predict(&self.structure.row(f, true));
                out[5] = (p1[2] + p2[2]) / 2.0;
            }
            let d1 = self.direction.model.predict(&self.direction.row(f, false))[0];
            let d2 = self.direction.model.predict(&self.direction.row(f, true))[0];
            out[6] = (d1 + 1.0 - d2) / 2.0;
            out[BASE_OUTPUTS.len()..]
                .copy_from_slice(&self.kind.model.predict(&self.kind.row(f, false)));
        }
        out
    }
}
