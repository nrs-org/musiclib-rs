//! Model2Vec static-embedding path — a transformer-free distillation of
//! `paraphrase-multilingual-MiniLM-L12-v2`.
//!
//! Inference is exactly the model2vec reference algorithm and needs no neural
//! network at runtime:
//!
//!   1. tokenize with the bundled Unigram tokenizer (`add_special_tokens=false`),
//!   2. drop `[UNK]` token ids,
//!   3. gather the matching rows of the static `[vocab × dim]` embedding table,
//!   4. mean-pool them (empty token list → zero vector),
//!   5. L2-normalize if the model's `config.json` sets `normalize: true`.
//!
//! The table is a single F16 `embeddings` tensor; we upcast to f32 once at load.
//! This produces *different* (256-d, lossy, word-order-insensitive) vectors than
//! the real MiniLM — this is the default backend, trading quality for speed.

use anyhow::{Context, Result};
use tokenizers::Tokenizer;

/// HF repo for the distilled static model (256-d PCA of MiniLM-L12-v2).
const REPO: &str = "Jarbas/m2v-256-paraphrase-multilingual-MiniLM-L12-v2";

/// Decode one IEEE-754 binary16 value to f32. The embedding table ships as F16
/// to halve its on-disk size, and this is the only place we ever touch f16 — not
/// worth a dependency. Branch-light conversion after Fabian Giesen, correct for
/// normals, subnormals, zero, inf and NaN; the end-to-end model2vec parity test
/// guards it.
#[inline]
fn f16_to_f32(h: u16) -> f32 {
    let h = h as u32;
    let shifted_exp = 0x7c00u32 << 13; // F16 exponent mask, shifted into F32 position
    let mut o = (h & 0x7fff) << 13; // exponent + mantissa
    let exp = shifted_exp & o;
    o += (127 - 15) << 23; // rebias the exponent
    if exp == shifted_exp {
        o += (128 - 16) << 23; // inf/NaN: saturate the F32 exponent
    } else if exp == 0 {
        o += 1 << 23; // subnormal: renormalize…
        o = (f32::from_bits(o) - f32::from_bits(113 << 23)).to_bits(); // …via the magic number
    }
    o |= (h & 0x8000) << 16; // sign
    f32::from_bits(o)
}

pub struct StaticEmbedder {
    tokenizer: Tokenizer,
    /// Row-major `[vocab * dim]` table, upcast from the stored F16.
    embeddings: Vec<f32>,
    dim: usize,
    /// Token ids equal to this are skipped (matches model2vec's unk filtering).
    unk_id: Option<u32>,
    normalize: bool,
}

impl StaticEmbedder {
    /// Download (cached) and load the static model. Shares the HF cache with the
    /// MiniLM/torch stack under `~/.cache/huggingface`.
    pub fn load() -> Result<Self> {
        let api = hf_hub::api::sync::Api::new()?;
        let repo = api.model(REPO.to_string());
        let tok_path = repo.get("tokenizer.json").context("fetch tokenizer.json")?;
        let st_path = repo
            .get("model.safetensors")
            .context("fetch model.safetensors")?;
        let cfg_path = repo.get("config.json").context("fetch config.json")?;

        let mut tokenizer = Tokenizer::from_file(&tok_path).map_err(anyhow::Error::msg)?;
        // model2vec encodes raw strings with no truncation/padding.
        let _ = tokenizer.with_truncation(None);
        let unk_id = tokenizer.token_to_id("[UNK]");

        let cfg: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&cfg_path).context("read config.json")?)?;
        let normalize = cfg
            .get("normalize")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Read the F16 `embeddings` tensor and upcast to f32.
        let bytes = std::fs::read(&st_path).context("read model.safetensors")?;
        let st = safetensors::SafeTensors::deserialize(&bytes)
            .map_err(|e| anyhow::anyhow!("parse safetensors: {e}"))?;
        let t = st
            .tensor("embeddings")
            .map_err(|e| anyhow::anyhow!("get embeddings: {e}"))?;
        let shape = t.shape();
        anyhow::ensure!(shape.len() == 2, "expected 2-D embeddings, got {shape:?}");
        let dim = shape[1];
        let raw = t.data();
        anyhow::ensure!(
            raw.len() == shape[0] * dim * 2,
            "embeddings size/shape mismatch"
        );
        let mut embeddings = vec![0f32; shape[0] * dim];
        for (dst, chunk) in embeddings.iter_mut().zip(raw.chunks_exact(2)) {
            *dst = f16_to_f32(u16::from_le_bytes([chunk[0], chunk[1]]));
        }

        Ok(Self {
            tokenizer,
            embeddings,
            dim,
            unk_id,
            normalize,
        })
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = self.embed_batch(std::slice::from_ref(&text.to_string()))?;
        Ok(v.pop().unwrap())
    }

    pub fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            let enc = self
                .tokenizer
                .encode(text.as_str(), false)
                .map_err(anyhow::Error::msg)?;
            let mut acc = vec![0f32; self.dim];
            let mut count = 0usize;
            for &id in enc.get_ids() {
                if Some(id) == self.unk_id {
                    continue;
                }
                let off = id as usize * self.dim;
                let row = &self.embeddings[off..off + self.dim];
                for (a, &r) in acc.iter_mut().zip(row) {
                    *a += r;
                }
                count += 1;
            }
            if count > 0 {
                let inv = 1.0 / count as f32;
                for a in &mut acc {
                    *a *= inv;
                }
            }
            if self.normalize {
                let norm = acc.iter().map(|x| x * x).sum::<f32>().sqrt() + 1e-32;
                for a in &mut acc {
                    *a /= norm;
                }
            }
            out.push(acc);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::f16_to_f32;

    #[test]
    fn f16_decode_known_values() {
        let cases: &[(u16, f32)] = &[
            (0x0000, 0.0),
            (0x8000, -0.0),
            (0x3c00, 1.0),
            (0xbc00, -1.0),
            (0x4000, 2.0),
            (0xc000, -2.0),
            (0x3555, 0.333_251_95),   // nearest-f16 to 1/3
            (0x7bff, 65504.0),        // largest finite f16
            (0x0400, 6.103_515_6e-5), // smallest normal
            (0x0001, 5.960_464_5e-8), // smallest subnormal (2^-24)
            (0x03ff, 6.097_555_2e-5), // largest subnormal
        ];
        for &(bits, want) in cases {
            let got = f16_to_f32(bits);
            assert!(
                (got - want).abs() <= want.abs() * 1e-6 + 1e-12,
                "f16 {bits:#06x}: got {got}, want {want}"
            );
            assert_eq!(
                got.is_sign_negative(),
                want.is_sign_negative(),
                "sign for {bits:#06x}"
            );
        }
        assert!(f16_to_f32(0x7c00).is_infinite() && f16_to_f32(0x7c00) > 0.0); // +inf
        assert!(f16_to_f32(0xfc00).is_infinite() && f16_to_f32(0xfc00) < 0.0); // -inf
        assert!(f16_to_f32(0x7e00).is_nan()); // NaN
    }
}
