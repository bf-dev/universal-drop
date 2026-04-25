use anyhow::Result;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};
use universal_drop::{Config, build_state, routes, scan_input_dir, start_worker, watcher};

#[tokio::main]
async fn main() -> Result<()> {
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env()?;
    config.ensure_dirs()?;
    let bind_addr = config.bind_addr;
    let state = build_state(config);
    let _worker = start_worker(state.clone());
    let queued = scan_input_dir(&state).await?;
    if !queued.is_empty() {
        info!(count = queued.len(), "queued files discovered at startup");
    }
    let _watcher = watcher::start_input_watcher(state.clone())?;

    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    info!(%bind_addr, "universal-drop listening");
    axum::serve(listener, app).await?;
    Ok(())
}
