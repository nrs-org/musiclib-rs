//! MiniLM sentence embeddings, CPU-only, via candle.
//!
//! Reproduces `SentenceTransformer("paraphrase-multilingual-MiniLM-L12-v2")
//! .encode(text, normalize_embeddings=True)`:
//!   tokenize (XLM-R SentencePiece) → BERT encoder → mean-pool over the
//!   attention mask → L2 normalize. No CUDA / no hardware acceleration: the
//!   device is always `Device::Cpu`.
//!
//! The model weights/tokenizer are pulled from the HuggingFace hub on first use
//! and cached under the standard `~/.cache/huggingface` directory (mirroring the
//! Python server, which downloaded its model on demand).

use anyhow::{Context, Result};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use tokenizers::Tokenizer;

const REPO: &str = "sentence-transformers/paraphrase-multilingual-MiniLM-L12-v2";
/// sentence-transformers caps this model at 128 tokens (sentence_bert_config.json).
const MAX_SEQ_LEN: usize = 128;

pub struct Embedder {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

impl Embedder {
    pub fn load() -> Result<Self> {
        let device = Device::Cpu;
        let api = hf_hub::api::sync::Api::new()?;
        let repo = api.model(REPO.to_string());

        let config_path = repo.get("config.json").context("fetch config.json")?;
        let tokenizer_path = repo.get("tokenizer.json").context("fetch tokenizer.json")?;

        let config: Config =
            serde_json::from_str(&std::fs::read_to_string(config_path)?).context("parse config")?;

        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(anyhow::Error::msg)
            .context("load tokenizer")?;
        // Pad to the longest sequence in each batch; truncate at the model cap.
        let pad_id = tokenizer.token_to_id("<pad>").unwrap_or(1);
        tokenizer
            .with_padding(Some(tokenizers::PaddingParams {
                strategy: tokenizers::PaddingStrategy::BatchLongest,
                pad_id,
                pad_token: "<pad>".to_string(),
                ..Default::default()
            }))
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MAX_SEQ_LEN,
                ..Default::default()
            }))
            .map_err(anyhow::Error::msg)?;

        // Prefer safetensors; fall back to the PyTorch checkpoint.
        let vb = if let Ok(safetensors) = repo.get("model.safetensors") {
            unsafe { VarBuilder::from_mmaped_safetensors(&[safetensors], DType::F32, &device)? }
        } else {
            let pth = repo
                .get("pytorch_model.bin")
                .context("fetch pytorch_model.bin")?;
            VarBuilder::from_pth(&pth, DType::F32, &device)?
        };
        let model = BertModel::load(vb, &config).context("load BERT")?;

        Ok(Self {
            model,
            tokenizer,
            device,
        })
    }

    /// Embed a batch of texts; each row is L2-normalized (384-d).
    pub fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(anyhow::Error::msg)?;

        let batch = encodings.len();
        let seq_len = encodings[0].get_ids().len();

        let mut ids = Vec::with_capacity(batch * seq_len);
        let mut mask = Vec::with_capacity(batch * seq_len);
        let mut type_ids = Vec::with_capacity(batch * seq_len);
        for enc in &encodings {
            ids.extend(enc.get_ids().iter().copied());
            mask.extend(enc.get_attention_mask().iter().copied());
            type_ids.extend(enc.get_type_ids().iter().copied());
        }

        let input_ids = Tensor::from_vec(ids, (batch, seq_len), &self.device)?;
        let token_type_ids = Tensor::from_vec(type_ids, (batch, seq_len), &self.device)?;
        let attention_mask = Tensor::from_vec(mask, (batch, seq_len), &self.device)?;

        // [B, L, H]
        let hidden = self
            .model
            .forward(&input_ids, &token_type_ids, Some(&attention_mask))?;

        // Mean pooling weighted by the attention mask (matches the ST Pooling layer).
        let mask_f = attention_mask.to_dtype(DType::F32)?; // [B, L]
        let mask_exp = mask_f.unsqueeze(2)?; // [B, L, 1]
        let summed = hidden.broadcast_mul(&mask_exp)?.sum(1)?; // [B, H]
        let counts = mask_f.sum(1)?.clamp(1e-9, f64::INFINITY)?.unsqueeze(1)?; // [B, 1]
        let mean = summed.broadcast_div(&counts)?; // [B, H]

        // L2 normalize.
        let norm = mean
            .sqr()?
            .sum_keepdim(1)?
            .sqrt()?
            .clamp(1e-12, f64::INFINITY)?;
        let normalized = mean.broadcast_div(&norm)?;

        Ok(normalized.to_vec2::<f32>()?)
    }

    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        Ok(self.embed_batch(&[text.to_string()])?.pop().unwrap())
    }
}
