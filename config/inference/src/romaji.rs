//! Port of `detect_romaji_tokens` from embedding_server.py — bit-faithful,
//! including the dict-insertion-order argmax tie-break and the exact wordfreq
//! zipf contribution (bundled English table).

use std::collections::HashMap;

use fancy_regex::Regex as FancyRegex;

use crate::model::{Weights, predict_batch};

/// Insertion-order-stable argmax: returns `(index, value)` of the first maximum.
fn argmax_prob(probs: &[f32]) -> (usize, f32) {
    let mut bi = 0usize;
    let mut bv = probs[0];
    for (i, &p) in probs.iter().enumerate() {
        if p > bv {
            bv = p;
            bi = i;
        }
    }
    (bi, bv)
}

const NGRAM_N: usize = 3;
const CJK_BONUS: f32 = 0.8;
const EN_PENALTY_SCALE: f32 = 0.5;
const SYLLABLE_WEIGHT: f32 = 3.0;

// _MORA_RE verbatim (fancy-regex supports (?i), lookahead and the \1 backref).
const MORA_PATTERN: &str = r"(?i)ou|oo|uu|ii|aa|ee|sh[auo]|sh[ei]|ch[auoe]|chi|ts[uaoe]|tsi|ky[auo]|ny[auo]|hy[auo]|my[auo]|ry[auo]|gy[auo]|by[auo]|py[auo]|zy[auo]|jy[auo]|[kgsztdnhbpmyrwfjv][aeiou]|[aeiou]|n(?=[^aeiouny]|$)|([ptksmbgdz])\1";

/// Codepoints matched by the Python `_CJK_RE`: kana (U+3040–U+30FF),
/// CJK unified (U+4E00–U+9FFF), halfwidth/fullwidth forms (U+FF00–U+FFEF).
fn is_cjk_char(c: char) -> bool {
    let u = c as u32;
    (0x3040..=0x30FF).contains(&u)
        || (0x4E00..=0x9FFF).contains(&u)
        || (0xFF00..=0xFFEF).contains(&u)
}
fn has_cjk(s: &str) -> bool {
    s.chars().any(is_cjk_char)
}

/// Python `_tokenize`: `re.findall(r"[^\s\-–—·•・/|]+", text)`.
fn tokenize(text: &str) -> Vec<String> {
    let is_sep = |c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '-' | '\u{2013}' | '\u{2014}' | '\u{00B7}' | '\u{2022}' | '\u{30FB}' | '/' | '|'
            )
    };
    text.split(is_sep)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

pub struct Romaji {
    mora: FancyRegex,
    zipf: HashMap<String, f32>,
}

impl Romaji {
    pub fn new(zipf_tsv: &str) -> Self {
        let mut zipf = HashMap::new();
        for line in zipf_tsv.lines() {
            if let Some((w, z)) = line.split_once('\t') {
                zipf.insert(w.to_string(), z.parse().unwrap());
            }
        }
        Romaji {
            mora: FancyRegex::new(MORA_PATTERN).unwrap(),
            zipf,
        }
    }

    fn zipf_frequency(&self, word_lower: &str) -> f32 {
        self.zipf.get(word_lower).copied().unwrap_or(0.0)
    }

    /// Python `_coverage`: fraction of the lowercased token covered by mora matches.
    fn coverage(&self, token: &str) -> f32 {
        let t = token.to_lowercase();
        let total = t.chars().count();
        if total == 0 {
            return 0.0;
        }
        let mut covered = 0usize;
        let mut pos = 0usize;
        // Non-overlapping leftmost matches, like Python's finditer.
        while pos <= t.len() {
            match self.mora.find_from_pos(&t, pos).unwrap() {
                Some(m) => {
                    covered += t[m.start()..m.end()].chars().count();
                    pos = if m.end() > m.start() {
                        m.end()
                    } else {
                        m.end() + 1
                    };
                }
                None => break,
            }
        }
        covered as f32 / total as f32
    }

    /// Returns the tokens classified as romanised Japanese (ja-Latn).
    pub fn detect(&self, text: &str, w: &Weights) -> Vec<String> {
        let ja_latn = w.labels.iter().position(|l| l == "ja-Latn").unwrap();
        let en = w.labels.iter().position(|l| l == "en").unwrap();

        let tokens = tokenize(text);
        let n = tokens.len();
        let cjk = has_cjk(text);

        // votes[i]: insertion-ordered (label_idx -> accumulated prob)
        let mut votes: Vec<Vec<(usize, f32)>> = vec![Vec::new(); n];
        let add = |v: &mut Vec<(usize, f32)>, lang: usize, prob: f32| {
            if let Some(e) = v.iter_mut().find(|(l, _)| *l == lang) {
                e.1 += prob;
            } else {
                v.push((lang, prob));
            }
        };

        // Collect every LangID query (unigram tokens, then n-gram windows in
        // the same nested order) and run them through one batched forward pass
        // so the int8 GEMMs are shared across the batch.
        let mut ngram_spans: Vec<(usize, usize)> = Vec::new();
        let mut joined: Vec<String> = Vec::new();
        for start in 0..n {
            let hi = (start + NGRAM_N).min(n);
            for end in (start + 2)..=hi {
                joined.push(tokens[start..end].join(" "));
                ngram_spans.push((start, end));
            }
        }
        let mut queries: Vec<&str> = Vec::with_capacity(n + joined.len());
        queries.extend(tokens.iter().map(|s| s.as_str()));
        queries.extend(joined.iter().map(|s| s.as_str()));
        let probs = predict_batch(&queries, w);

        for i in 0..n {
            let (lang, prob) = argmax_prob(&probs[i]);
            add(&mut votes[i], lang, prob);
        }
        for (k, &(start, end)) in ngram_spans.iter().enumerate() {
            let (lang, prob) = argmax_prob(&probs[n + k]);
            for i in start..end {
                add(&mut votes[i], lang, prob);
            }
        }

        // adjusted votes
        let mut adjusted = votes;
        for (i, tok) in tokens.iter().enumerate() {
            let z = self.zipf_frequency(&tok.to_lowercase());
            add(&mut adjusted[i], en, z * EN_PENALTY_SCALE);
            let tok_has_cjk = has_cjk(tok);
            if !tok_has_cjk {
                add(
                    &mut adjusted[i],
                    ja_latn,
                    self.coverage(tok) * SYLLABLE_WEIGHT,
                );
            }
            if cjk && !tok_has_cjk {
                add(&mut adjusted[i], ja_latn, CJK_BONUS);
            }
        }

        // first-pass predictions (insertion-ordered argmax)
        let argmax = |v: &Vec<(usize, f32)>| -> usize {
            let mut bi = v[0].0;
            let mut bv = v[0].1;
            for &(l, p) in v.iter().skip(1) {
                if p > bv {
                    bv = p;
                    bi = l;
                }
            }
            bi
        };
        let preds: Vec<usize> = adjusted.iter().map(argmax).collect();

        // neighbor smoothing
        for i in 0..n {
            if has_cjk(&tokens[i]) {
                continue;
            }
            let left = if i > 0 { Some(preds[i - 1]) } else { None };
            let right = if i + 1 < n { Some(preds[i + 1]) } else { None };
            let cnt = [left, right]
                .iter()
                .flatten()
                .filter(|&&p| p == ja_latn)
                .count();
            if cnt == 2 {
                add(&mut adjusted[i], ja_latn, SYLLABLE_WEIGHT * 0.5);
            } else if cnt == 1 {
                add(&mut adjusted[i], ja_latn, SYLLABLE_WEIGHT * 0.2);
            }
        }

        tokens
            .into_iter()
            .zip(adjusted.iter())
            .filter(|(_, v)| argmax(v) == ja_latn)
            .map(|(t, _)| t)
            .collect()
    }
}
