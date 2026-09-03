use crate::{errors::ApiError, programs};
use axum::{
    Json,
    extract::{Path, Query, State as AxumState},
    http::{HeaderMap, header::AUTHORIZATION},
};
use ribolita::{
    EntriesDatabase, Query as EntriesQuery, ReadTransaction, RunEnvironment, StateDatabase,
    StateLMDBDatabase,
};
use ribolita_common::{
    entry::{Entry, EntryOutput, entry_to_json},
    key, rmp_json,
    state::State as StateValue,
    value,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::Arc,
};
use tokio::sync::RwLock;

pub(crate) struct AppState {
    pub(crate) data_dir: PathBuf,
    pub(crate) api_keys: RwLock<HashMap<String, String>>,
}

#[derive(Deserialize)]
pub struct EntryRequest {
    when: Option<u64>,
    kind: String,
    #[serde(default)]
    data: BTreeMap<String, serde_json::Value>,
}

#[derive(Serialize)]
pub struct StatusResponse {
    status: &'static str,
}

pub(crate) async fn entry(
    AxumState(state): AxumState<Arc<AppState>>,
    Path(program_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<EntryRequest>,
) -> Result<Json<StatusResponse>, ApiError> {
    authorize(&headers, &state, &program_id).await?;
    let program = programs::load_program(&state.data_dir, &program_id)?;
    let wasm = std::fs::read(&program.wasm_path)
        .map_err(|e| ApiError::internal(format!("failed to read wasm: {e}")))?;

    let when = payload.when.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    });
    let values = payload
        .data
        .into_iter()
        .map(|(k, v)| (k, rmp_json::json_value_to_rmp_value(&v)))
        .collect();

    let entry = Entry {
        id: 0,
        when,
        kind: payload.kind,
        values,
    };

    let mut env: RunEnvironment<StateLMDBDatabase> =
        RunEnvironment::new(&program.dir, &wasm).map_err(ApiError::internal_anyhow)?;
    env.run(entry).map_err(ApiError::internal_anyhow)?;

    Ok(Json(StatusResponse { status: "ok" }))
}

#[derive(Deserialize)]
pub struct StateQuery {
    path: Option<String>,
}

pub(crate) async fn state(
    AxumState(state): AxumState<Arc<AppState>>,
    Path(program_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<StateQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorize(&headers, &state, &program_id).await?;
    let program = programs::load_program(&state.data_dir, &program_id)?;
    let state_dir = program.dir.join("state");
    std::fs::create_dir_all(&state_dir)
        .map_err(|e| ApiError::internal(format!("failed to create state dir: {e}")))?;
    let state_db = StateLMDBDatabase::init(&state_dir).map_err(ApiError::internal_anyhow)?;
    let txn = state_db
        .start_read_txn()
        .map_err(ApiError::internal_anyhow)?;

    let path_parts = query
        .path
        .as_ref()
        .map(|s| key::parse(s))
        .unwrap_or_default();
    let prefix = key::to_bytes(&path_parts);
    let (num_rows, data) = txn.get(&prefix).map_err(ApiError::internal_anyhow)?;
    let state: StateValue =
        value::from_bytes(num_rows, &data).map_err(ApiError::internal_anyhow)?;
    let json_value = rmp_json::rmp_value_to_json_value(&state.0);

    Ok(Json(json_value))
}

#[derive(Deserialize)]
pub struct EntriesQueryParams {
    #[serde(default)]
    id: Vec<u64>,
    since: Option<u64>,
    last: Option<usize>,
}

pub(crate) async fn entries(
    AxumState(state): AxumState<Arc<AppState>>,
    Path(program_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<EntriesQueryParams>,
) -> Result<Json<Vec<EntryOutput>>, ApiError> {
    authorize(&headers, &state, &program_id).await?;
    let program = programs::load_program(&state.data_dir, &program_id)?;
    let entries_dir = program.dir.join("entries");
    std::fs::create_dir_all(&entries_dir)
        .map_err(|e| ApiError::internal(format!("failed to create entries dir: {e}")))?;
    let entries_db = EntriesDatabase::init(&entries_dir).map_err(ApiError::internal_anyhow)?;
    let txn = entries_db
        .start_read_txn()
        .map_err(ApiError::internal_anyhow)?;

    let query = if !query.id.is_empty() {
        EntriesQuery::ByIds(query.id)
    } else {
        EntriesQuery::ByTimestamps {
            since: query.since,
            until: None,
            limit: query.last,
        }
    };

    let entries = entries_db
        .query(&txn, query)
        .map_err(ApiError::internal_anyhow)?;
    let output = entries.into_iter().map(entry_to_json).collect();

    Ok(Json(output))
}

async fn authorize(
    headers: &HeaderMap,
    state: &AppState,
    program_id: &str,
) -> Result<(), ApiError> {
    let token = bearer_token(headers)
        .ok_or_else(|| ApiError::unauthorized("missing Authorization header"))?;
    let expected = get_or_load_api_key(state, program_id).await?;
    if token != expected {
        return Err(ApiError::unauthorized("invalid api key"));
    }
    Ok(())
}

async fn get_or_load_api_key(state: &AppState, program_id: &str) -> Result<String, ApiError> {
    if let Some(api_key) = state.api_keys.read().await.get(program_id).cloned() {
        return Ok(api_key);
    }

    let api_key = programs::load_api_key_for_program(&state.data_dir, program_id)?;
    state
        .api_keys
        .write()
        .await
        .insert(program_id.to_string(), api_key.clone());
    Ok(api_key)
}

fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let header = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let value = header.strip_prefix("Bearer ")?;
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}
