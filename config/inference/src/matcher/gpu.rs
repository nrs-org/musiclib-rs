//! Vulkan backend for the title encoder (feature `vulkan`): llama.cpp running
//! the bundle's `encoder/model-f16.gguf` (written by `bundle.py`) on the first
//! discrete GPU, through the C shim in `llama_shim.c`.
//!
//! The GGUF has no vocabulary: texts are tokenized by the same
//! `tokenizer.json` as the CPU path and passed in as token ids. llama.cpp's
//! BERT uses f16 weights and the tanh GELU, so its vectors differ from the
//! CPU path's by float noise only (cosine ≥ 0.99998 against the Python
//! reference on the parity texts).
//!
//! Throughput comes from packing many short texts into each call and running
//! several contexts at once (measured on an RTX 3060 Laptop, live library):
//! 512 tokens per call with flash attention, which skips the masked blocks
//! between texts; larger calls lose to the tokens² attention mask llama.cpp
//! builds on the CPU per call. Four contexts hide that host-side work behind
//! the GPU's.

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, bail};

unsafe extern "C" {
    fn shim_load(
        backend_dir: *const c_char,
        path: *const c_char,
        err: *mut c_char,
        err_len: usize,
    ) -> *mut c_void;
    fn shim_device_name(model: *mut c_void) -> *const c_char;
    fn shim_n_embd(model: *mut c_void) -> i32;
    fn shim_free_model(model: *mut c_void);
    fn shim_context(model: *mut c_void, n_ubatch: i32, n_seq_max: i32) -> *mut c_void;
    fn shim_free_context(ctx: *mut c_void);
    fn shim_encode(
        ctx: *mut c_void,
        tokens: *const i32,
        lens: *const i32,
        n_seqs: i32,
        out: *mut f32,
        n_embd: i32,
    ) -> i32;
}

/// Tokens per call (see the module docs).
const UBATCH: usize = 512;
/// llama.cpp's limit on sequences per context.
const MAX_SEQS: usize = 256;
/// Contexts encoding concurrently.
const CONTEXTS: usize = 4;

/// One llama.cpp context; each is used by one thread at a time.
struct Ctx(*mut c_void);

// SAFETY: a llama context may move between threads; the Mutex around each
// keeps it to one thread at a time.
unsafe impl Send for Ctx {}

pub struct GpuEncoder {
    model: *mut c_void,
    contexts: Vec<Mutex<Ctx>>,
    n_embd: usize,
    device: String,
}

// SAFETY: the model is only read after loading (llama.cpp shares one model
// between contexts), and each context is behind its own Mutex.
unsafe impl Send for GpuEncoder {}
unsafe impl Sync for GpuEncoder {}

impl GpuEncoder {
    pub fn load(gguf: &Path) -> Result<Self> {
        // llama.cpp's backend init and model loading aren't safe to run
        // from several threads at once (e.g. parallel tests).
        static LOADING: Mutex<()> = Mutex::new(());
        let _loading = LOADING.lock().unwrap_or_else(|e| e.into_inner());
        let path = CString::new(gguf.to_str().context("GGUF path is not UTF-8")?)?;
        let backend_dir =
            option_env!("LLAMA_BACKEND_DIR").map(|d| CString::new(d).unwrap_or_default());
        let mut err = [0 as c_char; 256];
        let model = unsafe {
            shim_load(
                backend_dir
                    .as_ref()
                    .map_or(std::ptr::null(), |d| d.as_ptr()),
                path.as_ptr(),
                err.as_mut_ptr(),
                err.len(),
            )
        };
        if model.is_null() {
            bail!(
                "{}",
                unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy()
            );
        }
        let mut gpu = Self {
            model,
            contexts: Vec::new(),
            n_embd: unsafe { shim_n_embd(model) } as usize,
            device: unsafe { CStr::from_ptr(shim_device_name(model)) }
                .to_string_lossy()
                .into_owned(),
        };
        for _ in 0..CONTEXTS {
            let ctx = unsafe { shim_context(model, UBATCH as i32, MAX_SEQS as i32) };
            if ctx.is_null() {
                bail!(
                    "llama.cpp could not create an embedding context on {}",
                    gpu.device
                );
            }
            gpu.contexts.push(Mutex::new(Ctx(ctx)));
        }
        Ok(gpu)
    }

    pub fn device(&self) -> &str {
        &self.device
    }

    /// Mean-pooled `n_embd`-wide embedding per token sequence, in input order.
    pub fn embed(&self, seqs: &[&[u32]]) -> Result<Vec<Vec<f32>>> {
        if let Some(s) = seqs.iter().find(|s| s.len() > UBATCH) {
            bail!("a text has {} tokens, more than {UBATCH}", s.len());
        }
        // Length-sorted, so neighbours pack evenly.
        let mut order: Vec<usize> = (0..seqs.len()).collect();
        order.sort_by_key(|&i| seqs[i].len());
        let mut batches: Vec<&[usize]> = Vec::new();
        let mut rest = &order[..];
        while !rest.is_empty() {
            let (mut n, mut tokens) = (0, 0);
            while n < rest.len() && n < MAX_SEQS && tokens + seqs[rest[n]].len() <= UBATCH {
                tokens += seqs[rest[n]].len();
                n += 1;
            }
            let (batch, tail) = rest.split_at(n);
            batches.push(batch);
            rest = tail;
        }

        // Per worker: (input index, embedding) for every text it encoded.
        type Done = Result<Vec<(usize, Vec<f32>)>>;
        let next = AtomicUsize::new(0);
        let results: Vec<Done> = std::thread::scope(|scope| {
            let workers: Vec<_> = self
                .contexts
                .iter()
                .map(|ctx| {
                    let (batches, next) = (&batches, &next);
                    scope.spawn(move || -> Done {
                        let ctx = ctx.lock().unwrap();
                        let mut done = Vec::new();
                        let (mut tokens, mut lens) =
                            (Vec::with_capacity(UBATCH), Vec::with_capacity(MAX_SEQS));
                        let mut out = vec![0f32; MAX_SEQS * self.n_embd];
                        loop {
                            let k = next.fetch_add(1, Ordering::Relaxed);
                            let Some(batch) = batches.get(k) else { break };
                            tokens.clear();
                            lens.clear();
                            for &i in batch.iter() {
                                tokens.extend(seqs[i].iter().map(|&t| t as i32));
                                lens.push(seqs[i].len() as i32);
                            }
                            let rc = unsafe {
                                shim_encode(
                                    ctx.0,
                                    tokens.as_ptr(),
                                    lens.as_ptr(),
                                    batch.len() as i32,
                                    out.as_mut_ptr(),
                                    self.n_embd as i32,
                                )
                            };
                            if rc != 0 {
                                bail!("llama.cpp encode failed ({rc}) on {}", self.device);
                            }
                            for (q, &i) in batch.iter().enumerate() {
                                done.push((
                                    i,
                                    out[q * self.n_embd..(q + 1) * self.n_embd].to_vec(),
                                ));
                            }
                        }
                        Ok(done)
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|w| w.join().expect("encoder worker panicked"))
                .collect()
        });

        let mut vectors = vec![Vec::new(); seqs.len()];
        for done in results {
            for (i, v) in done? {
                vectors[i] = v;
            }
        }
        Ok(vectors)
    }
}

impl Drop for GpuEncoder {
    fn drop(&mut self) {
        for ctx in self.contexts.drain(..) {
            unsafe { shim_free_context(ctx.into_inner().unwrap().0) };
        }
        unsafe { shim_free_model(self.model) };
    }
}
