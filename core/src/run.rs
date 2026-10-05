use crate::database::Row;
use crate::{EntriesStore, Query, ReadTransaction, StateDatabase, WriteTransaction};
use log::trace;
use ribolita_common::key::{self, KeyPart};
use ribolita_common::state::State;
use ribolita_common::{Opcode, ROW_COUNT_BYTES, entry::Entry, rmp_json, value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;
use wasmtime::{Caller, Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};

/// Instructions (roughly) a single entry may execute before it's aborted.
pub const DEFAULT_FUEL_PER_ENTRY: u64 = 1_000_000_000;
/// Maximum linear memory a program instance may grow to.
pub const DEFAULT_MAX_MEMORY: usize = 256 * 1024 * 1024;

const REQUIRED_EXPORTS: &[&str] = &["memory", "malloc", "init", "process"];
const REPLAY_BATCH: u64 = 1024;

/// Owns a program's databases and its compiled WASM module, and turns entries into state.
pub struct RunEnvironment<S: StateDatabase + 'static, E: EntriesStore + 'static> {
    engine: Engine,
    module: Module,
    linker: Linker<WasmData<S>>,
    store: Store<WasmData<S>>,
    entries: E,
    pub fuel_per_entry: u64,
}

impl<S: StateDatabase + 'static, E: EntriesStore + 'static> std::fmt::Debug
    for RunEnvironment<S, E>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<RunEnvironment {:?}>", self.store.data().result)
    }
}

struct WasmData<S> {
    result: Result<(), String>,
    state: S,
    /// writes of entries already processed in this batch, not yet committed
    base: Overlay,
    /// writes of the entry being processed, merged into `base` if it succeeds
    pending: Overlay,
    limits: StoreLimits,
}

/// State writes made while processing entries, not yet committed to the state database.
///
/// Writes replace whole subtrees (setting `a.b` drops everything that was under `a.b`), which is
/// modeled as a set of cleared prefixes plus the rows written after them.
#[derive(Default)]
struct Overlay {
    cleared: BTreeSet<Vec<u8>>,
    writes: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl Overlay {
    fn is_empty(&self) -> bool {
        self.cleared.is_empty() && self.writes.is_empty()
    }

    /// whether `key` (or a parent of it) was cleared. Keys are self-delimiting, so any byte prefix
    /// that matches a cleared path is a path prefix.
    fn is_cleared(&self, key: &[u8]) -> bool {
        (0..=key.len()).any(|i| self.cleared.contains(&key[..i]))
    }

    fn clear(&mut self, prefix: &[u8]) {
        let under_prefix = |set: &BTreeSet<Vec<u8>>| -> Vec<Vec<u8>> {
            set.range(prefix.to_vec()..)
                .take_while(|k| k.starts_with(prefix))
                .cloned()
                .collect()
        };
        for key in under_prefix(&self.cleared) {
            self.cleared.remove(&key);
        }
        let written: BTreeSet<Vec<u8>> = self.writes.keys().cloned().collect();
        for key in under_prefix(&written) {
            self.writes.remove(&key);
        }
        if !self.is_cleared(prefix) {
            self.cleared.insert(prefix.to_vec());
        }
    }

    fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.writes.insert(key, value);
    }

    /// Rows under `prefix` (full keys) as seen through the overlay, on top of `lower`.
    fn scan(&self, prefix: &[u8], lower: impl FnOnce() -> anyhow::Result<Vec<Row>>) -> anyhow::Result<Vec<Row>> {
        let mut rows = BTreeMap::new();
        if !self.is_cleared(prefix) {
            for (key, value) in lower()? {
                if !self.is_cleared(&key) {
                    rows.insert(key, value);
                }
            }
        }
        for (key, value) in self
            .writes
            .range(prefix.to_vec()..)
            .take_while(|(k, _)| k.starts_with(prefix))
        {
            rows.insert(key.clone(), value.clone());
        }
        Ok(rows.into_iter().collect())
    }

    /// Applies the writes of `newer` on top of this overlay.
    fn merge(&mut self, newer: Overlay) {
        // writes covered by a clear were dropped when it happened, so all clears go first
        for prefix in &newer.cleared {
            self.clear(prefix);
        }
        self.writes.extend(newer.writes);
    }

    fn apply(&self, txn: &impl WriteTransaction) -> anyhow::Result<()> {
        for prefix in &self.cleared {
            txn.delete_prefix(prefix)?;
        }
        for (key, value) in &self.writes {
            txn.put(key, value)?;
        }
        Ok(())
    }
}

/// Copies `len` bytes at `ptr` out of the guest's memory, trapping on out-of-bounds access.
fn guest_bytes<S>(caller: &mut Caller<'_, WasmData<S>>, ptr: i32, len: i32) -> wasmtime::Result<Vec<u8>> {
    let mem = caller
        .get_export("memory")
        .and_then(|exp| exp.into_memory())
        .ok_or_else(|| wasmtime::Error::msg("wasm doesn't export 'memory'"))?;
    let (start, len) = (ptr as u32 as usize, len as u32 as usize);
    mem.data(&caller)
        .get(start..start + len)
        .map(<[u8]>::to_vec)
        .ok_or_else(|| wasmtime::Error::msg(format!("out of bounds guest buffer {start}+{len}")))
}

fn scan_relative<S: StateDatabase>(data: &WasmData<S>, prefix: &[u8]) -> anyhow::Result<Vec<Row>> {
    let mut rows = data.pending.scan(prefix, || {
        data.base
            .scan(prefix, || data.state.start_read_txn()?.scan(prefix))
    })?;
    for (key, _) in rows.iter_mut() {
        key.drain(..prefix.len());
    }
    Ok(rows)
}

fn state_modify<S: StateDatabase>(
    data: &mut WasmData<S>,
    path: &[u8],
    rows_blob: &[u8],
    opcode: Opcode,
) -> anyhow::Result<()> {
    let rows = value::split_rows(rows_blob)?;
    match opcode {
        Opcode::Set => {
            data.pending.clear(path);
            for (rel_key, value) in rows {
                data.pending.put([path, rel_key].concat(), value.to_vec());
            }
        }
        Opcode::Delete => data.pending.clear(path),
        Opcode::Push => {
            // append under the next free index of the sequence at `path`
            let mut next = 0u32;
            for (rel_key, _) in scan_relative(data, path)? {
                match key::decode_part(&rel_key, &mut 0)? {
                    KeyPart::Index(i) => next = next.max(i + 1),
                    KeyPart::Field(_) => anyhow::bail!("push target is a map, not a sequence"),
                }
            }
            let index = key::to_bytes(&[KeyPart::Index(next)]);
            for (rel_key, value) in rows {
                data.pending
                    .put([path, &index, rel_key].concat(), value.to_vec());
            }
        }
        Opcode::Incr | Opcode::Decr => {
            let existing: BTreeMap<Vec<u8>, Vec<u8>> =
                scan_relative(data, path)?.into_iter().collect();
            for (rel_key, delta) in rows {
                let delta = decode_integer(delta)?;
                let base = match existing.get(rel_key) {
                    Some(value) => decode_integer(value)?,
                    None => 0,
                };
                let next = match opcode {
                    Opcode::Incr => base.checked_add(delta),
                    _ => base.checked_sub(delta),
                }
                .ok_or_else(|| anyhow::anyhow!("integer overflow"))?;
                data.pending
                    .put([path, rel_key].concat(), encode_integer(next)?);
            }
        }
        other => anyhow::bail!("unsupported state opcode {other:?}"),
    }
    Ok(())
}

fn build_linker<S: StateDatabase + 'static>(engine: &Engine) -> Result<Linker<WasmData<S>>, RunError> {
    let mut linker = Linker::new(engine);

    linker
        .func_wrap(
            "env",
            "state_get_internal",
            |mut caller: Caller<'_, WasmData<S>>, ptr: i32, len: i32| -> wasmtime::Result<i32> {
                let prefix = guest_bytes(&mut caller, ptr, len)?;
                let rows = scan_relative(caller.data(), &prefix)?;
                let blob = value::join_rows(&rows);

                // hand the guest `[u32 blob_len][blob]`, allocated with its own `malloc`
                let malloc = caller
                    .get_export("malloc")
                    .and_then(|exp| exp.into_func())
                    .ok_or_else(|| wasmtime::Error::msg("wasm doesn't export 'malloc'"))?
                    .typed::<i32, i32>(&caller)?;
                let ptr = malloc.call(&mut caller, (ROW_COUNT_BYTES + blob.len()) as i32)?;
                let mem = caller
                    .get_export("memory")
                    .and_then(|exp| exp.into_memory())
                    .ok_or_else(|| wasmtime::Error::msg("wasm doesn't export 'memory'"))?;
                let ptr_usize = ptr as u32 as usize;
                mem.write(&mut caller, ptr_usize, &(blob.len() as u32).to_le_bytes())?;
                mem.write(&mut caller, ptr_usize + ROW_COUNT_BYTES, &blob)?;
                Ok(ptr)
            },
        )
        .map_err(RunError::Linking)?;

    linker
        .func_wrap(
            "env",
            "state_modify_internal",
            |mut caller: Caller<'_, WasmData<S>>,
             key_ptr: i32,
             key_len: i32,
             val_ptr: i32,
             val_len: i32,
             opcode: i32|
             -> wasmtime::Result<i32> {
                let opcode = Opcode::from(opcode);
                trace!("state_modify {:?}", opcode);
                let path = guest_bytes(&mut caller, key_ptr, key_len)?;
                let rows_blob = guest_bytes(&mut caller, val_ptr, val_len)?;
                state_modify(caller.data_mut(), &path, &rows_blob, opcode)?;
                Ok(1)
            },
        )
        .map_err(RunError::Linking)?;

    linker
        .func_wrap(
            "env",
            "print_from_buffer",
            |mut caller: Caller<'_, WasmData<S>>, ptr: i32, len: i32| -> wasmtime::Result<()> {
                let bytes = guest_bytes(&mut caller, ptr, len)?;
                eprintln!("[wasm] {}", String::from_utf8_lossy(&bytes));
                Ok(())
            },
        )
        .map_err(RunError::Linking)?
        .func_wrap(
            "env",
            "return_err",
            |mut caller: Caller<'_, WasmData<S>>, ptr: i32, len: i32| -> wasmtime::Result<()> {
                let bytes = guest_bytes(&mut caller, ptr, len)?;
                caller.data_mut().result = Err(String::from_utf8_lossy(&bytes).into_owned());
                Ok(())
            },
        )
        .map_err(RunError::Linking)?;

    Ok(linker)
}

fn compile(engine: &Engine, wasm_module: &[u8]) -> Result<Module, RunError> {
    let module = Module::new(engine, wasm_module).map_err(RunError::WasmtimeShenanigans)?;
    let exports: BTreeSet<&str> = module.exports().map(|e| e.name()).collect();
    for name in REQUIRED_EXPORTS {
        if !exports.contains(name) {
            return Err(RunError::BadWasm(format!("missing export '{name}'")));
        }
    }
    Ok(module)
}

impl<S: StateDatabase + 'static, E: EntriesStore + 'static> RunEnvironment<S, E> {
    pub fn new(db_path: &std::path::Path, wasm_module: &[u8]) -> Result<Self, RunError> {
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).map_err(RunError::WasmtimeShenanigans)?;
        let module = compile(&engine, wasm_module)?;

        let state_path = db_path.join("state");
        let entries_path = db_path.join("entries");
        std::fs::create_dir_all(&state_path)?;
        std::fs::create_dir_all(&entries_path)?;
        let state = S::init(&state_path)
            .map_err(|str| RunError::DatabaseOpen(state_path.to_string_lossy().to_string(), str))?;
        let entries = E::init(&entries_path).map_err(|str| {
            RunError::DatabaseOpen(entries_path.to_string_lossy().to_string(), str)
        })?;

        let mut store = Store::new(
            &engine,
            WasmData {
                result: Ok(()),
                state,
                pending: Overlay::default(),
                base: Overlay::default(),
                limits: StoreLimitsBuilder::new()
                    .memory_size(DEFAULT_MAX_MEMORY)
                    .build(),
            },
        );
        store.limiter(|data| &mut data.limits);
        let linker = build_linker(&engine)?;

        Ok(RunEnvironment {
            engine,
            module,
            linker,
            store,
            entries,
            fuel_per_entry: DEFAULT_FUEL_PER_ENTRY,
        })
    }

    /// Swaps the program. Existing state is left as is, call [`Self::replay`] to rebuild it.
    pub fn set_module(&mut self, wasm_module: &[u8]) -> Result<(), RunError> {
        self.module = compile(&self.engine, wasm_module)?;
        Ok(())
    }

    pub fn entries(&self) -> &E {
        &self.entries
    }

    pub fn state_db(&self) -> &S {
        &self.store.data().state
    }

    /// Reads the state under a dotted path (like `"items.3"`) as JSON. Missing paths are `null`.
    pub fn state_json(&self, path: &str) -> anyhow::Result<serde_json::Value> {
        let prefix = key::to_bytes(&key::parse(path)?);
        let blob = self.state_db().start_read_txn()?.get(&prefix)?;
        if value::split_rows(&blob)?.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        let state: State = value::from_bytes(&blob)?;
        Ok(rmp_json::rmp_value_to_json_value(&state.0))
    }

    /// Runs `entry` through the program, then appends it to the log and commits its state
    /// changes. Nothing is written if the program fails. Returns the id assigned to the entry.
    pub fn run(&mut self, mut entry: Entry) -> Result<u64, RunError> {
        entry.id = self.entries.last_id()? + 1;

        self.store.data_mut().base = Overlay::default();
        self.execute(&entry)?;
        let pending = std::mem::take(&mut self.store.data_mut().base);

        // the log is the source of truth: if the state commit fails after this, replay recovers
        self.entries.save(&[&entry])?;
        self.commit_state(&pending)?;

        Ok(entry.id)
    }

    /// Overwrites existing entries (by id) and rebuilds the state from the whole log.
    pub fn update_entries(&mut self, entries: &[&Entry]) -> Result<usize, RunError> {
        self.entries.save(entries)?;
        self.replay()
    }

    /// Discards the state and rebuilds it by running every entry in id order. The new state is
    /// committed at once, and only if every entry succeeds. Returns the number of entries run.
    pub fn replay(&mut self) -> Result<usize, RunError> {
        let last_id = self.entries.last_id()?;

        let mut base = Overlay::default();
        base.clear(&[]);
        self.store.data_mut().base = base;

        let mut count = 0;
        let mut next_id = 1;
        let result = loop {
            if next_id > last_id {
                break Ok(());
            }
            let batch_end = (next_id + REPLAY_BATCH).min(last_id + 1);
            let batch = self
                .entries
                .query(Query::ByIds((next_id..batch_end).collect()))?;
            next_id = batch_end;

            if let Err(err) = batch.iter().try_for_each(|entry| {
                self.execute(entry)
                    .map_err(|err| RunError::Replay(entry.id, Box::new(err)))
            }) {
                break Err(err);
            }
            count += batch.len();
        };

        let pending = std::mem::take(&mut self.store.data_mut().base);
        result?;
        self.commit_state(&pending)?;
        Ok(count)
    }

    fn commit_state(&self, pending: &Overlay) -> Result<(), RunError> {
        if pending.is_empty() {
            return Ok(());
        }
        let txn = self.state_db().start_write_txn()?;
        pending.apply(&txn)?;
        txn.commit()?;
        Ok(())
    }

    /// Runs the program on one entry and, if it succeeds, merges its state writes into `base`.
    fn execute(&mut self, entry: &Entry) -> Result<(), RunError> {
        self.store.data_mut().pending = Overlay::default();
        let result = self.execute_inner(entry);
        let data = self.store.data_mut();
        let pending = std::mem::take(&mut data.pending);
        if result.is_ok() {
            data.base.merge(pending);
        }
        result
    }

    fn execute_inner(&mut self, entry: &Entry) -> Result<(), RunError> {
        self.store.data_mut().result = Ok(());
        self.store
            .set_fuel(self.fuel_per_entry)
            .map_err(RunError::WasmtimeShenanigans)?;

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
        let entry_bytes = rmp_serde::to_vec(entry)?;
        let write_ptr = alloc_fn
            .call(&mut self.store, entry_bytes.len() as i32)
            .map_err(RunError::WasmtimeShenanigans)? as u32 as usize;
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
            .clone()
            .map_err(RunError::WasmCall)
    }
}

fn decode_integer(value: &[u8]) -> anyhow::Result<i128> {
    let val = rmpv::decode::read_value(&mut Cursor::new(value))
        .map_err(|_| anyhow::anyhow!("failed to decode value"))?;
    match val {
        rmpv::Value::Integer(n) => {
            if let Some(v) = n.as_i64() {
                Ok(v as i128)
            } else if let Some(v) = n.as_u64() {
                Ok(v as i128)
            } else {
                anyhow::bail!("integer out of range")
            }
        }
        rmpv::Value::Nil => Ok(0),
        _ => anyhow::bail!("expected integer"),
    }
}

fn encode_integer(value: i128) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let val = if (i64::MIN as i128) <= value && value <= (i64::MAX as i128) {
        rmpv::Value::Integer((value as i64).into())
    } else if value >= 0 && value <= (u64::MAX as i128) {
        rmpv::Value::Integer((value as u64).into())
    } else {
        anyhow::bail!("integer out of range");
    };

    rmpv::encode::write_value(&mut buf, &val)
        .map_err(|_| anyhow::anyhow!("failed to encode value"))?;
    Ok(buf)
}

#[derive(thiserror::Error, Debug)]
pub enum RunError {
    #[error("database open at {0}: {1}")]
    DatabaseOpen(String, String),

    #[error("database action: {0}")]
    DatabaseAction(#[from] anyhow::Error),

    #[error("wasmtime shenanigans: {0:#}")]
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

    #[error("replaying entry {0}: {1}")]
    Replay(u64, Box<RunError>),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StateMemDatabase;

    fn path(parts: &[KeyPart]) -> Vec<u8> {
        key::to_bytes(parts)
    }

    #[test]
    fn overlay_masks_cleared_rows() {
        let db = StateMemDatabase::init(std::path::Path::new("")).unwrap();
        {
            let txn = db.start_write_txn().unwrap();
            txn.put(&path(&[KeyPart::Field("a"), KeyPart::Field("x")]), b"1").unwrap();
            txn.put(&path(&[KeyPart::Field("a"), KeyPart::Field("y")]), b"2").unwrap();
            txn.put(&path(&[KeyPart::Field("b")]), b"3").unwrap();
        }

        let mut overlay = Overlay::default();
        overlay.clear(&path(&[KeyPart::Field("a")]));
        overlay.put(path(&[KeyPart::Field("a"), KeyPart::Field("z")]), b"4".to_vec());

        let read = db.start_read_txn().unwrap();
        let rows = overlay.scan(&[], || read.scan(&[])).unwrap();
        assert_eq!(
            rows,
            vec![
                (path(&[KeyPart::Field("a"), KeyPart::Field("z")]), b"4".to_vec()),
                (path(&[KeyPart::Field("b")]), b"3".to_vec()),
            ]
        );

        overlay.apply(&db.start_write_txn().unwrap()).unwrap();
        assert_eq!(db.start_read_txn().unwrap().scan(&[]).unwrap(), rows);
    }

    #[test]
    fn overlay_clear_subsumes_children() {
        let mut overlay = Overlay::default();
        overlay.clear(&path(&[KeyPart::Field("a"), KeyPart::Index(1)]));
        overlay.put(path(&[KeyPart::Field("a"), KeyPart::Index(1)]), b"x".to_vec());
        overlay.clear(&path(&[KeyPart::Field("a")]));
        assert_eq!(overlay.cleared.len(), 1);
        assert!(overlay.writes.is_empty());
        assert!(overlay.is_cleared(&path(&[KeyPart::Field("a"), KeyPart::Index(7)])));
        assert!(!overlay.is_cleared(&path(&[KeyPart::Field("ab")])));
    }

    #[test]
    fn overlay_merge_keeps_order() {
        let mut base = Overlay::default();
        base.put(path(&[KeyPart::Field("a"), KeyPart::Field("x")]), b"1".to_vec());
        base.put(path(&[KeyPart::Field("b")]), b"2".to_vec());

        let mut newer = Overlay::default();
        newer.clear(&path(&[KeyPart::Field("a")]));
        newer.put(path(&[KeyPart::Field("a"), KeyPart::Field("y")]), b"3".to_vec());
        base.merge(newer);

        let rows = base.scan(&[], || Ok(vec![])).unwrap();
        assert_eq!(
            rows,
            vec![
                (path(&[KeyPart::Field("a"), KeyPart::Field("y")]), b"3".to_vec()),
                (path(&[KeyPart::Field("b")]), b"2".to_vec()),
            ]
        );
    }
}
