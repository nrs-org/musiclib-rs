mod mock_client;
mod mock_server;

pub use mock_client::MockHttpClient;
pub use mock_server::MockServer;

pub fn init_test_logger() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        dotenv::dotenv().ok();
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .init();
    });
}
