//! Same LaBSE benchmark, but via `tch` (Rust bindings to libtorch itself)
//! instead of reimplementing the forward pass in candle. This calls the same
//! oneDNN-fused CPU kernels Python torch dispatches to -- the traced
//! TorchScript module (labse_traced.pt) already bakes in CLS-pool + Dense +
//! Tanh + Normalize, so no post-processing is needed here beyond tokenizing.

use std::time::Instant;

use anyhow::{Context, Result};
use serde::Deserialize;
use tch::{CModule, Kind, Tensor};
use tokenizers::Tokenizer;

const MAX_SEQ_LEN: usize = 256;

#[derive(Deserialize)]
struct TextRow {
    text: String,
}

#[derive(Deserialize)]
struct RefRow {
    text: String,
    vector: Vec<f32>,
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb)
}

fn load_tokenizer() -> Result<Tokenizer> {
    let api = hf_hub::api::sync::Api::new()?;
    let repo = api.model("sentence-transformers/LaBSE".to_string());
    let tokenizer_path = repo.get("tokenizer.json").context("fetch tokenizer.json")?;
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
    Ok(tokenizer)
}

fn embed_batch(module: &CModule, tokenizer: &Tokenizer, texts: &[String]) -> Result<Vec<Vec<f32>>> {
    let encodings = tokenizer
        .encode_batch(texts.to_vec(), true)
        .map_err(anyhow::Error::msg)?;
    let batch = encodings.len();
    let seq_len = encodings[0].get_ids().len();

    let mut ids = Vec::with_capacity(batch * seq_len);
    let mut mask = Vec::with_capacity(batch * seq_len);
    let mut type_ids = Vec::with_capacity(batch * seq_len);
    for enc in &encodings {
        ids.extend(enc.get_ids().iter().map(|&x| x as i64));
        mask.extend(enc.get_attention_mask().iter().map(|&x| x as i64));
        type_ids.extend(enc.get_type_ids().iter().map(|&x| x as i64));
    }

    let shape = [batch as i64, seq_len as i64];
    let input_ids = Tensor::from_slice(&ids).reshape(shape).to_kind(Kind::Int64);
    let attention_mask = Tensor::from_slice(&mask).reshape(shape).to_kind(Kind::Int64);
    let token_type_ids = Tensor::from_slice(&type_ids).reshape(shape).to_kind(Kind::Int64);

    let out = module.forward_ts(&[input_ids, attention_mask, token_type_ids])?;
    let dim = out.size()[1] as usize;
    let flat: Vec<f32> = out.reshape([-1]).try_into()?;
    Ok(flat.chunks(dim).map(|c| c.to_vec()).collect())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("bench");

    eprintln!("libtorch intra-op threads: {}", tch::utils::get_num_threads());

    let module_path = "/tmp/claude-1000/-home-ayaneso-dev-nrs-org-musiclib-rs/9df73696-1f73-4e1e-8645-917d5e3cee9e/scratchpad/labse_traced_v26.pt";
    let t_load = Instant::now();
    let module = CModule::load(module_path).context("load traced module")?;
    let tokenizer = load_tokenizer()?;
    eprintln!("model load: {:.2?}", t_load.elapsed());

    match mode {
        "check" => {
            let path = args.get(2).context("usage: check <reference.json>")?;
            let rows: Vec<RefRow> = serde_json::from_str(&std::fs::read_to_string(path)?)?;
            let texts: Vec<String> = rows.iter().map(|r| r.text.clone()).collect();
            let got = embed_batch(&module, &tokenizer, &texts)?;
            let mut min_cos = f32::INFINITY;
            for (row, vec) in rows.iter().zip(got.iter()) {
                let c = cosine(&row.vector, vec);
                min_cos = min_cos.min(c);
                println!("cos={c:.6}");
            }
            println!("min cosine similarity: {min_cos:.6}");
        }
        "bench" => {
            let path = args.get(2).context("usage: bench <texts.json> [batch_size]")?;
            let batch_size: usize = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(32);
            let rows: Vec<TextRow> = serde_json::from_str(&std::fs::read_to_string(path)?)?;
            let mut texts: Vec<String> = rows.iter().map(|r| r.text.clone()).collect();
            let length_sort = args.get(4).map(String::as_str) == Some("sorted");
            if length_sort {
                texts.sort_by_key(|t| std::cmp::Reverse(t.chars().count()));
            }
            eprintln!(
                "encoding {} texts, batch_size={batch_size}, backend=tch/libtorch, length_sort={length_sort}",
                texts.len()
            );

            let t0 = Instant::now();
            let mut n_done = 0usize;
            for chunk in texts.chunks(batch_size) {
                let _ = embed_batch(&module, &tokenizer, chunk)?;
                n_done += chunk.len();
                if n_done % (batch_size * 20) == 0 {
                    eprintln!("  {n_done}/{}", texts.len());
                }
            }
            let dt = t0.elapsed();
            println!("=== labse-bench (tch/libtorch) ===");
            println!("texts        : {}", texts.len());
            println!("batch_size   : {batch_size}");
            println!("encode total : {dt:.2?}");
            println!("per text     : {:.2}ms", dt.as_secs_f64() * 1000.0 / texts.len() as f64);
        }
        other => anyhow::bail!("unknown mode {other:?}, expected check|bench"),
    }
    Ok(())
}
