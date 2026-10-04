//! Pair facts → views → pair features: a port of `view_from_facts` in
//! `train/learned-matcher/export_parity.py` and `pair_features` in
//! `train/learned-matcher/features.py`. Parity is checked against the
//! exported fixtures (`tests/matcher_parity.rs`).

use std::collections::{BTreeSet, HashMap, HashSet};

use serde::Deserialize;

use super::text::{
    MARKER_NAMES, artist_core, artist_core_raw, bracket_contents, core_title, digits, has_cjk,
    is_generic_artist, is_placeholder, jacc, markers, norm, py_len, tokens, trigrams,
};

pub const VIDEO_SOURCES: [&str; 4] = ["youtube", "nicovideo", "soundcloud", "bilibili"];
pub const DIM: usize = 256;
pub const PAIR_DIMS: usize = 64;

/// One `musiclib-pair-facts/1` record (see docs/plan-v15-runtime.md).
#[derive(Debug, Clone, Deserialize)]
pub struct PairFacts {
    pub source: String,
    pub identifier: String,
    #[serde(default)]
    pub names: Vec<(String, bool)>,
    #[serde(default)]
    pub durations: Vec<f64>,
    pub release_date: Option<String>,
    pub release_type: Option<String>,
    pub primary_type: Option<String>,
    #[serde(default)]
    pub contributions: Vec<Contribution>,
    #[serde(default)]
    pub parents: Vec<Parent>,
    #[serde(default)]
    pub children: Vec<Child>,
    #[serde(default)]
    pub credited: Vec<Option<i64>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Contribution {
    pub artist_entry_id: Option<i64>,
    pub artist_name: Option<String>,
    pub role: Option<String>,
    #[serde(default)]
    pub main: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Parent {
    pub entry_id: Option<i64>,
    pub entry_type: Option<String>,
    pub disc: Option<i64>,
    pub track: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Child {
    pub entry_id: Option<i64>,
    pub entry_type: Option<String>,
    pub name: Option<String>,
}

/// What `pair_features` reads about one side (an entry = a set of pairs).
#[derive(Debug, Default)]
pub struct View {
    pub typ: String,
    pub names: Vec<String>,
    pub texts: Vec<String>,
    pub durations: Vec<f64>,
    pub artists: HashSet<i64>,
    pub artist_names: BTreeSet<String>,
    pub releases: HashSet<i64>,
    pub positions: HashSet<(i64, i64, i64)>,
    pub children: HashSet<i64>,
    pub credits: HashSet<i64>,
    pub groups: HashSet<i64>,
    pub child_titles: Vec<String>,
    pub uploaders: HashSet<i64>,
    pub years: HashSet<i64>,
    pub rtypes: HashSet<String>,
    pub video_only: bool,
}

/// The pair's primary artist: the main `listed_artist`, else the uploader.
pub fn primary_artist_name(f: &PairFacts) -> Option<&str> {
    let role = |c: &Contribution, r: &str| c.role.as_deref() == Some(r);
    if let Some(c) = f
        .contributions
        .iter()
        .find(|c| c.main && role(c, "listed_artist"))
    {
        return c.artist_name.as_deref();
    }
    f.contributions
        .iter()
        .find(|c| role(c, "uploader"))
        .and_then(|c| c.artist_name.as_deref())
}

/// `view_from_facts`: fold an entry's pair facts (in sorted pair order) into a view.
pub fn build_view(facts: &[&PairFacts], typ: &str) -> View {
    let mut v = View {
        typ: typ.to_owned(),
        ..Default::default()
    };
    let mut names: Vec<String> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    let mut named_sources: HashSet<&str> = HashSet::new();
    let mut durations: BTreeSet<u64> = BTreeSet::new();
    for f in facts {
        for d in &f.durations {
            durations.insert(d.to_bits());
        }
        if let Some(rd) = f.release_date.as_deref() {
            let head: Vec<char> = rd.chars().take(4).collect();
            if !head.is_empty() && head.iter().all(|c| c.is_ascii_digit()) {
                v.years
                    .insert(head.iter().collect::<String>().parse().unwrap());
            }
        }
        let rt = f
            .release_type
            .as_deref()
            .filter(|s| !s.is_empty())
            .or(f.primary_type.as_deref().filter(|s| !s.is_empty()));
        if let Some(rt) = rt {
            v.rtypes.insert(rt.to_lowercase());
        }
        let pa_name = if typ == "track" {
            primary_artist_name(f).filter(|s| !s.is_empty())
        } else {
            None
        };
        for (n, _) in &f.names {
            names.push(n.clone());
            texts.push(match pa_name {
                Some(a) => format!("{n} [A] {a}"),
                None => n.clone(),
            });
            named_sources.insert(f.source.as_str());
        }
        for c in &f.contributions {
            let role = c.role.as_deref().unwrap_or("");
            if c.main || matches!(role, "listed_artist" | "uploader" | "vocal") {
                if let Some(e) = c.artist_entry_id {
                    v.artists.insert(e);
                }
                if let Some(n) = c.artist_name.as_deref().filter(|s| !s.is_empty()) {
                    v.artist_names.insert(n.to_owned());
                }
            }
        }
        for c in &f.contributions {
            if c.role.as_deref() == Some("uploader")
                && let Some(e) = c.artist_entry_id
            {
                v.uploaders.insert(e);
            }
        }
        for p in &f.parents {
            let Some(pe) = p.entry_id else { continue };
            if typ == "release" && p.entry_type.as_deref() == Some("release_group") {
                v.groups.insert(pe);
            }
            if p.entry_type.as_deref() == Some("release") {
                v.releases.insert(pe);
                if let Some(tn) = p.track {
                    let dn = p.disc.filter(|&d| d != 0).unwrap_or(1);
                    v.positions.insert((pe, dn, tn));
                }
            }
        }
        for k in &f.children {
            if let Some(ke) = k.entry_id
                && matches!(k.entry_type.as_deref(), Some("track" | "release"))
            {
                v.children.insert(ke);
            }
            if typ == "release"
                && let Some(n) = &k.name
            {
                v.child_titles.push(core_title(n));
            }
        }
        if typ == "artist" {
            v.credits.extend(f.credited.iter().flatten().copied());
        }
    }
    v.durations = durations.into_iter().map(f64::from_bits).collect();
    let mut seen: HashSet<String> = HashSet::new();
    for (n, t) in names.into_iter().zip(texts) {
        if seen.insert(t.clone()) {
            v.names.push(n);
            v.texts.push(t);
        }
    }
    v.names.truncate(40);
    v.texts.truncate(40);
    v.video_only =
        !named_sources.is_empty() && named_sources.iter().all(|s| VIDEO_SOURCES.contains(s));
    v
}

/// Every text `pair_features` will look up in the embedding table.
pub fn view_texts(v: &View) -> impl Iterator<Item = &String> {
    v.texts
        .iter()
        .chain(v.names.iter())
        .chain(v.artist_names.iter())
}

/// Encoder vectors by text (each L2-normalised, `DIM` wide).
pub trait Vectors {
    fn vector(&self, text: &str) -> &[f32];
}

impl Vectors for HashMap<String, Vec<f32>> {
    fn vector(&self, text: &str) -> &[f32] {
        self.get(text)
            .map(Vec::as_slice)
            .unwrap_or_else(|| panic!("no vector for {text:?}"))
    }
}

fn unit64(v: &[f32]) -> Vec<f64> {
    let n = v.iter().map(|&x| x as f64 * x as f64).sum::<f64>().sqrt();
    v.iter().map(|&x| x as f64 / n).collect()
}

/// Cosine matrix rows × cols, as `features.cos`: normalise and multiply in
/// float64, then round to float32. Reproducible across implementations, and
/// identical texts give exactly 1.0. Takes `unit64` vectors.
fn sims(a: &[Vec<f64>], b: &[Vec<f64>]) -> Vec<Vec<f32>> {
    a.iter()
        .map(|u| {
            b.iter()
                .map(|v| u.iter().zip(v).map(|(x, y)| x * y).sum::<f64>() as f32)
                .collect()
        })
        .collect()
}

fn f32_max(xs: impl Iterator<Item = f32>) -> f32 {
    xs.fold(f32::NEG_INFINITY, f32::max)
}

fn f64_mean(xs: &[f32]) -> f64 {
    xs.iter().map(|&x| x as f64).sum::<f64>() / xs.len() as f64
}

/// (max, meanbest, minbest, argmax) of a non-empty similarity matrix.
fn sim_stats(s: &[Vec<f32>]) -> (f32, f64, f32, (usize, usize)) {
    let mut best = (0, 0);
    let mut m = f32::NEG_INFINITY;
    for (i, row) in s.iter().enumerate() {
        for (j, &x) in row.iter().enumerate() {
            if x > m {
                m = x;
                best = (i, j);
            }
        }
    }
    let row_max: Vec<f32> = s.iter().map(|r| f32_max(r.iter().copied())).collect();
    let col_max: Vec<f32> = (0..s[0].len())
        .map(|j| f32_max(s.iter().map(|r| r[j])))
        .collect();
    let meanbest = (f64_mean(&row_max) + f64_mean(&col_max)) / 2.0;
    let f32_min = |xs: &[f32]| xs.iter().copied().fold(f32::INFINITY, f32::min);
    let minbest = f32_min(&row_max).min(f32_min(&col_max));
    (m, meanbest, minbest, best)
}

fn opt_max(xs: impl Iterator<Item = f64>) -> f64 {
    let mut out = f64::NAN;
    for x in xs {
        if out.is_nan() || x > out {
            out = x;
        }
    }
    out
}

fn b(x: bool) -> f64 {
    if x { 1.0 } else { 0.0 }
}

/// Ordered `(name, value)` features, NaN = missing (Python `np.nan`).
pub struct Features(pub Vec<(&'static str, f64)>);

impl Features {
    pub fn get(&self, name: &str) -> Option<f64> {
        self.0.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
    }
}

const D_NAMES: [&str; PAIR_DIMS] = {
    const N: [&str; 64] = [
        "d0", "d1", "d2", "d3", "d4", "d5", "d6", "d7", "d8", "d9", "d10", "d11", "d12", "d13",
        "d14", "d15", "d16", "d17", "d18", "d19", "d20", "d21", "d22", "d23", "d24", "d25", "d26",
        "d27", "d28", "d29", "d30", "d31", "d32", "d33", "d34", "d35", "d36", "d37", "d38", "d39",
        "d40", "d41", "d42", "d43", "d44", "d45", "d46", "d47", "d48", "d49", "d50", "d51", "d52",
        "d53", "d54", "d55", "d56", "d57", "d58", "d59", "d60", "d61", "d62", "d63",
    ];
    N
};
const P_NAMES: [&str; PAIR_DIMS] = [
    "p0", "p1", "p2", "p3", "p4", "p5", "p6", "p7", "p8", "p9", "p10", "p11", "p12", "p13", "p14",
    "p15", "p16", "p17", "p18", "p19", "p20", "p21", "p22", "p23", "p24", "p25", "p26", "p27",
    "p28", "p29", "p30", "p31", "p32", "p33", "p34", "p35", "p36", "p37", "p38", "p39", "p40",
    "p41", "p42", "p43", "p44", "p45", "p46", "p47", "p48", "p49", "p50", "p51", "p52", "p53",
    "p54", "p55", "p56", "p57", "p58", "p59", "p60", "p61", "p62", "p63",
];
const M_XOR: [&str; 17] = [
    "m_xor_live",
    "m_xor_remix",
    "m_xor_instrumental",
    "m_xor_acoustic",
    "m_xor_cover",
    "m_xor_medley",
    "m_xor_short",
    "m_xor_mv",
    "m_xor_arrange",
    "m_xor_edit",
    "m_xor_remaster",
    "m_xor_ver",
    "m_xor_acappella",
    "m_xor_full",
    "m_xor_demo",
    "m_xor_stem",
    "m_xor_performance",
];
const M_BOTH: [&str; 17] = [
    "m_both_live",
    "m_both_remix",
    "m_both_instrumental",
    "m_both_acoustic",
    "m_both_cover",
    "m_both_medley",
    "m_both_short",
    "m_both_mv",
    "m_both_arrange",
    "m_both_edit",
    "m_both_remaster",
    "m_both_ver",
    "m_both_acappella",
    "m_both_full",
    "m_both_demo",
    "m_both_stem",
    "m_both_performance",
];
const S_M: [&str; 17] = [
    "s_m_live",
    "s_m_remix",
    "s_m_instrumental",
    "s_m_acoustic",
    "s_m_cover",
    "s_m_medley",
    "s_m_short",
    "s_m_mv",
    "s_m_arrange",
    "s_m_edit",
    "s_m_remaster",
    "s_m_ver",
    "s_m_acappella",
    "s_m_full",
    "s_m_demo",
    "s_m_stem",
    "s_m_performance",
];

/// The half of `pair_features` that depends on one side only, computed once
/// per entry rather than once per pair (the matcher caches it per view).
pub struct Side {
    /// `unit64` encoder vectors of the view's texts, names and artist names.
    texts_u: Vec<Vec<f64>>,
    names_u: Vec<Vec<f64>>,
    artists_u: Vec<Vec<f64>>,
    /// The first `PAIR_DIMS` raw encoder values of each text.
    texts_head: Vec<Vec<f32>>,
    /// Digits and Python length of `norm(name)`, per name.
    names_digits: Vec<HashSet<String>>,
    names_len: Vec<usize>,
    /// Distinct normalised names, with each one's trigrams and tokens.
    norm_set: HashSet<String>,
    norm_tri: Vec<HashSet<String>>,
    norm_tok: Vec<HashSet<String>>,
    cores: HashSet<String>,
    core_tri: Vec<HashSet<String>>,
    brackets: HashSet<String>,
    /// Artist views only: artist cores (with trigrams) and the generic flag.
    artist_cores: HashSet<String>,
    artist_core_tri: Vec<HashSet<String>>,
    generic: bool,
    credit_cores: HashSet<String>,
    placeholder: bool,
    markers: HashSet<usize>,
    named_versions: HashSet<String>,
    cjk: f64,
}

impl Side {
    pub fn new<V: Vectors>(v: &View, vecs: &V) -> Self {
        let units = |ts: &mut dyn Iterator<Item = &String>| -> Vec<Vec<f64>> {
            ts.map(|t| unit64(vecs.vector(t))).collect()
        };
        let names_norm: Vec<String> = v.names.iter().map(|x| norm(x)).collect();
        let norm_set: HashSet<String> = names_norm.iter().cloned().collect();
        let norm_order: Vec<&String> = norm_set.iter().collect();
        let cores: HashSet<String> = v
            .names
            .iter()
            .map(|x| core_title(x))
            .filter(|x| !x.is_empty())
            .collect();
        let core_tri = cores.iter().map(|x| trigrams(x)).collect();
        let (artist_cores, generic) = if v.typ == "artist" {
            let ac: HashSet<String> = v
                .names
                .iter()
                .map(|x| artist_core(x))
                .filter(|x| !x.is_empty())
                .collect();
            let generic = v
                .names
                .iter()
                .any(|x| is_generic_artist(&norm(&artist_core_raw(x))));
            (ac, generic)
        } else {
            (HashSet::new(), false)
        };
        let artist_core_tri = artist_cores.iter().map(|x| trigrams(x)).collect();
        let (markers, named_versions) = markers(&v.names);
        Side {
            texts_u: units(&mut v.texts.iter()),
            names_u: if v.typ == "track" && !v.texts.is_empty() {
                units(&mut v.names.iter())
            } else {
                Vec::new()
            },
            artists_u: units(&mut v.artist_names.iter()),
            texts_head: v
                .texts
                .iter()
                .map(|t| vecs.vector(t)[..PAIR_DIMS].to_vec())
                .collect(),
            names_digits: names_norm.iter().map(|x| digits(x)).collect(),
            names_len: names_norm.iter().map(|x| py_len(x)).collect(),
            norm_tri: norm_order.iter().map(|x| trigrams(x)).collect(),
            norm_tok: norm_order.iter().map(|x| tokens(x)).collect(),
            norm_set,
            cores,
            core_tri,
            brackets: v.names.iter().flat_map(|x| bracket_contents(x)).collect(),
            artist_cores,
            artist_core_tri,
            generic,
            credit_cores: v.artist_names.iter().map(|x| artist_core(x)).collect(),
            placeholder: v.names.iter().any(|x| is_placeholder(&norm(x))),
            markers,
            named_versions,
            cjk: v.names.iter().filter(|x| has_cjk(x)).count() as f64 / v.names.len().max(1) as f64,
        }
    }
}

/// `pair_features(a, b, cache)`.
pub fn pair_features<V: Vectors>(a: &View, b: &View, vecs: &V) -> Features {
    pair_features_sides(a, &Side::new(a, vecs), b, &Side::new(b, vecs))
}

/// `pair_features` from precomputed sides (`Side::new` of each view).
pub fn pair_features_sides(a: &View, sa: &Side, b_: &View, sb: &Side) -> Features {
    let mut f: Vec<(&'static str, f64)> = Vec::with_capacity(250);
    let na = &a.names;
    let nb = &b_.names;
    // Indices of the best-matching names (None: no names on that side).
    let best: (Option<usize>, Option<usize>);
    if !a.texts.is_empty() && !b_.texts.is_empty() {
        let s = sims(&sa.texts_u, &sb.texts_u);
        let (m, meanbest, minbest, (i, j)) = sim_stats(&s);
        f.push(("enc_max", m as f64));
        f.push(("enc_meanbest", meanbest));
        f.push(("enc_minbest", minbest as f64));
        let u = &sa.texts_head[i];
        let v = &sb.texts_head[j];
        best = (Some(i), Some(j));
        if a.typ == "track" {
            let t = sims(&sa.names_u, &sb.names_u);
            let (tm, tmean, _, _) = sim_stats(&t);
            f.push(("enc_title_max", tm as f64));
            f.push(("enc_title_meanbest", tmean));
        } else {
            f.push(("enc_title_max", m as f64));
            f.push(("enc_title_meanbest", meanbest));
        }
        for k in 0..PAIR_DIMS {
            f.push((D_NAMES[k], (u[k] - v[k]).abs() as f64));
            f.push((P_NAMES[k], (u[k] * v[k]) as f64));
        }
    } else {
        for n in [
            "enc_max",
            "enc_meanbest",
            "enc_minbest",
            "enc_title_max",
            "enc_title_meanbest",
        ] {
            f.push((n, f64::NAN));
        }
        for k in 0..PAIR_DIMS {
            f.push((D_NAMES[k], f64::NAN));
            f.push((P_NAMES[k], f64::NAN));
        }
        best = ((!na.is_empty()).then_some(0), (!nb.is_empty()).then_some(0));
    }

    if !a.artist_names.is_empty() && !b_.artist_names.is_empty() {
        let s = sims(&sa.artists_u, &sb.artists_u);
        f.push((
            "artist_enc_max",
            f32_max(s.iter().flatten().copied()) as f64,
        ));
    } else {
        f.push(("artist_enc_max", f64::NAN));
    }

    // lexical
    f.push((
        "lex_exact",
        b(sa.norm_set.intersection(&sb.norm_set).next().is_some()),
    ));
    f.push((
        "lex_tri_max",
        opt_max(
            sa.norm_tri
                .iter()
                .flat_map(|x| sb.norm_tri.iter().map(move |y| jacc(x, y))),
        ),
    ));
    f.push((
        "lex_tok_max",
        opt_max(
            sa.norm_tok
                .iter()
                .flat_map(|x| sb.norm_tok.iter().map(move |y| jacc(x, y))),
        ),
    ));
    // Length and digits of the best names, normalised (norm("") when a side
    // has no names).
    let pick = |s: &Side, i: Option<usize>| -> (usize, HashSet<String>) {
        match i {
            Some(i) => (s.names_len[i], s.names_digits[i].clone()),
            None => {
                let empty = norm("");
                (py_len(&empty), digits(&empty))
            }
        }
    };
    let ((la, da), (lb, db)) = (pick(sa, best.0), pick(sb, best.1));
    f.push(("len_ratio", la.min(lb) as f64 / la.max(lb).max(1) as f64));
    f.push((
        "num_conflict",
        b(!da.is_empty() && !db.is_empty() && da != db),
    ));
    f.push(("num_xor", b(da.is_empty() != db.is_empty())));
    let (ca, cb) = (sa.cjk, sb.cjk);
    f.push(("cjk_min", ca.min(cb)));
    f.push(("cjk_absdiff", (ca - cb).abs()));

    // core titles and bracket contents
    f.push((
        "core_exact",
        b(sa.cores.intersection(&sb.cores).next().is_some()),
    ));
    f.push((
        "core_tri_max",
        opt_max(
            sa.core_tri
                .iter()
                .flat_map(|x| sb.core_tri.iter().map(move |y| jacc(x, y))),
        ),
    ));
    let (pa_, pb_) = (&sa.brackets, &sb.brackets);
    f.push((
        "bracket_jacc",
        if !pa_.is_empty() && !pb_.is_empty() {
            jacc(pa_, pb_)
        } else {
            f64::NAN
        },
    ));
    f.push(("bracket_xor", b(pa_.is_empty() != pb_.is_empty())));
    if a.typ == "artist" {
        f.push((
            "artist_core_exact",
            b(sa.artist_cores
                .intersection(&sb.artist_cores)
                .next()
                .is_some()),
        ));
        f.push((
            "artist_core_tri_max",
            opt_max(
                sa.artist_core_tri
                    .iter()
                    .flat_map(|x| sb.artist_core_tri.iter().map(move |y| jacc(x, y))),
            ),
        ));
        f.push(("generic_name", b(sa.generic || sb.generic)));
    } else {
        f.push(("artist_core_exact", f64::NAN));
        f.push(("artist_core_tri_max", f64::NAN));
        f.push(("generic_name", f64::NAN));
    }
    if !a.artist_names.is_empty() && !b_.artist_names.is_empty() {
        f.push((
            "credit_core_overlap",
            b(sa.credit_cores
                .intersection(&sb.credit_cores)
                .next()
                .is_some()),
        ));
    } else {
        f.push(("credit_core_overlap", f64::NAN));
    }
    f.push(("placeholder", b(sa.placeholder || sb.placeholder)));

    // version markers
    let (ma, va) = (&sa.markers, &sa.named_versions);
    let (mb, vb) = (&sb.markers, &sb.named_versions);
    for k in 0..MARKER_NAMES.len() {
        f.push((M_XOR[k], b(ma.contains(&k) != mb.contains(&k))));
        f.push((M_BOTH[k], b(ma.contains(&k) && mb.contains(&k))));
    }
    f.push(("m_xor_count", ma.symmetric_difference(mb).count() as f64));
    for (k, name) in S_M.iter().enumerate() {
        f.push((name, b(ma.contains(&k)) - b(mb.contains(&k))));
    }
    f.push(("s_named_ver", b(!va.is_empty()) - b(!vb.is_empty())));
    f.push(("s_video", b(a.video_only) - b(b_.video_only)));
    let max_d = |v: &View| {
        v.durations
            .iter()
            .cloned()
            .fold(f64::NEG_INFINITY, f64::max)
    };
    let min_d = |v: &View| v.durations.iter().cloned().fold(f64::INFINITY, f64::min);
    let both_dur = !a.durations.is_empty() && !b_.durations.is_empty();
    f.push((
        "s_dur",
        if both_dur {
            (max_d(a) - max_d(b_)) / 1000.0
        } else {
            f64::NAN
        },
    ));
    f.push(("s_len", la as f64 - lb as f64));
    f.push(("s_names", na.len() as f64 - nb.len() as f64));
    f.push((
        "s_artists",
        a.artists.len() as f64 - b_.artists.len() as f64,
    ));
    f.push((
        "named_ver_conflict",
        b(!va.is_empty() && !vb.is_empty() && va.is_disjoint(vb)),
    ));
    f.push(("named_ver_xor", b(va.is_empty() != vb.is_empty())));

    // duration
    if both_dur {
        let d = a
            .durations
            .iter()
            .flat_map(|x| b_.durations.iter().map(move |y| (x - y).abs()))
            .fold(f64::INFINITY, f64::min);
        f.push(("dur_known", 1.0));
        f.push(("dur_delta_s", d / 1000.0));
        f.push(("dur_rel", d / max_d(a).max(max_d(b_)).max(1.0)));
        f.push((
            "dur_max_delta_s",
            (max_d(a).max(max_d(b_)) - min_d(a).min(min_d(b_))) / 1000.0,
        ));
    } else {
        f.push(("dur_known", 0.0));
        f.push(("dur_delta_s", f64::NAN));
        f.push(("dur_rel", f64::NAN));
        f.push(("dur_max_delta_s", f64::NAN));
    }
    // originality (not shipped: no MB year facts at runtime)
    f.push(("orig_gap_min", f64::NAN));
    f.push(("orig_gap_max", f64::NAN));
    f.push(("orig_known", 0.0));
    f.push(("s_orig_gap", f64::NAN));
    f.push(("video_sides", b(a.video_only) + b(b_.video_only)));
    if both_dur && a.video_only != b_.video_only {
        let (vid, aud) = if a.video_only { (a, b_) } else { (b_, a) };
        f.push(("mv_minus_audio_s", (max_d(vid) - max_d(aud)) / 1000.0));
    } else {
        f.push(("mv_minus_audio_s", f64::NAN));
    }

    // credits / structure
    f.push((
        "artists_known",
        b(!a.artists.is_empty() && !b_.artists.is_empty()),
    ));
    f.push(("artist_jacc", jacc(&a.artists, &b_.artists)));
    f.push(("artist_shared", b(!a.artists.is_disjoint(&b_.artists))));
    f.push(("same_release", b(!a.releases.is_disjoint(&b_.releases))));
    let same_pos = !a.positions.is_disjoint(&b_.positions);
    f.push(("same_position", b(same_pos)));
    let ra: HashSet<i64> = a.positions.iter().map(|p| p.0).collect();
    let rb: HashSet<i64> = b_.positions.iter().map(|p| p.0).collect();
    f.push(("position_conflict", b(!ra.is_disjoint(&rb) && !same_pos)));
    let groups_known = !a.groups.is_empty() && !b_.groups.is_empty();
    f.push(("groups_known", b(groups_known)));
    let same_group = !a.groups.is_disjoint(&b_.groups);
    f.push(("same_group", b(same_group)));
    f.push(("group_conflict", b(groups_known && !same_group)));
    f.push(("same_uploader", b(!a.uploaders.is_disjoint(&b_.uploaders))));
    if !a.child_titles.is_empty() && !b_.child_titles.is_empty() {
        let ta_: HashSet<&String> = a.child_titles.iter().filter(|t| !t.is_empty()).collect();
        let tb_: HashSet<&String> = b_.child_titles.iter().filter(|t| !t.is_empty()).collect();
        let (la, lb) = (ta_.len(), tb_.len());
        f.push(("tracklist_title_jacc", jacc(&ta_, &tb_)));
        f.push((
            "tracklist_title_cover",
            ta_.intersection(&tb_).count() as f64 / la.min(lb).max(1) as f64,
        ));
        f.push((
            "tracklist_len_ratio",
            la.min(lb) as f64 / la.max(lb).max(1) as f64,
        ));
        f.push(("tracklist_extra", (la as f64 - lb as f64).abs()));
    } else {
        for n in [
            "tracklist_title_jacc",
            "tracklist_title_cover",
            "tracklist_len_ratio",
            "tracklist_extra",
        ] {
            f.push((n, f64::NAN));
        }
    }
    f.push((
        "children_known",
        b(!a.children.is_empty() && !b_.children.is_empty()),
    ));
    f.push(("children_jacc", jacc(&a.children, &b_.children)));
    f.push((
        "children_ratio",
        if !a.children.is_empty() && !b_.children.is_empty() {
            a.children.len().min(b_.children.len()) as f64
                / a.children.len().max(b_.children.len()) as f64
        } else {
            f64::NAN
        },
    ));
    f.push(("credits_jacc", jacc(&a.credits, &b_.credits)));
    f.push((
        "credits_shared_log",
        (a.credits.intersection(&b_.credits).count() as f64).ln_1p(),
    ));
    f.push((
        "years_known",
        b(!a.years.is_empty() && !b_.years.is_empty()),
    ));
    f.push((
        "year_delta",
        a.years
            .iter()
            .flat_map(|x| b_.years.iter().map(move |y| (x - y).abs()))
            .min()
            .map_or(f64::NAN, |d| d as f64),
    ));
    f.push((
        "rtype_equal",
        if !a.rtypes.is_empty() && !b_.rtypes.is_empty() {
            b(!a.rtypes.is_disjoint(&b_.rtypes))
        } else {
            f64::NAN
        },
    ));
    let type_id = ["track", "artist", "release", "release_group"]
        .iter()
        .position(|t| *t == a.typ)
        .map_or(f64::NAN, |i| i as f64);
    f.push(("type_id", type_id));
    Features(f)
}
