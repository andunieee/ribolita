use crate::{errors::ApiError, programs};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State as AxumState},
    http::{HeaderMap, header::AUTHORIZATION},
};
use ribolita::{MdbxStore, Query as EntriesQuery, RunEnvironment, RunError};
use ribolita_common::{
    entry::{Author, EntryId, EntryOutput, entry_to_json},
    rmp_json,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};
use tokio::sync::RwLock;

type Env = RunEnvironment<MdbxStore>;

pub(crate) struct AppState {
    pub(crate) data_dir: PathBuf,
    pub(crate) api_keys: RwLock<HashMap<String, String>>,
    /// entries created through the API are attributed to the server
    pub(crate) author: Author,
    /// one open environment per program: databases can't be opened twice in a process, and
    /// entries of a program must be processed one at a time
    pub(crate) envs: Mutex<HashMap<String, Arc<Mutex<Env>>>>,
    /// the server's iroh node id, which devices connect to
    pub(crate) node_id: String,
    pub(crate) node: OnceLock<tokio::sync::mpsc::UnboundedSender<ribolita_node::net::Cmd>>,
}

impl AppState {
    /// Pushes a program's changes to the peers it's synced with.
    fn sync(&self, program: &str) {
        if let Some(node) = self.node.get() {
            let program = Some(program.to_string());
            node.send(ribolita_node::net::Cmd::Sync { program }).ok();
        }
    }

    pub(crate) fn env(&self, program_id: &str) -> Result<Arc<Mutex<Env>>, ApiError> {
        let mut envs = self.envs.lock().unwrap();
        if let Some(env) = envs.get(program_id) {
            return Ok(env.clone());
        }
        let program = programs::load_program(&self.data_dir, program_id)?;
        let wasm = std::fs::read(&program.wasm_path)
            .map_err(|e| ApiError::internal(format!("failed to read wasm: {e}")))?;
        let env = Env::new(&program.dir, &wasm, self.author).map_err(ApiError::internal_anyhow)?;
        let env = Arc::new(Mutex::new(env));
        envs.insert(program_id.to_string(), env.clone());
        Ok(env)
    }
}

/// Runs blocking work (wasm, database access) on a program's environment off the async runtime.
async fn with_env<T: Send + 'static>(
    state: Arc<AppState>,
    program_id: String,
    f: impl FnOnce(&mut Env) -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(move || {
        let env = state.env(&program_id)?;
        let mut env = env.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut env)
    })
    .await
    .map_err(ApiError::internal_anyhow)?
}

/// Failures caused by the program or the request are the client's problem, not the server's.
fn run_error(err: RunError) -> ApiError {
    match err {
        RunError::Rejected(_) | RunError::Wasm(_) | RunError::BadWasm(_) => {
            ApiError::bad_request(err.to_string())
        }
        RunError::NotFound(_) => ApiError::not_found(err.to_string()),
        _ => ApiError::internal(err.to_string()),
    }
}

#[derive(Deserialize)]
pub struct EntryRequest {
    /// when the event happened, in seconds since the unix epoch; defaults to now
    when: Option<u64>,
    kind: String,
    #[serde(default)]
    data: BTreeMap<String, serde_json::Value>,
}

#[derive(Serialize)]
pub struct EntryResponse {
    id: String,
    /// existing entries the program rejects after this one was inserted before them
    rejected: Vec<RejectedEntry>,
}

#[derive(Serialize)]
pub struct RejectedEntry {
    id: String,
    error: String,
}

#[derive(Serialize)]
pub struct ProgramResponse {
    replayed: usize,
}

pub(crate) async fn entry(
    AxumState(state): AxumState<Arc<AppState>>,
    Path(program_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<EntryRequest>,
) -> Result<Json<EntryResponse>, ApiError> {
    authorize(&headers, &state, &program_id).await?;

    let values = payload
        .data
        .into_iter()
        .map(|(k, v)| (k, rmp_json::json_value_to_rmp_value(&v)))
        .collect();

    let (entry, report) = with_env(state.clone(), program_id.clone(), move |env| {
        env.add_with_report(payload.kind, values, payload.when)
            .map_err(run_error)
    })
    .await?;
    let rejected = report
        .failed
        .into_iter()
        .map(|(id, error)| RejectedEntry {
            id: id.to_string(),
            error,
        })
        .collect();
    state.sync(&program_id);
    Ok(Json(EntryResponse {
        id: entry.id.to_string(),
        rejected,
    }))
}

/// Replaces the program's wasm module (raw request body) and rebuilds its state from the log.
/// If any entry fails under the new module, the old one is kept and nothing changes.
pub(crate) async fn update_program(
    AxumState(state): AxumState<Arc<AppState>>,
    Path(program_id): Path<String>,
    headers: HeaderMap,
    wasm: Bytes,
) -> Result<Json<ProgramResponse>, ApiError> {
    authorize(&headers, &state, &program_id).await?;
    let program = programs::load_program(&state.data_dir, &program_id)?;
    let old_wasm = std::fs::read(&program.wasm_path)
        .map_err(|e| ApiError::internal(format!("failed to read wasm: {e}")))?;

    let replayed = with_env(state, program_id, move |env| {
        env.set_module(&wasm).map_err(run_error)?;
        let replayed = match env.replay() {
            Ok(report) if report.failed.is_empty() => report.applied,
            result => {
                // put the old module back and rebuild the state it produced
                env.set_module(&old_wasm).map_err(run_error)?;
                env.replay().map_err(run_error)?;
                return Err(match result {
                    Ok(report) => {
                        let (id, err) = &report.failed[0];
                        ApiError::bad_request(format!(
                            "new module rejects {} existing entries, e.g. {id}: {err}",
                            report.failed.len()
                        ))
                    }
                    Err(err) => run_error(err),
                });
            }
        };
        programs::write_wasm(&program.dir, &wasm)?;
        Ok(replayed)
    })
    .await?;

    Ok(Json(ProgramResponse { replayed }))
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
    let path = query.path.unwrap_or_default();
    let json_value = with_env(state, program_id, move |env| {
        env.state_json(&path).map_err(ApiError::internal_anyhow)
    })
    .await?;

    Ok(Json(json_value))
}

#[derive(Deserialize)]
pub struct EntriesQueryParams {
    /// comma-separated entry ids
    id: Option<String>,
    /// seconds since the unix epoch
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

    let query = if let Some(ids) = query.id {
        let ids = ids
            .split(',')
            .map(|id| id.trim().parse::<EntryId>())
            .collect::<Result<_, _>>()
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        EntriesQuery::ByIds(ids)
    } else {
        EntriesQuery::ByTime {
            since: query.since,
            until: None,
            limit: query.last,
        }
    };

    let entries = with_env(state, program_id, move |env| {
        env.entries(query).map_err(run_error)
    })
    .await?;
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
    if !constant_time_eq(token.as_bytes(), expected.as_bytes()) {
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

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
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
