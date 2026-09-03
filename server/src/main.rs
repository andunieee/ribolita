use axum::{
    Router,
    routing::{get, post},
};
use serde::Deserialize;
use std::{net::SocketAddr, path::PathBuf, sync::Arc};

mod api;
mod errors;
mod programs;
mod web;

#[derive(Deserialize)]
struct Settings {
    #[serde(rename = "RIBOLITA_DATA_DIR", default = "default_data_dir")]
    data_dir: PathBuf,
    #[serde(rename = "RIBOLITA_HOST", default = "default_host")]
    host: String,
    #[serde(rename = "RIBOLITA_PORT", default = "default_port")]
    port: u16,
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("./data")
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    7333
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let settings: Settings = envy::from_env()?;
    std::fs::create_dir_all(&settings.data_dir)?;

    let addr: SocketAddr = format!("{}:{}", settings.host, settings.port).parse()?;

    let api_keys = programs::load_api_keys(&settings.data_dir)?;

    let app_state = Arc::new(api::AppState {
        data_dir: settings.data_dir,
        api_keys: tokio::sync::RwLock::new(api_keys),
    });

    let app = Router::new()
        .route("/", get(web::index))
        .route("/upload", post(web::upload))
        .route("/api/:program_id/entry", post(api::entry))
        .route("/api/:program_id/state", get(api::state))
        .route("/api/:program_id/entries", get(api::entries))
        .with_state(app_state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("listening on http://{}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}
