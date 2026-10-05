use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{get, post, put},
};
use serde::Deserialize;
use std::{net::SocketAddr, path::PathBuf, sync::Arc};

mod api;
mod errors;
mod programs;
mod sync;
mod web;

const MAX_WASM_SIZE: usize = 64 * 1024 * 1024;

#[derive(Deserialize)]
struct Settings {
    #[serde(default = "default_data_dir")]
    data_dir: PathBuf,
    #[serde(default = "default_host")]
    host: String,
    #[serde(default = "default_port")]
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
    // reads RIBOLITA_DATA_DIR, RIBOLITA_HOST and RIBOLITA_PORT
    let settings: Settings = envy::prefixed("RIBOLITA_").from_env()?;
    std::fs::create_dir_all(&settings.data_dir)?;

    let addr: SocketAddr = format!("{}:{}", settings.host, settings.port).parse()?;

    let api_keys = programs::load_api_keys(&settings.data_dir)?;

    let secret = ribolita_node::net::load_or_create_secret(&settings.data_dir)?;
    let app_state = Arc::new(api::AppState {
        data_dir: settings.data_dir,
        api_keys: tokio::sync::RwLock::new(api_keys),
        author: ribolita_node::net::author_of(&secret),
        envs: Default::default(),
        node_id: secret.public().to_string(),
        node: Default::default(),
    });
    println!("iroh node id: {}", app_state.node_id);
    sync::spawn(app_state.clone(), secret);

    let app = Router::new()
        .route("/", get(web::index))
        .route(
            "/upload",
            post(web::upload).layer(DefaultBodyLimit::max(MAX_WASM_SIZE)),
        )
        .route(
            "/api/:program_id/program",
            put(api::update_program).layer(DefaultBodyLimit::max(MAX_WASM_SIZE)),
        )
        .route("/api/:program_id/entry", post(api::entry))
        .route("/api/:program_id/state", get(api::state))
        .route("/api/:program_id/entries", get(api::entries))
        .with_state(app_state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("listening on http://{}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}
