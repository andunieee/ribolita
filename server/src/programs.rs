use crate::errors::ApiError;
use rand::{rngs::OsRng, RngCore};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

const PROGRAM_WASM_FILE: &str = "program.wasm";
const PROGRAM_API_KEY_FILE: &str = "api.key";

#[derive(Clone)]
pub(crate) struct ProgramSummary {
    pub(crate) id: String,
}

#[derive(Clone)]
pub(crate) struct Program {
    pub(crate) dir: PathBuf,
    pub(crate) wasm_path: PathBuf,
}

pub(crate) fn list_programs(data_dir: &Path) -> Result<Vec<ProgramSummary>, ApiError> {
    let mut programs = Vec::new();
    for entry in std::fs::read_dir(data_dir)
        .map_err(|e| ApiError::internal(format!("failed to read data dir: {e}")))?
    {
        let entry =
            entry.map_err(|e| ApiError::internal(format!("failed to read dir entry: {e}")))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(id) = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
        else {
            continue;
        };

        let wasm_path = program_wasm_path(&path);
        if !wasm_path.is_file() {
            continue;
        }

        programs.push(ProgramSummary { id });
    }
    programs.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(programs)
}

pub(crate) fn is_valid_program_id(program_id: &str) -> bool {
    !program_id.is_empty()
        && program_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub(crate) fn load_program(data_dir: &Path, program_id: &str) -> Result<Program, ApiError> {
    // ids come from URL paths, where `%2F` decodes to `/`
    if !is_valid_program_id(program_id) {
        return Err(ApiError::not_found("program not found"));
    }
    let dir = data_dir.join(program_id);
    if !dir.is_dir() {
        return Err(ApiError::not_found("program not found"));
    }

    let wasm_path = program_wasm_path(&dir);
    if !wasm_path.is_file() {
        return Err(ApiError::not_found("program.wasm not found"));
    }

    Ok(Program { dir, wasm_path })
}

pub(crate) fn load_api_keys(data_dir: &Path) -> Result<HashMap<String, String>, ApiError> {
    let mut map = HashMap::new();
    for program in list_programs(data_dir)? {
        let dir = data_dir.join(&program.id);
        let api_key = load_or_create_api_key(&dir)?;
        map.insert(program.id, api_key);
    }
    Ok(map)
}

pub(crate) fn load_api_key_for_program(
    data_dir: &Path,
    program_id: &str,
) -> Result<String, ApiError> {
    let program = load_program(data_dir, program_id)?;
    load_or_create_api_key(&program.dir)
}

pub(crate) fn program_wasm_path(program_dir: &Path) -> PathBuf {
    program_dir.join(PROGRAM_WASM_FILE)
}

/// Writes `program.wasm` atomically, so a crash never leaves a truncated module behind.
pub(crate) fn write_wasm(program_dir: &Path, wasm: &[u8]) -> Result<(), ApiError> {
    let tmp_path = program_dir.join(format!("{PROGRAM_WASM_FILE}.tmp"));
    std::fs::write(&tmp_path, wasm)
        .and_then(|_| std::fs::rename(&tmp_path, program_wasm_path(program_dir)))
        .map_err(|e| ApiError::internal(format!("failed to write program.wasm: {e}")))
}

pub(crate) fn api_key_path(program_dir: &Path) -> PathBuf {
    program_dir.join(PROGRAM_API_KEY_FILE)
}

pub(crate) fn load_or_create_api_key(program_dir: &Path) -> Result<String, ApiError> {
    let key_path = api_key_path(program_dir);
    if key_path.is_file() {
        let api_key = std::fs::read_to_string(&key_path)
            .map_err(|e| ApiError::internal(format!("failed to read api key: {e}")))?;
        let api_key = api_key.trim().to_string();
        if api_key.is_empty() {
            return Err(ApiError::internal("api key file is empty"));
        }
        return Ok(api_key);
    }

    let api_key = generate_api_key();
    write_api_key(program_dir, &api_key)?;
    Ok(api_key)
}

pub(crate) fn write_api_key(program_dir: &Path, api_key: &str) -> Result<(), ApiError> {
    let key_path = api_key_path(program_dir);
    std::fs::write(&key_path, api_key)
        .map_err(|e| ApiError::internal(format!("failed to write api key: {e}")))
}

pub(crate) fn generate_api_key() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}
