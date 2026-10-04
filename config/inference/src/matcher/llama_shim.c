// Thin C layer over llama.cpp for the title encoder's Vulkan backend
// (feature `vulkan`, see gpu.rs). It exists so that llama.cpp's parameter
// structs are filled in by a C compiler reading the real llama.h, not by
// hand-written Rust layouts that would silently break on a llama.cpp update.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "ggml-backend.h"
#include "llama.h"

typedef struct {
    struct llama_model * model;
    ggml_backend_dev_t   dev;
} shim_model_t;

typedef struct {
    struct llama_context * ctx;
    struct llama_batch     batch;
    int                    n_ubatch;
} shim_context_t;

static void quiet_log(enum ggml_log_level level, const char * text, void * user) {
    (void) user;
    if (level == GGML_LOG_LEVEL_ERROR) {
        fputs(text, stderr);
    }
}

static ggml_backend_dev_t find_gpu(void) {
    for (size_t i = 0; i < ggml_backend_dev_count(); i++) {
        ggml_backend_dev_t dev = ggml_backend_dev_get(i);
        if (ggml_backend_dev_type(dev) == GGML_BACKEND_DEVICE_TYPE_GPU) {
            return dev;
        }
    }
    return NULL;
}

// Load `path` onto the first discrete GPU. NULL with `err` set when there is
// none or loading fails. `backend_dir` holds dynamically loaded ggml backends
// for llama.cpp builds that ship them that way (may be NULL).
void * shim_load(const char * backend_dir, const char * path, char * err, size_t err_len) {
    static int initialised = 0;
    if (!initialised) {
        llama_log_set(quiet_log, NULL);
        llama_backend_init();
        initialised = 1;
    }
    ggml_backend_dev_t dev = find_gpu();
    if (dev == NULL && backend_dir != NULL) {
        ggml_backend_load_all_from_path(backend_dir);
        dev = find_gpu();
    }
    if (dev == NULL) {
        snprintf(err, err_len, "no discrete GPU among %zu ggml devices", ggml_backend_dev_count());
        return NULL;
    }
    ggml_backend_dev_t devices[2] = { dev, NULL };
    struct llama_model_params mp = llama_model_default_params();
    mp.devices      = devices;
    mp.n_gpu_layers = 999;
    struct llama_model * model = llama_model_load_from_file(path, mp);
    if (model == NULL) {
        snprintf(err, err_len, "llama.cpp could not load %s", path);
        return NULL;
    }
    shim_model_t * m = malloc(sizeof *m);
    m->model = model;
    m->dev   = dev;
    return m;
}

const char * shim_device_name(void * m) {
    return ggml_backend_dev_description(((shim_model_t *) m)->dev);
}

int shim_n_embd(void * m) {
    return llama_model_n_embd(((shim_model_t *) m)->model);
}

void shim_free_model(void * m) {
    if (m != NULL) {
        llama_model_free(((shim_model_t *) m)->model);
        free(m);
    }
}

// An embedding context: mean pooling, flash attention (ggml-vulkan skips the
// fully masked blocks between packed texts), up to `n_seq_max` texts and
// `n_ubatch` tokens per call.
void * shim_context(void * m, int n_ubatch, int n_seq_max) {
    struct llama_context_params cp = llama_context_default_params();
    cp.n_ctx           = n_ubatch;
    cp.n_batch         = n_ubatch;
    cp.n_ubatch        = n_ubatch;
    cp.n_seq_max       = n_seq_max;
    cp.embeddings      = true;
    cp.pooling_type    = LLAMA_POOLING_TYPE_MEAN;
    cp.flash_attn_type = LLAMA_FLASH_ATTN_TYPE_ENABLED;
    cp.no_perf         = true;
    cp.n_threads       = 2;
    cp.n_threads_batch = 2;
    struct llama_context * ctx = llama_init_from_model(((shim_model_t *) m)->model, cp);
    if (ctx == NULL) {
        return NULL;
    }
    shim_context_t * c = malloc(sizeof *c);
    c->ctx      = ctx;
    c->batch    = llama_batch_init(n_ubatch, 0, 1);
    c->n_ubatch = n_ubatch;
    return c;
}

void shim_free_context(void * c) {
    if (c != NULL) {
        shim_context_t * s = c;
        llama_batch_free(s->batch);
        llama_free(s->ctx);
        free(s);
    }
}

// Encode `n_seqs` texts packed back to back in `tokens` (text i has `lens[i]`
// tokens) and write each text's mean-pooled embedding to `out`
// (`n_seqs * n_embd` floats). 0 on success.
int shim_encode(void * c, const int32_t * tokens, const int32_t * lens, int n_seqs, float * out, int n_embd) {
    shim_context_t * s = c;
    struct llama_batch * b = &s->batch;
    b->n_tokens = 0;
    for (int q = 0; q < n_seqs; q++) {
        for (int p = 0; p < lens[q]; p++) {
            if (b->n_tokens >= s->n_ubatch) {
                return -1;
            }
            int k = b->n_tokens++;
            b->token[k]     = *tokens++;
            b->pos[k]       = p;
            b->n_seq_id[k]  = 1;
            b->seq_id[k][0] = q;
            b->logits[k]    = 1;
        }
    }
    int rc = llama_encode(s->ctx, *b);
    if (rc != 0) {
        return rc;
    }
    for (int q = 0; q < n_seqs; q++) {
        const float * e = llama_get_embeddings_seq(s->ctx, q);
        if (e == NULL) {
            return -2;
        }
        memcpy(out + (size_t) q * n_embd, e, sizeof(float) * n_embd);
    }
    return 0;
}
