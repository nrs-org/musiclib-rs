//! LightGBM text-model evaluator (`model.txt` as written by `Booster.save_model`).
//!
//! Supports what the learned matcher's models use: numerical splits with the
//! `decision_type` missing-value bits, multiclass (softmax) and binary
//! (sigmoid) objectives. Categorical splits and linear trees are rejected at
//! load time rather than evaluated wrongly.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};

const DEFAULT_LEFT_MASK: i32 = 2;
const CATEGORICAL_MASK: i32 = 1;
/// LightGBM's `kZeroThreshold`.
const ZERO_THRESHOLD: f64 = 1e-35;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Objective {
    Multiclass,
    Binary { sigmoid: f64 },
}

/// One split, packed so a visit touches one cache line instead of one per
/// field array (tree walks are most of the matcher's scoring time).
#[derive(Debug, Clone, Copy)]
struct Node {
    threshold: f64,
    feature: u32,
    /// LightGBM `decision_type`: missing-value kind in bits 2–3, default-left in bit 1.
    decision: i32,
    left: i32,
    right: i32,
}

#[derive(Debug)]
struct Tree {
    nodes: Vec<Node>,
    leaf_value: Vec<f64>,
}

impl Tree {
    fn predict(&self, x: &[f64]) -> f64 {
        if self.nodes.is_empty() {
            return self.leaf_value[0];
        }
        let mut node: i32 = 0;
        while node >= 0 {
            let n = &self.nodes[node as usize];
            let missing = (n.decision >> 2) & 3;
            let mut v = x[n.feature as usize];
            if v.is_nan() && missing != 2 {
                v = 0.0;
            }
            let go_left =
                if (missing == 1 && v.abs() <= ZERO_THRESHOLD) || (missing == 2 && v.is_nan()) {
                    n.decision & DEFAULT_LEFT_MASK != 0
                } else {
                    v <= n.threshold
                };
            node = if go_left { n.left } else { n.right };
        }
        self.leaf_value[!node as usize]
    }
}

/// A loaded LightGBM model.
#[derive(Debug)]
pub struct Gbdt {
    feature_names: Vec<String>,
    num_class: usize,
    objective: Objective,
    trees: Vec<Tree>,
}

fn parse_list<T: std::str::FromStr>(v: &str, what: &str) -> Result<Vec<T>>
where
    T::Err: std::fmt::Display,
{
    v.split_whitespace()
        .map(|s| {
            s.parse::<T>()
                .map_err(|e| anyhow::anyhow!("{what}: {s:?}: {e}"))
        })
        .collect()
}

impl Gbdt {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut header: HashMap<&str, &str> = HashMap::new();
        let mut blocks: Vec<HashMap<&str, &str>> = Vec::new();
        let mut current: Option<HashMap<&str, &str>> = None;
        for line in text.lines() {
            if line.starts_with("Tree=") {
                if let Some(b) = current.take() {
                    blocks.push(b);
                }
                current = Some(HashMap::new());
                continue;
            }
            if line == "end of trees" {
                break;
            }
            if let Some((k, v)) = line.split_once('=') {
                match current.as_mut() {
                    Some(b) => {
                        b.insert(k, v);
                    }
                    None => {
                        header.insert(k, v);
                    }
                }
            }
        }
        if let Some(b) = current.take() {
            blocks.push(b);
        }

        let feature_names: Vec<String> = header
            .get("feature_names")
            .context("missing feature_names")?
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let num_class: usize = header
            .get("num_class")
            .context("missing num_class")?
            .parse()?;
        let objective_line = header.get("objective").context("missing objective")?;
        let objective = match objective_line.split_whitespace().next() {
            Some("multiclass") => Objective::Multiclass,
            Some("binary") => {
                let sigmoid = objective_line
                    .split_whitespace()
                    .find_map(|t| t.strip_prefix("sigmoid:"))
                    .map(str::parse)
                    .transpose()?
                    .unwrap_or(1.0);
                Objective::Binary { sigmoid }
            }
            other => bail!("unsupported objective {other:?}"),
        };
        if header.contains_key("average_output") {
            bail!("average_output (random forest) models are not supported");
        }

        let mut trees = Vec::with_capacity(blocks.len());
        for (i, b) in blocks.iter().enumerate() {
            let get = |k: &str| {
                b.get(k)
                    .copied()
                    .with_context(|| format!("tree {i}: missing {k}"))
            };
            if get("num_cat")?.trim() != "0" {
                bail!("tree {i}: categorical splits are not supported");
            }
            if b.get("is_linear").is_some_and(|v| v.trim() != "0") {
                bail!("tree {i}: linear trees are not supported");
            }
            let num_leaves: usize = get("num_leaves")?.trim().parse()?;
            let leaf_value: Vec<f64> = parse_list(get("leaf_value")?, "leaf_value")?;
            let tree = if num_leaves <= 1 {
                Tree {
                    nodes: vec![],
                    leaf_value,
                }
            } else {
                let decision: Vec<i32> = parse_list(get("decision_type")?, "decision_type")?;
                if decision.iter().any(|d| d & CATEGORICAL_MASK != 0) {
                    bail!("tree {i}: categorical decision");
                }
                let feature: Vec<usize> = parse_list(get("split_feature")?, "split_feature")?;
                let threshold: Vec<f64> = parse_list(get("threshold")?, "threshold")?;
                let left: Vec<i32> = parse_list(get("left_child")?, "left_child")?;
                let right: Vec<i32> = parse_list(get("right_child")?, "right_child")?;
                let n = feature.len();
                if [threshold.len(), decision.len(), left.len(), right.len()] != [n; 4] {
                    bail!("tree {i}: split arrays differ in length");
                }
                if feature.iter().any(|&f| f >= feature_names.len()) {
                    bail!("tree {i}: split feature out of range");
                }
                let in_range = |c: i32| {
                    if c >= 0 {
                        (c as usize) < n
                    } else {
                        ((!c) as usize) < leaf_value.len()
                    }
                };
                if !left.iter().chain(&right).all(|&c| in_range(c)) {
                    bail!("tree {i}: child index out of range");
                }
                Tree {
                    nodes: (0..n)
                        .map(|k| Node {
                            threshold: threshold[k],
                            feature: feature[k] as u32,
                            decision: decision[k],
                            left: left[k],
                            right: right[k],
                        })
                        .collect(),
                    leaf_value,
                }
            };
            trees.push(tree);
        }
        let per_iter = if objective == Objective::Multiclass {
            num_class
        } else {
            1
        };
        if trees.len() % per_iter != 0 {
            bail!("{} trees is not a multiple of {per_iter}", trees.len());
        }
        Ok(Self {
            feature_names,
            num_class,
            objective,
            trees,
        })
    }

    pub fn feature_names(&self) -> &[String] {
        &self.feature_names
    }

    /// Output width: `num_class` for multiclass, 1 for binary.
    pub fn n_outputs(&self) -> usize {
        match self.objective {
            Objective::Multiclass => self.num_class,
            Objective::Binary { .. } => 1,
        }
    }

    /// Probabilities for one row, `x` in `feature_names()` order (NaN = missing).
    pub fn predict(&self, x: &[f64]) -> Vec<f64> {
        assert_eq!(x.len(), self.feature_names.len(), "feature vector width");
        let k = self.n_outputs();
        let mut raw = vec![0.0f64; k];
        for (i, t) in self.trees.iter().enumerate() {
            raw[i % k] += t.predict(x);
        }
        match self.objective {
            Objective::Multiclass => {
                let m = raw.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                let exps: Vec<f64> = raw.iter().map(|r| (r - m).exp()).collect();
                let s: f64 = exps.iter().sum();
                exps.into_iter().map(|e| e / s).collect()
            }
            Objective::Binary { sigmoid } => vec![1.0 / (1.0 + (-sigmoid * raw[0]).exp())],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TINY: &str = "tree\nversion=v4\nnum_class=1\nnum_tree_per_iteration=1\nlabel_index=0\nmax_feature_idx=1\nobjective=binary sigmoid:1\nfeature_names=a b\nfeature_infos=none none\ntree_sizes=1\n\nTree=0\nnum_leaves=3\nnum_cat=0\nsplit_feature=0 1\nsplit_gain=1 1\nthreshold=0.5 1.0000000180025095e-35\ndecision_type=10 2\nleft_child=-1 -2\nright_child=1 -3\nleaf_value=1 2 3\nleaf_weight=1 1 1\nleaf_count=1 1 1\ninternal_value=0 0\ninternal_weight=1 1\ninternal_count=2 1\nis_linear=0\nshrinkage=1\n\n\nend of trees\n";

    fn raw(m: &Gbdt, x: &[f64]) -> f64 {
        let p = m.predict(x)[0];
        (p / (1.0 - p)).ln()
    }

    #[test]
    fn numerical_decisions_and_missing_values() {
        let m = Gbdt::parse(TINY).unwrap();
        assert!((raw(&m, &[0.2, 9.0]) - 1.0).abs() < 1e-12); // a <= 0.5 → leaf 0
        assert!((raw(&m, &[f64::NAN, 9.0]) - 1.0).abs() < 1e-12); // NaN, default left
        assert!((raw(&m, &[0.9, 0.0]) - 2.0).abs() < 1e-12); // b <= ~0 → leaf 1
        assert!((raw(&m, &[0.9, f64::NAN]) - 2.0).abs() < 1e-12); // NaN → 0.0 (no NaN rule)
        assert!((raw(&m, &[0.9, 1.0]) - 3.0).abs() < 1e-12);
    }
}
