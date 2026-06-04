# Batching providers — design plan

## Motivation

Several upstream APIs accept multiple entity IDs per call:

| Backend         | Endpoint                                | Limit              | Cost                              |
|-----------------|-----------------------------------------|--------------------|-----------------------------------|
| YouTube Data    | `videos.list?id=A,B,...`                | 50 IDs             | 1 quota unit regardless of count  |
| Spotify         | `tracks?ids=A,B,...`, `albums?ids=...`  | 50 / 20            | 1 rate-limit unit                 |
| Discogs         | (limited)                               | —                  | —                                 |

YouTube is the most painful: today every video is a separate request, each consuming 1 quota unit. With batching, a 50-video channel costs 1 unit instead of 50.

The hard part is fitting this into the existing per-entity `FetchProvider` API and the URL-keyed `HttpCache` without leaking batching concerns into backend code or breaking cache invariants.

## Two-part solution

1. **Coalescer layer in the HTTP stack** — a new optional layer, configured with per-endpoint *coalesce rules*, that merges single-entity requests into one batched upstream call and splits the response back. The layer lives below the caches and above the scheduler, so cache hits skip it entirely and the scheduler sees one batched call per N waiters.

2. **Tick-boundary drain inside the coalescer** — the principled way to decide *when* to fire a batch without timers or count thresholds. The coalescer's driver task uses cooperative scheduling to coalesce everything that's "ready right now," at which point it fires.

The rest of this doc explains both.

---

## Part 1: tick-boundary drain (background)

### Async scheduling, briefly

Rust async tasks are cooperative. A task runs straight through any synchronous code, and only yields control back to the runtime when it hits an `await` on something not-yet-ready (a channel with no message, a socket with no data, a timer that hasn't fired, etc.). At that point the task **parks**: the runtime takes it off the ready queue and runs the next ready task instead. When the awaited thing becomes ready, the task is put back on the ready queue and will eventually be resumed.

A useful mental model: the runtime is a loop that pulls the next ready task off a queue, runs it until it parks (or completes), then repeats. We'll call one iteration of that loop a **scheduling round** (the literature sometimes calls this a "tick", though the term is overloaded).

There are two primitives that matter for the drain pattern:

- `tokio::sync::mpsc::Receiver::recv().await` parks until at least one message is in the channel, then returns it.
- `tokio::sync::mpsc::Receiver::try_recv()` is synchronous: returns immediately with `Ok(msg)` if something is queued, `Err(Empty)` otherwise. It never parks.
- `tokio::task::yield_now().await` parks the current task and *immediately* puts it back on the ready queue. Effect: every other currently-ready task gets one scheduling round to run before you resume.

### The drain pattern

The coalescer runs a single long-lived **driver task**. Because quota is the scarce resource (not latency), we want to coalesce as aggressively as possible — so we yield-and-drain in a loop, exiting only when the API ceiling is hit or the queue truly drains:

```rust
loop {
    // 1. Park until somebody enqueues. Only blocking await in the loop.
    let first = rx.recv().await.expect("senders never all dropped");
    let mut batch = vec![first];

    // 2. Yield-and-drain until one of the two exit conditions fires.
    loop {
        tokio::task::yield_now().await;
        let before = batch.len();
        while let Ok(more) = rx.try_recv() {
            batch.push(more);
            if batch.len() >= max_batch { break; }
        }
        if batch.len() >= max_batch { break; } // hit API ceiling → fire
        if batch.len() == before    { break; } // drained dry      → fire
    }

    // 3. Fire one upstream call for the whole batch.
    spawn_batch(batch);
}
```

The trick is the inner loop. Each iteration:

- **`yield_now`** parks the driver and gives every other currently-ready task one scheduling round. Sibling fetch tasks poised at their enqueue point use this round to push into the channel.
- **`try_recv` drain** synchronously snapshots whatever is now queued.
- **Exit conditions:** stop if we've hit the API's per-call ceiling (`max_batch`, e.g. 50 for YouTube) — at which point queueing more wouldn't help anyway, the next batch will pick them up — or if the last yield produced no new items (the well is dry, more waiting can't help).

Without the yield in step 2, the driver would wake the *instant* one request is queued and snapshot immediately, missing any sibling task that hadn't quite reached its enqueue point. With the loop, we keep extending the batch as long as new work keeps trickling in, but never wait forever.

### Worked timeline

Suppose the importer spawns three concurrent tasks to fetch videos `X`, `Y`, `Z`, and the channel was empty. The coalescer's driver task is parked on `recv()`.

```
Round 1:  task-X runs:    sends X into channel → parks awaiting response.
          driver wakes (recv() became ready); enters inner loop;
                          yield_now → parks.
          task-Y runs:    sends Y → parks awaiting response.
          task-Z runs:    sends Z → parks awaiting response.
          (queue: [Y, Z])

Round 2:  driver resumes after yield.
          try_recv drains Y, Z. batch = [X, Y, Z], grew from 1 → 3.
          Below max_batch; progress was made → yield_now again → parks.

Round 3:  driver resumes. try_recv finds nothing. batch unchanged.
          "Drained dry" exit fires. Fire one HTTP call.

(later)   response arrives, splits into three, wakes X, Y, Z's waiters.
```

**Slow-producer case.** If task-Z reaches its enqueue point one round later than X and Y (say it had to await one extra thing first), Round 2's drain catches X and Y; Round 3 catches Z; Round 4 sees no progress and fires the batch of 3. The loop is more patient than a single-yield design, so this case becomes one batch of 3 instead of one batch of 2 plus a straggler batch of 1.

**Drip-producer worst case.** If somehow one item arrives per round forever, the batch grows by 1 each iteration and fires after `max_batch` rounds (~50 yields, microseconds). The API ceiling is the universal termination guarantee.

### Termination & exit guarantees

- **`max_batch` is the universal upper bound on iterations.** Even a worst-case "1 item per yield" drip producer fills the batch in at most `max_batch` rounds, which on tokio is microseconds. There is no way for the inner loop to run forever.
- **"Drained dry" handles the normal case in 1–3 rounds.** Fan-out via `tokio::spawn` typically has all siblings reach their enqueue point within one round; the loop exits at iteration 2 or 3.
- **Timer-free.** No `tokio::time::sleep(Duration::from_millis(5))` heuristics anywhere.
- **Count-free for the *minimum* batch size.** The only count threshold is the API ceiling, which is a hard constraint, not a tuning knob.
- **Quota-optimal asymptotically.** As long as producers eventually park (every realistic producer does), the loop captures everything that would have arrived "in this wave."

### Honest caveats

- **`yield_now` is a soft scheduling primitive, not a synchronization barrier.** It relies on tokio scheduling fairly enough that "currently-ready tasks get one round before me." On a multi-threaded runtime, "round" is fuzzy across threads. In practice this works, but it's a heuristic, not a contract — see the design discussion in the issue/PR for why we accept it anyway.
- **Sequential callers can't be helped.** If the importer processes one video at a time, there's nothing to coalesce. The actual workload fans out via `tokio::spawn` + `join_all`, so this doesn't bite.
- **Chunking.** If 200 IDs land before the loop exits, we cap at `max_batch` per call. The driver's outer `loop` immediately re-enters and forms the next batch from whatever is still queued — no extra yields needed because those items are already in the channel.
- **Fragmentation.** A producer that's slightly *more* than one round behind its siblings can still cause a fragmented batch (e.g. 49 + 1 instead of 50). Won't happen with current fan-out patterns; if it does, the fix is producer-side (`tokio::spawn` the enqueue path so it's poised), not in the coalescer.

### Variant: `recv_many`

Tokio 1.30+ has `mpsc::Receiver::recv_many(buf, limit)` which does roughly `recv().await` followed by a drain in one call. It does *not* include the yield, so it misses tasks that hadn't quite reached their enqueue point. We want the explicit `recv → (yield + try_recv)*` pattern instead.

---

## Part 2: coalescer layer

### Where it sits in the stack

Current order, innermost (network) → outermost (caller):

```
DefaultHttpClient → DomainScheduler → DbHttpCache (opt) → MemoryHttpCache (opt)
```

New order:

```
DefaultHttpClient → DomainScheduler → Coalescer (opt) → DbHttpCache (opt) → MemoryHttpCache (opt)
```

Rationale:

- **Above the scheduler:** the merged call is what counts as one scheduled request. Individual waiters don't each consume a scheduler slot.
- **Below the caches:** cache hits short-circuit the coalescer entirely. A warm cache means no batching is needed (or possible). Cache *writes* happen per-waiter on the way back up — each split sub-response is stored under its own single-entity URL, so the cache stays single-key.

### Trait shape

```rust
/// One coalesce rule. The layer holds a Vec of these; first match wins.
pub trait CoalesceRule: Send + Sync {
    /// If this request is batchable under this rule, return a CoalesceKey.
    /// Requests with equal CoalesceKey may be merged into one upstream call.
    /// Return None to pass the request through unchanged.
    fn group(&self, req: &Request) -> Option<CoalesceKey>;

    /// Max IDs per merged request (e.g. 50 for YouTube videos.list).
    fn max_batch(&self) -> usize;

    /// Merge N pre-grouped requests into one upstream request.
    /// All inputs share the same CoalesceKey.
    fn merge(&self, reqs: &[Request]) -> Request;

    /// Split a batched response back into per-input responses.
    /// Returns one entry per input request, in the same order.
    /// `None` means "the upstream returned nothing for this ID"
    /// (e.g. video deleted, ISRC unknown) — the layer will translate
    /// that into a synthetic 404 / empty response per the rule's choice.
    fn split(&self, batched: &Response, reqs: &[Request]) -> Vec<Option<Response>>;
}

/// Opaque hash key. Implementations build it from method + path + non-id
/// query params + headers that affect response (Accept-Language, etc.).
pub struct CoalesceKey(u64);
```

### Layer internals

```rust
struct Coalescer<Inner> {
    inner: Arc<Inner>,
    rules: Vec<Arc<dyn CoalesceRule>>,
    groups: Mutex<HashMap<CoalesceKey, mpsc::Sender<Pending>>>,
}

struct Pending {
    req: Request,
    respond: oneshot::Sender<Result<Response, Error>>,
}
```

On `make_request`:

1. Walk `rules`; first one to return `Some(key)` wins. If none matches, delegate straight to `inner` (no-op).
2. Look up (or create) the per-group sender. Creating it also spawns a driver task that owns the receiver and the matching `CoalesceRule`.
3. Push `Pending { req, respond }` into the sender, await on the oneshot.

Driver task per group (see Part 1 for the reasoning behind the loop shape):

```rust
let max_batch = rule.max_batch();
loop {
    let first = rx.recv().await?;
    let mut batch = vec![first];

    loop {
        tokio::task::yield_now().await;
        let before = batch.len();
        while let Ok(more) = rx.try_recv() {
            batch.push(more);
            if batch.len() >= max_batch { break; }
        }
        if batch.len() >= max_batch { break; } // hit API ceiling → fire
        if batch.len() == before    { break; } // drained dry      → fire
    }

    // If we hit max_batch but more is queued, the outer loop iter picks it up
    // (no extra yield needed — those items are already in the channel).
    spawn_fire(batch, rule.clone(), inner.clone());
}
```

`spawn_fire` is a separate task so the driver returns to the drain loop immediately and isn't blocked on the HTTP round-trip:

```rust
async fn spawn_fire(batch: Vec<Pending>, rule: Arc<dyn CoalesceRule>, inner: Arc<Inner>) {
    let reqs: Vec<_> = batch.iter().map(|p| p.req.clone()).collect();
    let merged = rule.merge(&reqs);
    match inner.make_request(merged).await {
        Ok(resp) => {
            let parts = rule.split(&resp, &reqs);
            for (pending, part) in batch.into_iter().zip(parts) {
                let _ = pending.respond.send(Ok(part.unwrap_or_else(synthetic_404)));
            }
        }
        Err(e) => {
            // All waiters get the same error. Cloning Error must be cheap or
            // we wrap in Arc internally.
            for pending in batch {
                let _ = pending.respond.send(Err(e.clone()));
            }
        }
    }
}
```

### Cache interaction

The crucial invariant: **each split sub-response must look byte-identical (for caching purposes) to what a single-entity call for that ID would have returned.** That way the cache key (which is derived from the single-entity URL the caller actually issued) stores the right body, and a subsequent cold-start run can replay from cache without going through the coalescer.

For YouTube `videos.list`:
- Single-entity body: `{ "kind": "youtube#videoListResponse", "etag": "...", "items": [<video>], "pageInfo": {...} }`
- Batched body: same shape with multiple `items`.
- `split` produces, per input: `{ "kind": ..., "items": [<this id's video>], "pageInfo": {...} }`.

The `etag` and `pageInfo` will be different from a true single-entity call, but no caller reads them, so the synthesis is fine. (Worth a comment in the rule impl.)

### Cache key safety

If a caller sets `Request::cache_key` to something custom, the coalescer must refuse to batch it (return `None` from `group`). The default `"{METHOD}:{url}"` keying is what makes single-entity cache lookups line up with split sub-responses; custom keys break the assumption.

---

## Part 3: first concrete user — YouTube `videos.list`

Backend file: `src/providers/backends/youtube_api/`.

### Rule impl sketch

```rust
pub struct YouTubeVideosListRule;

impl CoalesceRule for YouTubeVideosListRule {
    fn group(&self, req: &Request) -> Option<CoalesceKey> {
        let url = req.url();
        if url.host() != Some("www.googleapis.com") { return None; }
        if !url.path().ends_with("/youtube/v3/videos") { return None; }
        if req.cache_key().is_some() { return None; } // safety

        // Group by everything except the `id` param.
        let mut hasher = ...;
        for (k, v) in url.query_pairs() {
            if k != "id" { hash(&mut hasher, k, v); }
        }
        hash_method_and_path(&mut hasher, req);
        Some(CoalesceKey(hasher.finish()))
    }

    fn max_batch(&self) -> usize { 50 }

    fn merge(&self, reqs: &[Request]) -> Request {
        // Take reqs[0] as the template, replace `id` with the union.
        let mut url = reqs[0].url().clone();
        let ids: Vec<_> = reqs.iter().map(extract_id).collect();
        replace_query(&mut url, "id", &ids.join(","));
        Request::new(reqs[0].method().clone(), url)
            .with_headers(reqs[0].headers().clone())
    }

    fn split(&self, batched: &Response, reqs: &[Request]) -> Vec<Option<Response>> {
        let body: VideoListResponse = batched.json()?;
        let by_id: HashMap<_, _> = body.items.into_iter()
            .map(|v| (v.id.clone(), v))
            .collect();

        reqs.iter().map(|req| {
            let id = extract_id(req);
            by_id.get(&id).map(|video| {
                let synth = VideoListResponse {
                    kind: body.kind.clone(),
                    etag: String::new(),
                    items: vec![video.clone()],
                    page_info: PageInfo { total: 1, per_page: 1 },
                };
                Response::from_json(&synth).with_status(batched.status())
            })
        }).collect()
    }
}
```

### Wiring

Add a `coalescers: Vec<CoalescerKind>` field to `HttpClientConfig` (or per-backend section of `RegistryConfig`); `HttpClientConfig::build` inserts the layer in the right position when the list is non-empty. The YouTube backend registers its rule when `build_providers` constructs it.

### Backend code changes

Ideally zero. The backend keeps calling `http.make_request(GET /videos?id=X&part=...)`. The fact that it's batched downstream is invisible.

---

## Edge cases / open questions

1. **Partial failures.** If the upstream returns 200 but `items` lacks some requested IDs (video is private/deleted), `split` returns `None` for those — the layer converts to a synthetic 404 (or empty 200, depending on backend semantics). Decision per rule.

2. **Heterogeneous request settings.** Two callers ask for the same video but with different `part=` parameters. `group` should include `part` in the key, so they don't merge. Net effect: same video may be fetched twice if requested with different parts — acceptable, and rare in practice.

3. **`Retry-After` / 429 handling.** Lives in the scheduler, which sits below the coalescer. A retried batch is one merged call — each retry is amortized across all waiters. No change needed.

4. **Backpressure.** If 10 000 video IDs queue up before any batch fires, the channel buffer fills. Pick a generous bound (`mpsc::channel(1024)`?) and let `send` await — that just means callers are throttled, which is fine.

5. **Group eviction.** A group's sender + driver task stay alive forever once created. Cheap (one parked task per active endpoint shape) and avoids races on tear-down. Don't bother evicting.

6. **Cancellation.** If a waiter drops its oneshot receiver mid-flight, the coalescer should still complete the batch (other waiters depend on it) and just discard the orphaned response. `oneshot::Sender::send` returns `Err` if the receiver is gone — ignore it.

7. **Error cloning.** Coalesced waiters need to receive the same `Error` on failure. Either make `Error: Clone`, or wrap in `Arc<Error>` internally and clone the arc. Verify what `http::Error` (in this crate) actually is before deciding.

8. **`update_fixtures` interaction.** `RawFetchProvider` paths must still see *raw, non-coalesced* responses for fixture recording. Easiest: `update_fixtures` builds its `HttpClient` without the coalescer layer. Worth confirming in `src/bin/update_fixtures/`.

9. **Test strategy.** A unit test for the drain pattern using `tokio::time::pause()` and `tokio::task::yield_now()` can deterministically check that N concurrent `make_request`s coalesce into one inner call. The mock client (`test_utils::MockHttpClient`) needs to be told to expect the merged URL.

---

## Rollout

| Step | Change                                                                         | File(s)                                                 |
|------|--------------------------------------------------------------------------------|---------------------------------------------------------|
| 1    | `CoalesceRule` trait, `CoalesceKey`, `Pending`                                 | `src/http/coalescer.rs` (new)                           |
| 2    | `Coalescer<Inner>` layer with driver task and `make_request` impl              | same                                                    |
| 3    | Hook into `HttpClientConfig::build`; new optional `coalescers` config field    | `src/http/mod.rs`, `src/http/config.rs`                 |
| 4    | Unit tests with mock inner client: drain timing, splitting, partial misses     | `src/http/coalescer.rs` tests                           |
| 5    | `YouTubeVideosListRule` impl + registration in YouTube backend builder         | `src/providers/backends/youtube_api/`                   |
| 6    | Verify against real fixtures: a multi-video channel import does ≤ N/50 calls   | integration test or manual run with `import.sh`         |
| 7    | Confirm `update_fixtures` bypasses the coalescer                               | `src/bin/update_fixtures/`                              |
| 8    | (Later) Add rules for Spotify `tracks?ids=` and `albums?ids=`                  | `src/providers/backends/spotify/`                       |

Step 1–4 stand alone and can land in a PR by themselves with no behaviour change. Step 5 lights up YouTube. Step 8 reuses the same machinery for other backends.

## Non-goals

- **MusicBrainz.** All MusicBrainz `inc=` parameter sets are constant in this codebase, so there's nothing to coalesce — every request to a given endpoint already has identical query params.
- **Cross-endpoint coalescing** (e.g. fetching a video and a playlist in one round-trip). YouTube doesn't support it; not worth designing for.
- **Persistent batch queues across runs.** Drain is per-process; cold-start always goes through cache misses first, then batches naturally.
