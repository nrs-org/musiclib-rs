use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use anyhow::Context as _;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

const MODEL_SCHEMA: &str = "musiclib-dedup-logistic/1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelDecision {
    Merge,
    Separate,
    Defer,
}

#[derive(Debug, Deserialize)]
pub struct DedupModel {
    schema: String,
    features: Vec<String>,
    models_by_type: HashMap<String, TypeModel>,
    merge_threshold: f64,
    separate_threshold: f64,
    #[serde(default)]
    embedding: Option<EmbeddingContract>,
    #[serde(skip)]
    version: String,
}

#[derive(Debug, Deserialize)]
struct EmbeddingContract {
    #[serde(default)]
    required: bool,
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TypeModel {
    intercept: f64,
    coefficients: HashMap<String, f64>,
}

impl DedupModel {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading dedup model {}", path.display()))?;
        let model: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing dedup model {}", path.display()))?;
        let mut model = model;
        model.version = format!("sha256:{:x}", Sha256::digest(&bytes));
        model.validate()?;
        Ok(model)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.schema != MODEL_SCHEMA {
            anyhow::bail!(
                "unsupported dedup model schema {:?}; expected {MODEL_SCHEMA:?}",
                self.schema
            );
        }
        if !(0.0..=1.0).contains(&self.separate_threshold)
            || !(0.0..=1.0).contains(&self.merge_threshold)
            || self.separate_threshold >= self.merge_threshold
        {
            anyhow::bail!("invalid dedup model decision thresholds");
        }
        if self.features.is_empty() || self.models_by_type.is_empty() {
            anyhow::bail!("dedup model has no features or entry-type models");
        }
        let expected: BTreeSet<&str> = self.features.iter().map(String::as_str).collect();
        if expected.len() != self.features.len() {
            anyhow::bail!("dedup model contains duplicate feature names");
        }
        for (entry_type, model) in &self.models_by_type {
            if !model.intercept.is_finite() {
                anyhow::bail!("dedup model for {entry_type:?} has a non-finite intercept");
            }
            let actual: BTreeSet<&str> = model.coefficients.keys().map(String::as_str).collect();
            if actual != expected {
                anyhow::bail!("dedup model for {entry_type:?} has a mismatched feature set");
            }
            if model.coefficients.values().any(|value| !value.is_finite()) {
                anyhow::bail!("dedup model for {entry_type:?} has a non-finite coefficient");
            }
        }
        Ok(())
    }

    /// Score an already-extracted feature row. Missing or non-finite values are
    /// rejected so a metadata/export regression cannot silently become zero.
    pub fn probability(
        &self,
        entry_type: &str,
        values: &HashMap<String, f64>,
    ) -> anyhow::Result<f64> {
        let model = self
            .models_by_type
            .get(entry_type)
            .with_context(|| format!("dedup model has no {entry_type:?} scorer"))?;
        let mut logit = model.intercept;
        for feature in &self.features {
            let value = values
                .get(feature)
                .with_context(|| format!("missing dedup feature {feature:?}"))?;
            if !value.is_finite() {
                anyhow::bail!("dedup feature {feature:?} is non-finite");
            }
            logit += model.coefficients[feature] * value;
        }
        Ok(1.0 / (1.0 + (-logit.clamp(-35.0, 35.0)).exp()))
    }

    pub fn decide(&self, probability: f64) -> ModelDecision {
        if probability >= self.merge_threshold {
            ModelDecision::Merge
        } else if probability <= self.separate_threshold {
            ModelDecision::Separate
        } else {
            ModelDecision::Defer
        }
    }

    pub fn feature_names(&self) -> impl Iterator<Item = &str> {
        self.features.iter().map(String::as_str)
    }

    pub fn required_embedding_model(&self) -> Option<&str> {
        self.embedding
            .as_ref()
            .filter(|contract| contract.required)
            .and_then(|contract| contract.model.as_deref())
    }

    pub fn version(&self) -> &str {
        &self.version
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> DedupModel {
        serde_json::from_str(
            r#"{
                "schema":"musiclib-dedup-logistic/1",
                "features":["name","duration"],
                "models_by_type":{"track":{
                    "intercept":-2.0,
                    "coefficients":{"name":4.0,"duration":1.0}
                }},
                "merge_threshold":0.8,
                "separate_threshold":0.2
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn scores_raw_feature_coefficients_and_applies_defer_band() {
        let model = model();
        model.validate().unwrap();
        let strong = HashMap::from([("name".into(), 1.0), ("duration".into(), 1.0)]);
        let weak = HashMap::from([("name".into(), 0.0), ("duration".into(), 0.0)]);
        let middle = HashMap::from([("name".into(), 0.5), ("duration".into(), 0.0)]);
        assert_eq!(
            model.decide(model.probability("track", &strong).unwrap()),
            ModelDecision::Merge
        );
        assert_eq!(
            model.decide(model.probability("track", &weak).unwrap()),
            ModelDecision::Separate
        );
        assert_eq!(
            model.decide(model.probability("track", &middle).unwrap()),
            ModelDecision::Defer
        );
    }

    #[test]
    fn refuses_silent_missing_features() {
        let model = model();
        let error = model.probability("track", &HashMap::new()).unwrap_err();
        assert!(error.to_string().contains("missing dedup feature"));
    }
}
