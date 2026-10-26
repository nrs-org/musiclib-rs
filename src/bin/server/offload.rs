//! Runs imports on a multi-thread runtime of their own. The server's main
//! runtime is single-threaded (see `main.rs`), so an import run there walks
//! on one core and holds up every request while it saves. The import
//! pipeline is `Send` (the `import` CLI runs it on a multi-thread runtime),
//! and its soft-dedup already takes a thread of its own.

use std::{future::Future, sync::OnceLock};

use tokio::runtime::Runtime;
use tokio_util::task::AbortOnDropHandle;

/// Lives as long as the process: HTTP connections opened from it stay
/// pooled for requests made from the main runtime too.
fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .thread_name("import-worker")
            .enable_all()
            .build()
            .expect("building the import runtime")
    })
}

/// Runs `fut` on the import runtime and waits for it. Dropping the returned
/// future (a cancelled job) aborts it; a panic in it resumes here, so the
/// job fails as if it had panicked in place.
pub async fn run<F>(fut: F) -> F::Output
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let task = AbortOnDropHandle::new(runtime().spawn(fut));
    match task.await {
        Ok(output) => output,
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use super::run;

    /// The result comes back, off the calling thread; a dropped call (a
    /// cancelled job) stops the work.
    #[tokio::test(flavor = "current_thread")]
    async fn runs_elsewhere_and_stops_when_dropped() {
        let caller = std::thread::current().id();
        assert_ne!(
            run(async move { std::thread::current().id() }).await,
            caller
        );

        let finished = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&finished);
        let call = run(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            flag.store(true, Ordering::SeqCst);
        });
        let _ = tokio::time::timeout(Duration::from_millis(20), call).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!finished.load(Ordering::SeqCst));
    }
}
