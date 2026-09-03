use crate::state_db::{deserialize_rows, serialize_rows};
use crate::{EntriesDatabase, ReadTransaction, StateDatabase, WriteTransaction};
use log::trace;
use ribolita_common::ROW_COUNT_BYTES;
use ribolita_common::{Opcode, entry::Entry};
use std::collections::BTreeMap;
use std::io::Cursor;
use wasmtime::{Engine, Linker, Module, Store};

#[derive(Debug)]
pub struct RunEnvironment<S: StateDatabase + 'static> {
    module: wasmtime::Module,
    linker: wasmtime::Linker<WasmData<S>>,
    store: wasmtime::Store<WasmData<S>>,
}

struct WasmData<S> {
    result: Result<(), String>,
    state: S,
    entries: EntriesDatabase,
}

impl<S> std::fmt::Debug for WasmData<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<WasmData {:?}>", self.result)
    }
}

impl<S: StateDatabase + 'static> RunEnvironment<S> {
    pub fn new(db_path: &std::path::Path, wasm_module: &[u8]) -> Result<Self, RunError> {
        let engine = Engine::default();
        let module = Module::new(&engine, wasm_module).map_err(RunError::WasmtimeShenanigans)?;

        let state_path = db_path.join("state");
        let entries_path = db_path.join("entries");
        std::fs::create_dir_all(&state_path)?;
        std::fs::create_dir_all(&entries_path)?;
        let state = S::init(&state_path).map_err(|str| {
            RunError::DatabaseOpen(entries_path.to_string_lossy().to_string(), str)
        })?;
        let entries = EntriesDatabase::init(&entries_path).map_err(|str| {
            RunError::DatabaseOpen(entries_path.to_string_lossy().to_string(), str)
        })?;

        let store = Store::new(
            &engine,
            WasmData {
                result: Ok(()),
                state,
                entries,
            },
        );
        let mut linker = Linker::new(&engine);

        linker
            .func_wrap(
                "env",
                "state_get_internal",
                move |mut caller: wasmtime::Caller<'_, WasmData<S>>, ptr: i32, len: i32| -> i32 {
                    let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                    let data = mem.data(&caller);
                    let path_prefix = &data[ptr as usize..ptr as usize + len as usize];

                    let (_num_rows, rows_data) = {
                        let wd = caller.data();
                        let state_txn = match wd.state.start_read_txn() {
                            Ok(txn) => txn,
                            Err(_) => return -1,
                        };
                        match state_txn.get(&path_prefix) {
                            Ok(result) => result,
                            Err(_) => return -1,
                        }
                    };

                    // write the state as `[u32: buf_size]][serialize_rows(..)]` bytes.
                    // i.e. [u32: buf_size][u32: number_of_rows][data]
                    if let Some(ptr) = caller
                        .get_export("malloc")
                        .and_then(|exp| exp.into_func())
                        .and_then(|fun| fun.typed::<i32, i32>(&caller).ok())
                        .and_then(|alloc| {
                            alloc
                                .call(&mut caller, (ROW_COUNT_BYTES + rows_data.len()) as i32)
                                .ok()
                        })
                    {
                        if mem
                            .write(&mut caller, ptr as usize, &(rows_data.len().to_le_bytes()))
                            .and_then(|_| {
                                mem.write(&mut caller, ptr as usize + ROW_COUNT_BYTES, &rows_data)
                            })
                            .is_ok()
                        {
                            ptr
                        } else {
                            -3
                        }
                    } else {
                        -2
                    }
                },
            )
            .map_err(RunError::Linking)?;

        linker
            .func_wrap(
                "env",
                "state_modify_internal",
                move |mut caller: wasmtime::Caller<'_, WasmData<S>>,
                      key_ptr: i32,
                      key_len: i32,
                      val_ptr: i32,
                      val_len: i32,
                      opcode: i32| {
                    trace!("state_modify {:?}", opcode);
                    let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                    let data = mem.data(&caller);
                    let path_prefix = &data[key_ptr as usize..key_ptr as usize + key_len as usize];

                    // Copy data to owned vectors to avoid borrowing issues
                    let path_owned = path_prefix.to_vec();
                    let rows_owned =
                        data[val_ptr as usize..val_ptr as usize + val_len as usize].to_vec();

                    let result = {
                        let wd = caller.data();
                        let state_txn = match wd.state.start_write_txn() {
                            Ok(txn) => txn,
                            Err(_) => return -1,
                        };
                        let res = match Opcode::from(opcode) {
                            Opcode::Set => match state_txn.set(&path_owned, &rows_owned) {
                                Ok(_) => Ok(()),
                                Err(err) => Err(err.to_string()),
                            },
                            Opcode::Incr | Opcode::Decr => {
                                let (num_rows, existing_rows) = match state_txn.get(&path_owned) {
                                    Ok(result) => result,
                                    Err(_err) => return -1,
                                };

                                let updated = match apply_numeric_delta(
                                    num_rows,
                                    &existing_rows,
                                    &rows_owned,
                                    Opcode::from(opcode),
                                ) {
                                    Ok(data) => data,
                                    Err(_) => return -1,
                                };

                                match state_txn.set(&path_owned, &updated) {
                                    Ok(_) => Ok(()),
                                    Err(err) => Err(err.to_string()),
                                }
                            }
                            _ => Err("unknown opcode".to_string()),
                        };

                        // Commit the transaction if successful
                        if res.is_ok() && state_txn.commit().is_err() {
                            return -1;
                        }
                        res
                    };

                    match result {
                        Ok(_) => 1,
                        Err(_) => -1,
                    }
                },
            )
            .map_err(RunError::Linking)?;

        linker
            .func_wrap(
                "env",
                "print_from_buffer",
                |mut caller: wasmtime::Caller<'_, WasmData<S>>, ptr: i32, len: i32| {
                    let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                    let data = mem.data(&caller);
                    match std::str::from_utf8(&data[ptr as usize..ptr as usize + len as usize]) {
                        Ok(message) => {
                            eprintln!("[wasm] printed: {}", message);
                        }
                        Err(err) => {
                            eprintln!("[wasm] failed to print: {}", err);
                        }
                    }
                },
            )
            .map_err(RunError::Linking)?
            .func_wrap(
                "env",
                "return_err",
                |mut caller: wasmtime::Caller<'_, WasmData<S>>, ptr: i32, len: i32| {
                    let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                    let data = mem.data(&caller);
                    let err_str =
                        std::str::from_utf8(&data[ptr as usize..ptr as usize + len as usize])
                            .expect("unexpected utf8 decode error")
                            .to_string();
                    let s = caller.data_mut();
                    s.result = Err(err_str);
                },
            )
            .map_err(RunError::Linking)?;

        Ok(RunEnvironment {
            module,
            store,
            linker,
        })
    }
}

fn apply_numeric_delta(
    existing_num_rows: usize,
    existing_rows: &[u8],
    delta_rows: &[u8],
    opcode: Opcode,
) -> Result<Vec<u8>, String> {
    let mut current: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    if existing_num_rows > 0 {
        for (key, value) in deserialize_rows(existing_rows) {
            current.insert(key.to_vec(), value.to_vec());
        }
    }

    for (rel_key, delta_value) in deserialize_rows(delta_rows) {
        let delta = decode_integer(delta_value)?;
        let base = match current.get(rel_key) {
            Some(existing) => decode_integer(existing)?,
            None => 0,
        };

        let next = match opcode {
            Opcode::Incr => base
                .checked_add(delta)
                .ok_or("integer overflow".to_string())?,
            Opcode::Decr => base
                .checked_sub(delta)
                .ok_or("integer overflow".to_string())?,
            _ => return Err("unsupported opcode".to_string()),
        };

        current.insert(rel_key.to_vec(), encode_integer(next)?);
    }

    let rows = current
        .into_iter()
        .map(|(key, value)| (key, value))
        .collect::<Vec<_>>();
    Ok(serialize_rows(&rows))
}

fn decode_integer(value: &[u8]) -> Result<i128, String> {
    let val = rmpv::decode::read_value(&mut Cursor::new(value))
        .map_err(|_| "failed to decode value".to_string())?;
    match val {
        rmpv::Value::Integer(n) => {
            if let Some(v) = n.as_i64() {
                Ok(v as i128)
            } else if let Some(v) = n.as_u64() {
                Ok(v as i128)
            } else {
                Err("integer out of range".to_string())
            }
        }
        rmpv::Value::Nil => Ok(0),
        _ => Err("expected integer".to_string()),
    }
}

fn encode_integer(value: i128) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    let val = if (i64::MIN as i128) <= value && value <= (i64::MAX as i128) {
        rmpv::Value::Integer((value as i64).into())
    } else if value >= 0 && value <= (u64::MAX as i128) {
        rmpv::Value::Integer((value as u64).into())
    } else {
        return Err("integer out of range".to_string());
    };

    rmpv::encode::write_value(&mut buf, &val).map_err(|_| "failed to encode value".to_string())?;
    Ok(buf)
}

impl<S: StateDatabase + 'static> RunEnvironment<S> {
    pub fn run(&mut self, mut entry: Entry) -> Result<(), RunError> {
        // Get next entry ID with a temporary read transaction
        let entry_id = {
            let wd = self.store.data();
            let read_txn = wd
                .entries
                .start_read_txn()
                .map_err(RunError::DatabaseAction)?;
            wd.entries
                .get_last_id(&read_txn)
                .map_err(RunError::DatabaseAction)?
                + 1
        };

        entry.id = entry_id;

        // state transactions will be created as needed in WASM calls

        let instance = self
            .linker
            .instantiate(&mut self.store, &self.module)
            .map_err(RunError::WasmtimeShenanigans)?;

        // run init
        let init_fn = instance
            .get_typed_func::<(), ()>(&mut self.store, "init")
            .map_err(RunError::MissingExport)?;
        init_fn
            .call(&mut self.store, ())
            .map_err(RunError::WasmtimeShenanigans)?;

        // write entry
        let alloc_fn = instance
            .get_typed_func::<i32, i32>(&mut self.store, "malloc")
            .map_err(RunError::MissingExport)?;
        let mem = instance
            .get_memory(&mut self.store, "memory")
            .ok_or(RunError::MissingMemory)?;
        let entry_bytes = rmp_serde::to_vec(&entry)?;
        let write_ptr = alloc_fn
            .call(&mut self.store, entry_bytes.len() as i32)
            .map_err(RunError::WasmtimeShenanigans)? as usize;
        mem.write(&mut self.store, write_ptr, &entry_bytes)?;

        // run main process
        let process_fn = instance
            .get_typed_func::<(i32, i32), ()>(&mut self.store, "process")
            .map_err(RunError::MissingExport)?;
        process_fn
            .call(
                &mut self.store,
                (write_ptr as i32, entry_bytes.len() as i32),
            )
            .map_err(RunError::WasmtimeShenanigans)?;

        // check if there isn't an error in the wasm call
        self.store
            .data()
            .result
            .as_ref()
            .cloned()
            .map_err(|err_str| RunError::WasmCall(err_str.clone()))?;

        // save entry with separate transaction
        {
            let wd = self.store.data();
            let entries_txn = wd
                .entries
                .start_write_txn()
                .map_err(RunError::DatabaseAction)?;

            wd.entries.save(&entries_txn, entry.id, &entry)?;

            entries_txn.commit().map_err(|e| RunError::DatabaseAction(e.into()))?;
        }

        // state transactions are committed individually in WASM calls

        Ok(())
    }
}

#[derive(thiserror::Error, Debug)]
pub enum RunError {
    #[error("database open at {0}: {1}")]
    DatabaseOpen(String, String),

    #[error("database action: {0}")]
    DatabaseAction(#[from] anyhow::Error),

    #[error("wasmtime shenanigans: {0}")]
    WasmtimeShenanigans(wasmtime::Error),

    #[error("error linking: {0}")]
    Linking(wasmtime::Error),

    #[error("missing export on wasm: {0}")]
    MissingExport(anyhow::Error),

    #[error("wasm doesn't export 'memory'")]
    MissingMemory,

    #[error("failed to encode to messagepack: {0}")]
    MessagePackEncode(#[from] rmp_serde::encode::Error),

    #[error("failed to write to wasm buffer: {0}")]
    Write(#[from] wasmtime::MemoryAccessError),

    #[error("bad wasm code: {0}")]
    BadWasm(String),

    #[error("wasm call errored: {0}")]
    WasmCall(String),

    #[error("failed to read error message from wasm: {0}")]
    ReadError(#[from] std::io::Error),

    #[error("failed to decode from messagepack: {0}")]
    MessagePackDecode(#[from] rmp_serde::decode::Error),

    #[error("database error: {0}")]
    DatabaseString(String),
}

impl From<String> for RunError {
    fn from(s: String) -> Self {
        RunError::DatabaseString(s)
    }
}
