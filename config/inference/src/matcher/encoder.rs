//! Title encoder for the learned matcher: `gbnam8/jp-music-title-encoder`
//! (v7-L6, a 6-layer BERT).
//!
//! Matches the Python reference (`features.Encoder`): the bundle's
//! `tokenizer.json` (truncation 48, special tokens added) → BERT → mean pool
//! over the attention mask → first 256 dims → L2 normalise.
//!
//! Two backends: llama.cpp on a Vulkan GPU (feature `vulkan`, see `gpu.rs`)
//! when the bundle has `encoder/model-f16.gguf` and a discrete GPU is present,
//! else the CPU forward below. The CPU path matches the Python reference to
//! cosine ≥ 0.999999 and flips no parity verdict; the GPU path is ~12× faster
//! but its tanh GELU and f16 kernels drift to cosine ~0.99999, which tips
//! ~0.3% of parity verdicts (the trees split on raw vector components) —
//! accepted for the speed. All vectors in a run come from one backend.
//! `MUSICLIB_ENCODER=cpu` forces the exact CPU path.
//!
//! The CPU forward pass is hand-written rather than candle's `BertModel`: candle
//! runs every element-wise op (bias add, GELU, softmax) on one thread and
//! allocates per op, which was ~75% of the wall time. Here the matmuls use the
//! same `gemm` kernels candle does, the element-wise work is fused and spread
//! over rayon, and texts are packed without padding (each attends only to its
//! own tokens, which is what the padding mask did). The arithmetic mirrors
//! candle's (`libm::erff` GELU, f32 layer-norm sums), so vectors match it to
//! float rounding.

use std::path::Path;

use anyhow::{Context, Result, bail};
use candle_core::Device;
use rayon::prelude::*;
use serde::Deserialize;
use tokenizers::Tokenizer;

use super::features::DIM;

/// Texts per CPU forward pass. Texts are packed without padding, so this only
/// sets the matmul height: large enough to keep every core busy.
const BATCH: usize = 256;

#[derive(Deserialize)]
struct Config {
    hidden_size: usize,
    intermediate_size: usize,
    num_attention_heads: usize,
    num_hidden_layers: usize,
    layer_norm_eps: f64,
    hidden_act: String,
}

/// A dense layer: `weight` is `[out, in]` row-major, as stored.
struct Linear {
    weight: Vec<f32>,
    bias: Vec<f32>,
    n_in: usize,
    n_out: usize,
}

struct LayerNorm {
    weight: Vec<f32>,
    bias: Vec<f32>,
}

struct Layer {
    query: Linear,
    key: Linear,
    value: Linear,
    attn_out: Linear,
    attn_norm: LayerNorm,
    intermediate: Linear,
    output: Linear,
    out_norm: LayerNorm,
}

pub struct TitleEncoder {
    tokenizer: Tokenizer,
    cpu: Bert,
    #[cfg(feature = "vulkan")]
    gpu: Option<super::gpu::GpuEncoder>,
}

/// The CPU forward pass's weights.
struct Bert {
    hidden: usize,
    heads: usize,
    eps: f32,
    word: Vec<f32>,
    position: Vec<f32>,
    /// Token type 0's row (the only type a single-segment input uses).
    token_type: Vec<f32>,
    emb_norm: LayerNorm,
    layers: Vec<Layer>,
    threads: usize,
}

struct Weights(std::collections::HashMap<String, candle_core::Tensor>);

impl Weights {
    fn get(&self, name: &str, shape: &[usize]) -> Result<Vec<f32>> {
        let t = self
            .0
            .get(name)
            .with_context(|| format!("encoder weight {name} missing"))?;
        if t.dims() != shape {
            bail!(
                "encoder weight {name}: shape {:?}, expected {shape:?}",
                t.dims()
            );
        }
        Ok(t.flatten_all()?
            .to_dtype(candle_core::DType::F32)?
            .to_vec1()?)
    }

    fn linear(&self, prefix: &str, n_in: usize, n_out: usize) -> Result<Linear> {
        Ok(Linear {
            weight: self.get(&format!("{prefix}.weight"), &[n_out, n_in])?,
            bias: self.get(&format!("{prefix}.bias"), &[n_out])?,
            n_in,
            n_out,
        })
    }

    fn norm(&self, prefix: &str, n: usize) -> Result<LayerNorm> {
        Ok(LayerNorm {
            weight: self.get(&format!("{prefix}.weight"), &[n])?,
            bias: self.get(&format!("{prefix}.bias"), &[n])?,
        })
    }
}

impl Linear {
    /// `x · weightᵀ` for `rows` rows (no bias: callers fuse it into what follows).
    fn matmul(&self, x: &[f32], rows: usize, threads: usize) -> Vec<f32> {
        debug_assert_eq!(x.len(), rows * self.n_in);
        let mut out = vec![0f32; rows * self.n_out];
        if rows == 0 {
            return out;
        }
        // SAFETY: the strides describe `out` [rows, n_out], `x` [rows, n_in]
        // and `weight` read as its transpose [n_in, n_out], all row-major and
        // in bounds; `out` doesn't alias the inputs.
        unsafe {
            gemm::gemm(
                rows,
                self.n_out,
                self.n_in,
                out.as_mut_ptr(),
                1,
                self.n_out as isize,
                false,
                x.as_ptr(),
                1,
                self.n_in as isize,
                self.weight.as_ptr(),
                self.n_in as isize,
                1,
                0.0,
                1.0,
                false,
                false,
                false,
                gemm::Parallelism::Rayon(threads),
            );
        }
        out
    }
}

impl LayerNorm {
    /// candle's `layer_norm` kernel on one row, in place.
    fn apply(&self, row: &mut [f32], eps: f32) {
        let n = row.len() as f32;
        let (mut sum, mut sum2) = (0f32, 0f32);
        for &v in row.iter() {
            sum += v;
            sum2 += v * v;
        }
        let mean = sum / n;
        let var = sum2 / n - mean * mean;
        let inv_std = (var + eps).sqrt().recip();
        for ((d, &w), &b) in row.iter_mut().zip(&self.weight).zip(&self.bias) {
            *d = (*d - mean) * inv_std * w + b;
        }
    }
}

fn gelu_erf(v: f32) -> f32 {
    (libm::erff(v * std::f32::consts::FRAC_1_SQRT_2) + 1.) * 0.5 * v
}

impl TitleEncoder {
    /// Load from a directory holding `config.json`, `tokenizer.json`,
    /// `model.safetensors` and optionally `model-f16.gguf` (GPU backend).
    pub fn load(dir: &Path) -> Result<Self> {
        let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(anyhow::Error::msg)
            .context("loading encoder tokenizer.json")?;
        Ok(Self {
            tokenizer,
            cpu: Bert::load(dir)?,
            #[cfg(feature = "vulkan")]
            gpu: load_gpu(dir),
        })
    }

    /// Whether vectors come from the GPU backend (not the exact CPU path).
    pub fn on_gpu(&self) -> bool {
        #[cfg(feature = "vulkan")]
        return self.gpu.is_some();
        #[cfg(not(feature = "vulkan"))]
        false
    }

    /// One L2-normalised `DIM`-wide vector per text, in input order.
    pub fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let encodings = self.tokenize(texts)?;
        let seqs = real_tokens(&encodings);
        #[cfg(feature = "vulkan")]
        if let Some(gpu) = &self.gpu {
            return Ok(gpu.embed(&seqs)?.iter().map(|v| unit_prefix(v)).collect());
        }
        let mut order: Vec<usize> = (0..seqs.len()).collect();
        order.sort_by_key(|&i| seqs[i].len());
        let mut out: Vec<Vec<f32>> = vec![Vec::new(); seqs.len()];
        for chunk in order.chunks(BATCH) {
            let batch: Vec<&[u32]> = chunk.iter().map(|&i| seqs[i]).collect();
            for (&i, v) in chunk.iter().zip(self.cpu.forward(&batch)?) {
                out[i] = unit_prefix(&v);
            }
        }
        Ok(out)
    }

    fn tokenize(&self, texts: &[&str]) -> Result<Vec<tokenizers::Encoding>> {
        self.tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(anyhow::Error::msg)
    }
}

/// Real tokens only: tokenizer.json pads to the batch's longest, and padding
/// positions are masked out of attention and pooling anyway.
fn real_tokens(encodings: &[tokenizers::Encoding]) -> Vec<&[u32]> {
    encodings
        .iter()
        .map(|e| {
            let n = e.get_attention_mask().iter().filter(|&&m| m != 0).count();
            &e.get_ids()[..n]
        })
        .collect()
}

#[cfg(feature = "vulkan")]
fn load_gpu(dir: &Path) -> Option<super::gpu::GpuEncoder> {
    let gguf = dir.join("model-f16.gguf");
    if std::env::var("MUSICLIB_ENCODER").as_deref() == Ok("cpu") || !gguf.exists() {
        return None;
    }
    match super::gpu::GpuEncoder::load(&gguf) {
        Ok(gpu) => {
            eprintln!("title encoder: Vulkan ({})", gpu.device());
            Some(gpu)
        }
        Err(e) => {
            eprintln!("title encoder: Vulkan unavailable ({e:#}); using the CPU");
            None
        }
    }
}

/// The first `DIM` values of a pooled vector, L2-normalised.
fn unit_prefix(v: &[f32]) -> Vec<f32> {
    let head = &v[..DIM];
    let norm = head.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
    head.iter().map(|x| x / norm).collect()
}

impl Bert {
    fn load(dir: &Path) -> Result<Self> {
        let config: Config = serde_json::from_str(
            &std::fs::read_to_string(dir.join("config.json"))
                .context("reading encoder config.json")?,
        )
        .context("parsing encoder config.json")?;
        if config.hidden_act != "gelu" {
            bail!(
                "encoder hidden_act {:?} unsupported (only exact \"gelu\")",
                config.hidden_act
            );
        }
        if !config
            .hidden_size
            .is_multiple_of(config.num_attention_heads)
        {
            bail!("encoder hidden_size is not a multiple of num_attention_heads");
        }
        let w = Weights(
            candle_core::safetensors::load(dir.join("model.safetensors"), &Device::Cpu)
                .context("loading encoder weights")?,
        );
        let (h, i) = (config.hidden_size, config.intermediate_size);
        let word =
            w.0.get("embeddings.word_embeddings.weight")
                .context("word embeddings missing")?;
        let max_pos =
            w.0.get("embeddings.position_embeddings.weight")
                .context("position embeddings missing")?;
        let (vocab, positions) = (word.dims()[0], max_pos.dims()[0]);
        let mut token_type = w.get("embeddings.token_type_embeddings.weight", &[2, h])?;
        token_type.truncate(h);
        let layers = (0..config.num_hidden_layers)
            .map(|l| {
                let p = format!("encoder.layer.{l}");
                Ok(Layer {
                    query: w.linear(&format!("{p}.attention.self.query"), h, h)?,
                    key: w.linear(&format!("{p}.attention.self.key"), h, h)?,
                    value: w.linear(&format!("{p}.attention.self.value"), h, h)?,
                    attn_out: w.linear(&format!("{p}.attention.output.dense"), h, h)?,
                    attn_norm: w.norm(&format!("{p}.attention.output.LayerNorm"), h)?,
                    intermediate: w.linear(&format!("{p}.intermediate.dense"), h, i)?,
                    output: w.linear(&format!("{p}.output.dense"), i, h)?,
                    out_norm: w.norm(&format!("{p}.output.LayerNorm"), h)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            hidden: h,
            heads: config.num_attention_heads,
            eps: config.layer_norm_eps as f32,
            word: w.get("embeddings.word_embeddings.weight", &[vocab, h])?,
            position: w.get("embeddings.position_embeddings.weight", &[positions, h])?,
            token_type,
            emb_norm: w.norm("embeddings.LayerNorm", h)?,
            layers,
            threads: candle_core::utils::get_num_threads(),
        })
    }

    /// Mean-pooled vectors (first `DIM` values) for a batch of token sequences.
    fn forward(&self, seqs: &[&[u32]]) -> Result<Vec<Vec<f32>>> {
        let mut starts = Vec::with_capacity(seqs.len());
        let mut rows = 0;
        for s in seqs {
            starts.push(rows);
            rows += s.len();
        }
        let h = self.hidden;
        let n_pos = self.position.len() / h;
        let n_vocab = self.word.len() / h;

        // Embeddings: (word + token type) + position, then layer norm.
        let mut x = vec![0f32; rows * h];
        let positions: Vec<(u32, usize)> = seqs
            .iter()
            .flat_map(|s| s.iter().enumerate().map(|(p, &id)| (id, p)))
            .collect();
        if let Some(&(id, _)) = positions.iter().find(|(id, _)| *id as usize >= n_vocab) {
            bail!("token id {id} outside the encoder vocabulary");
        }
        if seqs.iter().any(|s| s.len() > n_pos) {
            bail!("text longer than the encoder's {n_pos} positions");
        }
        x.par_chunks_mut(h)
            .zip(&positions)
            .for_each(|(row, &(id, p))| {
                let word = &self.word[id as usize * h..][..h];
                let pos = &self.position[p * h..][..h];
                for k in 0..h {
                    row[k] = (word[k] + self.token_type[k]) + pos[k];
                }
                self.emb_norm.apply(row, self.eps);
            });

        for layer in &self.layers {
            x = self.layer(layer, &x, rows, seqs, &starts);
        }

        // Mean pool over each text's tokens (first DIM dims).
        Ok(seqs
            .par_iter()
            .zip(&starts)
            .map(|(s, &start)| {
                let mut mean = vec![0f32; DIM];
                for row in x[start * h..(start + s.len()) * h].chunks(h) {
                    for (m, &v) in mean.iter_mut().zip(row) {
                        *m += v;
                    }
                }
                let count = (s.len() as f32).max(1e-9);
                for m in &mut mean {
                    *m /= count;
                }
                mean
            })
            .collect())
    }

    fn layer(
        &self,
        l: &Layer,
        x: &[f32],
        rows: usize,
        seqs: &[&[u32]],
        starts: &[usize],
    ) -> Vec<f32> {
        let h = self.hidden;
        let t = self.threads;
        let mut q = l.query.matmul(x, rows, t);
        let mut k = l.key.matmul(x, rows, t);
        let mut v = l.value.matmul(x, rows, t);
        for (m, lin) in [(&mut q, &l.query), (&mut k, &l.key), (&mut v, &l.value)] {
            m.par_chunks_mut(h).for_each(|row| {
                for (a, &b) in row.iter_mut().zip(&lin.bias) {
                    *a += b;
                }
            });
        }

        // Self-attention, one (text, head) at a time over the text's own tokens.
        let d = h / self.heads;
        let scale = (1.0 / (d as f64).sqrt()) as f32;
        let per_text: Vec<Vec<f32>> = seqs
            .par_iter()
            .zip(starts)
            .map(|(s, &start)| {
                let n = s.len();
                let mut ctx = vec![0f32; n * h];
                let mut probs = vec![0f32; n];
                for head in 0..self.heads {
                    let col = head * d;
                    for i in 0..n {
                        let qi = &q[(start + i) * h + col..][..d];
                        let mut max = f32::NEG_INFINITY;
                        for j in 0..n {
                            let kj = &k[(start + j) * h + col..][..d];
                            let s = qi.iter().zip(kj).map(|(a, b)| a * b).sum::<f32>() * scale;
                            probs[j] = s;
                            max = max.max(s);
                        }
                        let mut sum = 0f32;
                        for p in &mut probs {
                            *p = (*p - max).exp();
                            sum += *p;
                        }
                        let out = &mut ctx[i * h + col..][..d];
                        for (j, p) in probs.iter().enumerate() {
                            let p = p / sum;
                            let vj = &v[(start + j) * h + col..][..d];
                            for (o, &vv) in out.iter_mut().zip(vj) {
                                *o += p * vv;
                            }
                        }
                    }
                }
                ctx
            })
            .collect();
        let ctx: Vec<f32> = per_text.concat();

        // Attention output: dense + bias + residual, layer norm.
        let mut a = l.attn_out.matmul(&ctx, rows, t);
        a.par_chunks_mut(h)
            .zip(x.par_chunks(h))
            .for_each(|(row, res)| {
                for ((o, &b), &r) in row.iter_mut().zip(&l.attn_out.bias).zip(res) {
                    *o = (*o + b) + r;
                }
                l.attn_norm.apply(row, self.eps);
            });

        // Feed-forward: dense + bias → GELU → dense + bias + residual, layer norm.
        let mut inter = l.intermediate.matmul(&a, rows, t);
        inter.par_chunks_mut(l.intermediate.n_out).for_each(|row| {
            for (o, &b) in row.iter_mut().zip(&l.intermediate.bias) {
                *o = gelu_erf(*o + b);
            }
        });
        let mut out = l.output.matmul(&inter, rows, t);
        out.par_chunks_mut(h)
            .zip(a.par_chunks(h))
            .for_each(|(row, res)| {
                for ((o, &b), &r) in row.iter_mut().zip(&l.output.bias).zip(res) {
                    *o = (*o + b) + r;
                }
                l.out_norm.apply(row, self.eps);
            });
        out
    }
}
