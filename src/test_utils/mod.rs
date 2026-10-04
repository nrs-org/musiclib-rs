mod mock_client;
mod mock_server;

pub use mock_client::MockHttpClient;
pub use mock_server::MockServer;

/// The learned-matcher checkout whose scripts and cdylib some host tests
/// exercise: `MUSICLIB_LEARNED_MATCHER`, else `../learned-matcher`. `None`
/// (the caller skips) when there is none.
pub fn learned_matcher_dir() -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("MUSICLIB_LEARNED_MATCHER")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../learned-matcher")
        });
    if dir.join("rhai").is_dir() {
        Some(dir)
    } else {
        eprintln!("skipping: no learned-matcher checkout at {}", dir.display());
        None
    }
}

pub fn init_test_logger() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        dotenv::dotenv().ok();
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .init();
    });
}
