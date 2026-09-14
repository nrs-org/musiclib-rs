//! Candle port of `sentence-transformers/LaBSE`.encode(normalize_embeddings=True):
//! BERT encoder -> CLS-token pooling -> Dense(768->768) + Tanh -> L2 normalize.
//! CPU only. `--features mkl` swaps candle's default GEMM for Intel MKL.
//!
//! Modes:
//!   check <reference.json>          cosine-similarity parity vs a Python reference
//!   bench <texts.json> [batch_size] throughput over the full PoC corpus

use std::time::Instant;

use anyhow::{Context, Result};
use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::{Linear, Module, VarBuilder};
use candle_transformers::models::bert::{BertModel, Config};
use serde::Deserialize;
use tokenizers::Tokenizer;

// candle-core's MKL backend references `hgemm_` (half-precision GEMM); the
// intel-mkl-src 2020.1 build doesn't export it. All our tensors are F32, so
// the f16 path is unreachable -- stub satisfies the linker. See
// config/inference/src/lib.rs for the identical fix.
#[cfg(feature = "mkl")]
#[unsafe(no_mangle)]
pub extern "C" fn hgemm_() {
    unreachable!("f16 GEMM is never invoked: all inference tensors are F32");
}

const REPO: &str = "sentence-transformers/LaBSE";
const MAX_SEQ_LEN: usize = 256; // sentence_bert_config.json

struct LaBSE {
    bert: BertModel,
    dense: Linear,
    tokenizer: Tokenizer,
    device: Device,
}

impl LaBSE {
    fn load() -> Result<Self> {
        let device = Device::Cpu;
        let api = hf_hub::api::sync::Api::new()?;
        let repo = api.model(REPO.to_string());

        let config_path = repo.get("config.json").context("fetch config.json")?;
        let tokenizer_path = repo.get("tokenizer.json").context("fetch tokenizer.json")?;
        let dense_weights = repo
            .get("2_Dense/model.safetensors")
            .context("fetch 2_Dense/model.safetensors")?;

        let config: Config =
            serde_json::from_str(&std::fs::read_to_string(config_path)?).context("parse config")?;

        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(anyhow::Error::msg)
            .context("load tokenizer")?;
        let pad_id = tokenizer.token_to_id("[PAD]").unwrap_or(0);
        tokenizer
            .with_padding(Some(tokenizers::PaddingParams {
                strategy: tokenizers::PaddingStrategy::BatchLongest,
                pad_id,
                pad_token: "[PAD]".to_string(),
                ..Default::default()
            }))
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MAX_SEQ_LEN,
                ..Default::default()
            }))
            .map_err(anyhow::Error::msg)?;

        let bert_weights = repo
            .get("model.safetensors")
            .context("fetch model.safetensors")?;
        let vb = unsafe { VarBuilder::from_mmaped_safetensors(&[bert_weights], DType::F32, &device)? };
        let bert = BertModel::load(vb, &config).context("load BERT")?;

        let vb_dense =
            unsafe { VarBuilder::from_mmaped_safetensors(&[dense_weights], DType::F32, &device)? };
        let weight = vb_dense.get((768, 768), "linear.weight")?;
        let bias = vb_dense.get(768, "linear.bias")?;
        let dense = Linear::new(weight, Some(bias));

        Ok(Self {
            bert,
            dense,
            tokenizer,
            device,
        })
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
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
            .bert
            .forward(&input_ids, &token_type_ids, Some(&attention_mask))?;

        // CLS-token pooling (1_Pooling: pooling_mode_cls_token=true). MKL's
        // matmul kernel requires a contiguous lhs; the slice view isn't.
        let cls = hidden.i((.., 0, ..))?.contiguous()?; // [B, H]

        // 2_Dense: Linear(768,768) + Tanh.
        let dense_out = self.dense.forward(&cls)?.tanh()?;

        // 3_Normalize: L2.
        let norm = dense_out
            .sqr()?
            .sum_keepdim(1)?
            .sqrt()?
            .clamp(1e-12, f64::INFINITY)?;
        let normalized = dense_out.broadcast_div(&norm)?;

        Ok(normalized.to_vec2::<f32>()?)
    }
}

#[derive(Deserialize)]
struct TextRow {
    view_id: String,
    text: String,
}

#[derive(Deserialize)]
struct RefRow {
    view_id: String,
    text: String,
    vector: Vec<f32>,
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("bench");
    let backend = if cfg!(feature = "mkl") { "mkl" } else { "pure-candle" };

    eprintln!("loading LaBSE ({backend} backend)...");
    let t_load = Instant::now();
    let model = LaBSE::load()?;
    eprintln!("model load: {:.2?}", t_load.elapsed());

    match mode {
        "check" => {
            let path = args.get(2).context("usage: check <reference.json>")?;
            let rows: Vec<RefRow> = serde_json::from_str(&std::fs::read_to_string(path)?)?;
            let texts: Vec<String> = rows.iter().map(|r| r.text.clone()).collect();
            let got = model.embed_batch(&texts)?;
            let mut min_cos = f32::INFINITY;
            for (row, vec) in rows.iter().zip(got.iter()) {
                let c = cosine(&row.vector, vec);
                min_cos = min_cos.min(c);
                println!("{:<40} cos={c:.6}", row.view_id);
            }
            println!("min cosine similarity: {min_cos:.6}");
        }
        "bench" => {
            let path = args.get(2).context("usage: bench <texts.json> [batch_size]")?;
            let batch_size: usize = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(32);
            let rows: Vec<TextRow> = serde_json::from_str(&std::fs::read_to_string(path)?)?;
            let texts: Vec<String> = rows.iter().map(|r| r.text.clone()).collect();
            eprintln!(
                "encoding {} texts, batch_size={batch_size}, backend={backend}",
                texts.len()
            );

            let t0 = Instant::now();
            let mut n_done = 0usize;
            for chunk in texts.chunks(batch_size) {
                let _ = model.embed_batch(chunk)?;
                n_done += chunk.len();
                if n_done % (batch_size * 20) == 0 {
                    eprintln!("  {n_done}/{}", texts.len());
                }
            }
            let dt = t0.elapsed();
            println!("=== labse-bench ({backend}) ===");
            println!("texts        : {}", texts.len());
            println!("batch_size   : {batch_size}");
            println!("model load   : {:.2?}", t_load.elapsed());
            println!("encode total : {dt:.2?}");
            println!(
                "per text     : {:.2}ms",
                dt.as_secs_f64() * 1000.0 / texts.len() as f64
            );
        }
        other => anyhow::bail!("unknown mode {other:?}, expected check|bench"),
    }

    Ok(())
}
