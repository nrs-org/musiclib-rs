mod http;
mod httpcache;
mod providers;

#[cfg(test)]
mod test_utils;

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    tracing::info!("starting up");
    tracing::debug!("debug message");
    tracing::warn!("something looks off");
    tracing::error!("something went wrong");
}
