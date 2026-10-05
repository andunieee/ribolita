use crate::store::{ReadTxn, Row, Store, Table, WriteTxn};
use crate::sync::IdSource;
use log::trace;
use ribolita_common::entry::{Author, Entry, EntryId};
use ribolita_common::key::{self, KeyPart};
use ribolita_common::state::State;
use ribolita_common::{Opcode, ROW_COUNT_BYTES, rmp_json, value};
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::io::Cursor;
use std::sync::Arc;
use wasmtime::{Caller, Config, Engine, Linker, Module, StoreLimits, StoreLimitsBuilder};

/// Instructions (roughly) a single entry may execute before it's aborted.
pub const DEFAULT_FUEL_PER_ENTRY: u64 = 1_000_000_000;
/// Maximum linear memory a program instance may grow to.
pub const DEFAULT_MAX_MEMORY: usize = 256 * 1024 * 1024;
/// Entries applied between state snapshots.
pub const DEFAULT_SNAPSHOT_INTERVAL: u64 = 1000;
/// Snapshots kept; inserting an entry older than all of them replays the whole log.
pub const DEFAULT_MAX_SNAPSHOTS: usize = 8;

const REQUIRED_EXPORTS: &[&str] = &["memory", "malloc", "init", "process"];
const BATCH: usize = 1024;
/// Entries applied per transaction while catching up, which bounds the memory pending writes use.
const COMMIT_EVERY: usize = 4096;

/// order key of the last entry the state reflects
const META_TIP: &[u8] = b"tip";
/// order key of the oldest stored entry at or before the tip that isn't reflected correctly in
/// the state (because it arrived late, or was deleted)
const META_DIRTY: &[u8] = b"dirty";
/// entries applied since the last snapshot
const META_SINCE_SNAPSHOT: &[u8] = b"since_snapshot";
/// `snapshot:` ++ order key: a snapshot of the state right after that entry
const META_SNAPSHOT: &[u8] = b"snapshot:";
/// `deleted:` ++ entry id => id of the tombstone that deletes it
const META_DELETED: &[u8] = b"deleted:";

/// Where an entry goes in the order entries are applied in: by `when`, then by id.
fn order_key(when: u64, id: &EntryId) -> Vec<u8> {
    [&when.to_be_bytes()[..], id.as_bytes()].concat()
}

fn entry_order_key(entry: &Entry) -> Vec<u8> {
    order_key(entry.when, &entry.id)
}

/// Owns a program's store and its compiled WASM module, and turns its log of entries into state.
///
/// The state is a function of the entries in the order of when they happened (`when`, then id
/// to break ties). Entries can arrive in any order, e.g. from other devices or backdated: those
/// that go before the newest applied one roll the state back to a snapshot and replay from
/// there. Entries are immutable: deleting one adds a tombstone entry, and editing is deleting
/// plus adding a replacement.
pub struct RunEnvironment<S: Store + 'static> {
    engine: Engine,
    module: Module,
    linker: Linker<WasmData<S>>,
    db: Arc<S>,
    /// writes of entries already applied in this batch, not yet committed
    base: Overlay,
    author: Author,
    pub fuel_per_entry: u64,
    pub snapshot_interval: u64,
    pub max_snapshots: usize,
}

impl<S: Store + 'static> std::fmt::Debug for RunEnvironment<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<RunEnvironment {:?}>", self.author)
    }
}

/// What a program instance can reach while processing one entry. Each entry gets a fresh
/// wasmtime store (instances are only freed with their store), and `base` is lent to it.
struct WasmData<S> {
    result: Result<(), String>,
    /// set when a host function failed for reasons that aren't the program's fault
    host_error: Option<String>,
    db: Arc<S>,
    /// writes of entries already applied in this batch, not yet committed
    base: Overlay,
    /// writes of the entry being processed, merged into `base` if it succeeds
    pending: Overlay,
    limits: StoreLimits,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ApplyReport {
    /// entries run through the program (including those that failed)
    pub applied: usize,
    /// entries the program rejected: they're kept in the log but have no effect on the state
    pub failed: Vec<(EntryId, String)>,
}

#[derive(Debug)]
pub enum Query {
    ByIds(Vec<EntryId>),
    /// entries that happened at `since <= when < until` (seconds), in the order they're applied.
    /// With a `limit`, the oldest ones if `since` is given, the newest ones otherwise.
    ByTime {
        since: Option<u64>,
        until: Option<u64>,
        limit: Option<usize>,
    },
}

impl Query {
    pub const ALL: Query = Query::ByTime {
        since: None,
        until: None,
        limit: None,
    };
}

/// State writes made while processing entries, not yet committed to the store.
///
/// Writes replace whole subtrees (setting `a.b` drops everything that was under `a.b`), which is
/// modeled as a set of cleared prefixes plus the rows written after them.
#[derive(Default)]
struct Overlay {
    cleared: BTreeSet<Vec<u8>>,
    writes: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl Overlay {
    /// whether `key` (or a parent of it) was cleared. Keys are self-delimiting, so any byte prefix
    /// that matches a cleared path is a path prefix.
    fn is_cleared(&self, key: &[u8]) -> bool {
        (0..=key.len()).any(|i| self.cleared.contains(&key[..i]))
    }

    fn clear(&mut self, prefix: &[u8]) {
        let cleared: Vec<Vec<u8>> = self
            .cleared
            .range(prefix.to_vec()..)
            .take_while(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        for key in cleared {
            self.cleared.remove(&key);
        }
        let written: Vec<Vec<u8>> = self
            .writes
            .range(prefix.to_vec()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, _)| k.clone())
            .collect();
        for key in written {
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
    fn scan(
        &self,
        prefix: &[u8],
        lower: impl FnOnce() -> anyhow::Result<Vec<Row>>,
    ) -> anyhow::Result<Vec<Row>> {
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

    fn apply(&self, txn: &impl WriteTxn) -> anyhow::Result<()> {
        for prefix in &self.cleared {
            txn.delete_prefix(Table::State, prefix)?;
        }
        for (key, value) in &self.writes {
            txn.put(Table::State, key, value)?;
        }
        Ok(())
    }
}

/// Marks errors that come from the host (the store), as opposed to the program.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct HostFailure(String);

/// Copies `len` bytes at `ptr` out of the guest's memory, trapping on out-of-bounds access.
fn guest_bytes<S>(
    caller: &mut Caller<'_, WasmData<S>>,
    ptr: i32,
    len: i32,
) -> wasmtime::Result<Vec<u8>> {
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

/// Records host failures so they abort processing instead of counting as a program error.
fn note_host_failure<S>(
    caller: &mut Caller<'_, WasmData<S>>,
    err: anyhow::Error,
) -> wasmtime::Error {
    if let Some(failure) = err.downcast_ref::<HostFailure>() {
        caller.data_mut().host_error = Some(failure.0.clone());
    }
    err
}

/// State rows under `prefix`, with keys relative to it, as the program currently sees them.
fn scan_relative<S: Store>(data: &WasmData<S>, prefix: &[u8]) -> anyhow::Result<Vec<Row>> {
    let mut rows = data.pending.scan(prefix, || {
        data.base.scan(prefix, || {
            let read = || data.db.read()?.scan(Table::State, prefix);
            read().map_err(|e: anyhow::Error| HostFailure(format!("{e:#}")).into())
        })
    })?;
    for (key, _) in rows.iter_mut() {
        key.drain(..prefix.len());
    }
    Ok(rows)
}

fn state_modify<S: Store>(
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

fn build_linker<S: Store + 'static>(engine: &Engine) -> Result<Linker<WasmData<S>>, RunError> {
    let mut linker = Linker::new(engine);

    linker
        .func_wrap(
            "env",
            "state_get_internal",
            |mut caller: Caller<'_, WasmData<S>>, ptr: i32, len: i32| -> wasmtime::Result<i32> {
                let prefix = guest_bytes(&mut caller, ptr, len)?;
                let rows = scan_relative(caller.data(), &prefix)
                    .map_err(|e| note_host_failure(&mut caller, e))?;
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
                state_modify(caller.data_mut(), &path, &rows_blob, opcode)
                    .map_err(|e| note_host_failure(&mut caller, e))?;
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
    let module = Module::new(engine, wasm_module).map_err(RunError::Wasm)?;
    let exports: BTreeSet<&str> = module.exports().map(|e| e.name()).collect();
    for name in REQUIRED_EXPORTS {
        if !exports.contains(name) {
            return Err(RunError::BadWasm(format!("missing export '{name}'")));
        }
    }
    Ok(module)
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn decode_entry(bytes: &[u8]) -> Result<Entry, RunError> {
    Ok(rmp_serde::from_slice(bytes)?)
}

fn deleted_key(id: &EntryId) -> Vec<u8> {
    [META_DELETED, id.as_bytes()].concat()
}

fn snapshot_keys(txn: &impl ReadTxn) -> anyhow::Result<Vec<Vec<u8>>> {
    Ok(txn
        .scan(Table::Meta, META_SNAPSHOT)?
        .into_iter()
        .map(|(k, _)| k[META_SNAPSHOT.len()..].to_vec())
        .collect())
}

/// Snapshot rows are keyed by the length-prefixed order key, then the state path.
fn snapshot_prefix(order: &[u8]) -> Vec<u8> {
    [&(order.len() as u16).to_be_bytes()[..], order].concat()
}

fn delete_snapshot(txn: &impl WriteTxn, order: &[u8]) -> anyhow::Result<()> {
    txn.delete(Table::Meta, &[META_SNAPSHOT, order].concat())?;
    txn.delete_prefix(Table::Snapshots, &snapshot_prefix(order))
}

fn lower_dirty(dirty: &mut Option<Vec<u8>>, key: Vec<u8>) {
    if dirty.as_ref().is_none_or(|d| key < *d) {
        *dirty = Some(key);
    }
}

/// Where a processing run starts from.
struct Start {
    /// the state is the one right after the entry with this order key (None: before the first)
    after: Option<Vec<u8>>,
    /// the overlay that turns the stored state into the starting state
    base: Overlay,
    since_snapshot: u64,
    /// snapshots at or after this order key are outdated
    stale_from: Option<Vec<u8>>,
}

/// Work done by a processing run that's not committed yet.
#[derive(Default)]
struct Progress {
    last: Option<Vec<u8>>,
    since_snapshot: u64,
    snapshots: Vec<(Vec<u8>, Vec<Row>)>,
    stale_from: Option<Vec<u8>>,
    uncommitted: usize,
}

impl<S: Store + 'static> RunEnvironment<S> {
    /// Opens the store in `db_path`. Entries created here are attributed to `author`.
    pub fn new(
        db_path: &std::path::Path,
        wasm_module: &[u8],
        author: Author,
    ) -> Result<Self, RunError> {
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).map_err(RunError::Wasm)?;
        let module = compile(&engine, wasm_module)?;
        let db = Arc::new(S::open(db_path).map_err(RunError::Store)?);
        let linker = build_linker(&engine)?;

        Ok(RunEnvironment {
            engine,
            module,
            linker,
            db,
            base: Overlay::default(),
            author,
            fuel_per_entry: DEFAULT_FUEL_PER_ENTRY,
            snapshot_interval: DEFAULT_SNAPSHOT_INTERVAL,
            max_snapshots: DEFAULT_MAX_SNAPSHOTS,
        })
    }

    pub fn author(&self) -> Author {
        self.author
    }

    pub fn db(&self) -> &S {
        &self.db
    }

    /// Swaps the program. Existing state is left as is, call [`Self::replay`] to rebuild it.
    pub fn set_module(&mut self, wasm_module: &[u8]) -> Result<(), RunError> {
        self.module = compile(&self.engine, wasm_module)?;
        Ok(())
    }

    /// Reads the state under a dotted path (like `"items.3"`) as JSON. Missing paths are `null`.
    pub fn state_json(&self, path: &str) -> anyhow::Result<serde_json::Value> {
        let prefix = key::to_bytes(&key::parse(path)?);
        let rows: Vec<Row> = self
            .db()
            .read()?
            .scan(Table::State, &prefix)?
            .into_iter()
            .map(|(k, v)| (k[prefix.len()..].to_vec(), v))
            .collect();
        if rows.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        let state: State = value::from_bytes(&value::join_rows(&rows))?;
        Ok(rmp_json::rmp_value_to_json_value(&state.0))
    }

    /// The entry with this id, including tombstones and deleted entries.
    pub fn get_entry(&self, id: &EntryId) -> Result<Option<Entry>, RunError> {
        let bytes = self.db().read()?.get(Table::Entries, id.as_bytes())?;
        bytes.map(|b| decode_entry(&b)).transpose()
    }

    pub fn is_deleted(&self, id: &EntryId) -> Result<bool, RunError> {
        Ok(self
            .db()
            .read()?
            .get(Table::Meta, &deleted_key(id))?
            .is_some())
    }

    /// Live entries matching `query`, in the order they're applied in. Tombstones and deleted
    /// entries are left out.
    pub fn entries(&self, query: Query) -> Result<Vec<Entry>, RunError> {
        let txn = self.db().read()?;
        let live = |entry: &Entry| -> Result<bool, RunError> {
            Ok(entry.deletes.is_none() && txn.get(Table::Meta, &deleted_key(&entry.id))?.is_none())
        };
        let mut entries = Vec::new();
        match query {
            Query::ByIds(ids) => {
                for id in ids {
                    if let Some(bytes) = txn.get(Table::Entries, id.as_bytes())? {
                        let entry = decode_entry(&bytes)?;
                        if live(&entry)? {
                            entries.push(entry);
                        }
                    }
                }
            }
            Query::ByTime {
                since,
                until,
                limit,
            } => {
                let lower = since.unwrap_or(0).to_be_bytes();
                let upper = until.map_or([0xff; 9].to_vec(), |u| u.to_be_bytes().to_vec());
                for (_, id) in txn.range(Table::Order, &lower, &upper, usize::MAX)? {
                    let bytes = txn
                        .get(Table::Entries, &id)?
                        .ok_or_else(|| anyhow::anyhow!("order index points to a missing entry"))?;
                    let entry = decode_entry(&bytes)?;
                    if live(&entry)? {
                        entries.push(entry);
                    }
                }
                if let Some(limit) = limit {
                    if since.is_some() {
                        entries.truncate(limit);
                    } else {
                        entries.drain(..entries.len().saturating_sub(limit));
                    }
                }
            }
        }
        Ok(entries)
    }

    /// Creates an entry authored here, runs it and stores it. Nothing is stored if the program
    /// rejects it. `when` (seconds) is when the event happened, and defaults to now.
    pub fn add(
        &mut self,
        kind: String,
        values: Vec<(String, rmpv::Value)>,
        when: Option<u64>,
    ) -> Result<Entry, RunError> {
        Ok(self.add_with_report(kind, values, when)?.0)
    }

    /// Like [`Self::add`], also reporting the entries that were replayed after the new one
    /// because it happened before the newest entry. Some may have been rejected by the program
    /// in their new context (e.g. they referred to something the new entry changed).
    pub fn add_with_report(
        &mut self,
        kind: String,
        values: Vec<(String, rmpv::Value)>,
        when: Option<u64>,
    ) -> Result<(Entry, ApplyReport), RunError> {
        let entry = self.new_entry(kind, values, when.unwrap_or_else(now_secs), None);
        let report = self.process(std::slice::from_ref(&entry))?;
        Ok((entry, report))
    }

    /// Replaces an entry: deletes it and adds a new one with the given contents, as one change
    /// that's rejected as a whole if the program rejects the replacement. `when` defaults to
    /// the replaced entry's.
    pub fn edit(
        &mut self,
        id: &EntryId,
        when: Option<u64>,
        kind: String,
        values: Vec<(String, rmpv::Value)>,
    ) -> Result<(Entry, ApplyReport), RunError> {
        let old = self.live_entry(id)?;
        let tombstone = self.tombstone(id);
        let replacement = self.new_entry(kind, values, when.unwrap_or(old.when), None);
        let report = self.process(&[tombstone, replacement.clone()])?;
        Ok((replacement, report))
    }

    /// Deletes an entry by adding a tombstone for it.
    pub fn delete(&mut self, id: &EntryId) -> Result<ApplyReport, RunError> {
        self.live_entry(id)?;
        let tombstone = self.tombstone(id);
        self.process(&[tombstone])
    }

    fn live_entry(&self, id: &EntryId) -> Result<Entry, RunError> {
        match self.get_entry(id)? {
            Some(entry) if entry.deletes.is_none() && !self.is_deleted(id)? => Ok(entry),
            _ => Err(RunError::NotFound(id.clone())),
        }
    }

    fn new_entry(
        &self,
        kind: String,
        values: Vec<(String, rmpv::Value)>,
        when: u64,
        deletes: Option<EntryId>,
    ) -> Entry {
        Entry {
            id: EntryId::generate(),
            when,
            author: self.author,
            kind,
            values,
            deletes,
        }
    }

    fn tombstone(&self, target: &EntryId) -> Entry {
        self.new_entry(String::new(), Vec::new(), now_secs(), Some(target.clone()))
    }

    /// Stores entries (e.g. received from a peer) without running them; call
    /// [`Self::catch_up`] to apply them. Returns the ids of the entries that were new.
    pub fn store_entries(&mut self, entries: Vec<Entry>) -> Result<Vec<EntryId>, RunError> {
        let txn = self.db().write()?;
        let tip = txn.get(Table::Meta, META_TIP)?;
        let mut dirty = txn.get(Table::Meta, META_DIRTY)?;
        let mut accepted = Vec::new();
        for entry in entries {
            if txn.get(Table::Entries, entry.id.as_bytes())?.is_some() {
                continue;
            }
            if let Some(key) = Self::insert(&txn, &entry)?
                && tip.as_ref().is_some_and(|tip| key <= *tip)
            {
                lower_dirty(&mut dirty, key);
            }
            accepted.push(entry.id);
        }
        if let Some(dirty) = dirty {
            txn.put(Table::Meta, META_DIRTY, &dirty)?;
        }
        txn.commit()?;
        Ok(accepted)
    }

    /// Writes a new entry and its indexes. Returns the order key from which the state has to
    /// be recomputed because of it, if any.
    fn insert(txn: &impl WriteTxn, entry: &Entry) -> Result<Option<Vec<u8>>, RunError> {
        txn.put(
            Table::Entries,
            entry.id.as_bytes(),
            &rmp_serde::to_vec(entry)?,
        )?;
        if let Some(target) = &entry.deletes {
            txn.put(Table::Meta, &deleted_key(target), entry.id.as_bytes())?;
            let target = txn.get(Table::Entries, target.as_bytes())?;
            return target
                .map(|bytes| Ok(entry_order_key(&decode_entry(&bytes)?)))
                .transpose();
        }
        let key = entry_order_key(entry);
        txn.put(Table::Order, &key, entry.id.as_bytes())?;
        if txn.get(Table::Meta, &deleted_key(&entry.id))?.is_some() {
            // its tombstone arrived first
            return Ok(None);
        }
        Ok(Some(key))
    }

    /// Stored entries, as sync peers exchange them.
    pub fn entries_for_sync(&self, ids: &[EntryId]) -> Result<Vec<Entry>, RunError> {
        let txn = self.db().read()?;
        let mut entries = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(bytes) = txn.get(Table::Entries, id.as_bytes())? {
                entries.push(decode_entry(&bytes)?);
            }
        }
        Ok(entries)
    }

    /// Applies stored entries the state doesn't reflect yet.
    pub fn catch_up(&mut self) -> Result<ApplyReport, RunError> {
        self.process(&[])
    }

    /// Discards the state and all snapshots and rebuilds them from the whole log, e.g. after
    /// the module changed.
    pub fn replay(&mut self) -> Result<ApplyReport, RunError> {
        let txn = self.db().write()?;
        for key in snapshot_keys(&txn)? {
            delete_snapshot(&txn, &key)?;
        }
        txn.put(Table::Meta, META_DIRTY, &[])?;
        txn.commit()?;
        self.catch_up()
    }

    /// Figures out where processing has to start: right after the tip, or, if entries arrived
    /// (or `new` ones go) before it, from the latest snapshot before them.
    fn start(&self, new: &[Entry]) -> Result<Start, RunError> {
        let txn = self.db().read()?;
        let tip = txn.get(Table::Meta, META_TIP)?;
        let mut from = txn.get(Table::Meta, META_DIRTY)?;
        for entry in new {
            let key = match &entry.deletes {
                Some(target) => match txn.get(Table::Entries, target.as_bytes())? {
                    Some(bytes) => entry_order_key(&decode_entry(&bytes)?),
                    None => continue,
                },
                None => entry_order_key(entry),
            };
            if tip.as_ref().is_some_and(|tip| key <= *tip) {
                lower_dirty(&mut from, key);
            }
        }

        let Some(from) = from else {
            let since_snapshot = txn
                .get(Table::Meta, META_SINCE_SNAPSHOT)?
                .and_then(|b| Some(u64::from_le_bytes(b.try_into().ok()?)))
                .unwrap_or(0);
            return Ok(Start {
                after: tip,
                base: Overlay::default(),
                since_snapshot,
                stale_from: None,
            });
        };

        // roll back: start from the newest snapshot before `from`, or from nothing
        let mut base = Overlay::default();
        base.clear(&[]);
        let snapshot = snapshot_keys(&txn)?.into_iter().filter(|k| *k < from).max();
        if let Some(snapshot) = &snapshot {
            let prefix = snapshot_prefix(snapshot);
            for (key, value) in txn.scan(Table::Snapshots, &prefix)? {
                base.put(key[prefix.len()..].to_vec(), value);
            }
        }
        trace!("rolling back to {snapshot:?} to apply entries from {from:?}");
        Ok(Start {
            after: snapshot,
            base,
            since_snapshot: 0,
            stale_from: Some(from),
        })
    }

    /// Stored entries ordered after `after` (exclusive), with their order keys.
    fn entries_after(
        &self,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Entry)>, RunError> {
        let txn = self.db().read()?;
        let lower = after.unwrap_or(&[]);
        let rows = txn.range(Table::Order, lower, &[0xff; 9], limit + 1)?;
        rows.into_iter()
            .filter(|(key, _)| after.is_none() || key[..] != *lower)
            .take(limit)
            .map(|(key, id)| {
                let bytes = txn
                    .get(Table::Entries, &id)?
                    .ok_or_else(|| anyhow::anyhow!("order index points to a missing entry"))?;
                Ok((key, decode_entry(&bytes)?))
            })
            .collect()
    }

    /// Runs every entry the state doesn't reflect through the program, in order, and commits
    /// the result. `new` entries aren't stored yet: if the program rejects any of them, nothing
    /// is committed; otherwise they're stored with the rest. Stored entries the program rejects
    /// are no-ops, as they are on every peer.
    fn process(&mut self, new: &[Entry]) -> Result<ApplyReport, RunError> {
        let start = self.start(new)?;
        self.base = start.base;
        let mut progress = Progress {
            last: start.after,
            since_snapshot: start.since_snapshot,
            stale_from: start.stale_from,
            ..Progress::default()
        };
        let mut report = ApplyReport::default();
        let result = self.run_entries(new, &mut progress, &mut report);
        let result = result.and_then(|()| self.commit(&mut progress, new));
        self.base = Overlay::default();
        result.map(|()| report)
    }

    fn run_entries(
        &mut self,
        new: &[Entry],
        progress: &mut Progress,
        report: &mut ApplyReport,
    ) -> Result<(), RunError> {
        let deleted_by_new: HashSet<&EntryId> =
            new.iter().filter_map(|e| e.deletes.as_ref()).collect();
        let new_ids: HashSet<&EntryId> = new.iter().map(|e| &e.id).collect();
        let mut pending: Vec<(Vec<u8>, Entry)> = new
            .iter()
            .filter(|e| e.deletes.is_none())
            .map(|e| (entry_order_key(e), e.clone()))
            .collect();
        pending.sort_by(|a, b| a.0.cmp(&b.0));
        let mut pending = VecDeque::from(pending);

        loop {
            let mut batch = self.entries_after(progress.last.as_deref(), BATCH)?;
            let at_end = batch.len() < BATCH;
            // merge in the new entries that go within this batch
            while let Some((key, _)) = pending.front()
                && (at_end || batch.last().is_some_and(|(last, _)| key < last))
            {
                let item = pending.pop_front().unwrap();
                let position = batch.partition_point(|(k, _)| *k < item.0);
                batch.insert(position, item);
            }

            for (key, entry) in batch {
                let deleted = deleted_by_new.contains(&entry.id) || {
                    let txn = self.db().read()?;
                    txn.get(Table::Meta, &deleted_key(&entry.id))?.is_some()
                };
                if !deleted && let Err(err) = self.execute(&entry)? {
                    if new_ids.contains(&entry.id) {
                        return Err(RunError::Rejected(err));
                    }
                    report.failed.push((entry.id.clone(), err));
                }
                report.applied += 1;
                progress.since_snapshot += 1;
                progress.uncommitted += 1;

                if progress.since_snapshot >= self.snapshot_interval {
                    let rows = self.current_state()?;
                    progress.snapshots.push((key.clone(), rows));
                    let excess = progress.snapshots.len().saturating_sub(self.max_snapshots);
                    progress.snapshots.drain(..excess);
                    progress.since_snapshot = 0;
                }
                progress.last = Some(key);
            }

            if at_end && pending.is_empty() {
                return Ok(());
            }
            if new.is_empty() && progress.uncommitted >= COMMIT_EVERY {
                self.commit(progress, &[])?;
            }
        }
    }

    /// The full state as seen by the program right now (stored state plus pending writes).
    fn current_state(&self) -> Result<Vec<Row>, RunError> {
        Ok(self
            .base
            .scan(&[], || self.db.read()?.scan(Table::State, &[]))?)
    }

    fn commit(&mut self, progress: &mut Progress, new: &[Entry]) -> Result<(), RunError> {
        let base = std::mem::take(&mut self.base);
        let txn = self.db().write()?;

        if let Some(from) = progress.stale_from.take() {
            for key in snapshot_keys(&txn)?.into_iter().filter(|k| *k >= from) {
                delete_snapshot(&txn, &key)?;
            }
        }
        base.apply(&txn)?;
        for (order, rows) in progress.snapshots.drain(..) {
            txn.put(Table::Meta, &[META_SNAPSHOT, &order].concat(), &[])?;
            let prefix = snapshot_prefix(&order);
            for (key, value) in rows {
                txn.put(Table::Snapshots, &[&prefix[..], &key].concat(), &value)?;
            }
        }
        let snapshots = snapshot_keys(&txn)?;
        for key in &snapshots[..snapshots.len().saturating_sub(self.max_snapshots)] {
            delete_snapshot(&txn, key)?;
        }

        match &progress.last {
            Some(last) => txn.put(Table::Meta, META_TIP, last)?,
            None => txn.delete(Table::Meta, META_TIP)?,
        }
        txn.put(
            Table::Meta,
            META_SINCE_SNAPSHOT,
            &progress.since_snapshot.to_le_bytes(),
        )?;
        txn.delete(Table::Meta, META_DIRTY)?;
        for entry in new {
            Self::insert(&txn, entry)?;
        }
        txn.commit()?;
        progress.uncommitted = 0;
        Ok(())
    }

    /// Runs the program on one entry and, if it succeeds, merges its state writes into `base`.
    /// The outer error is a host failure that must abort processing; the inner one means the
    /// program rejected the entry, which every peer will agree on.
    fn execute(&mut self, entry: &Entry) -> Result<Result<(), String>, RunError> {
        let mut wasm = wasmtime::Store::new(
            &self.engine,
            WasmData {
                result: Ok(()),
                host_error: None,
                db: self.db.clone(),
                base: std::mem::take(&mut self.base),
                pending: Overlay::default(),
                limits: StoreLimitsBuilder::new()
                    .memory_size(DEFAULT_MAX_MEMORY)
                    .build(),
            },
        );
        wasm.limiter(|data| &mut data.limits);
        let result = self.execute_in(&mut wasm, entry);

        let mut data = wasm.into_data();
        self.base = data.base;
        if let Some(err) = data.host_error.take() {
            return Err(RunError::Store(anyhow::anyhow!(err)));
        }
        Ok(match result {
            Ok(()) => {
                self.base.merge(data.pending);
                Ok(())
            }
            Err(err) => Err(format!("{err:#}")),
        })
    }

    fn execute_in(
        &self,
        wasm: &mut wasmtime::Store<WasmData<S>>,
        entry: &Entry,
    ) -> anyhow::Result<()> {
        wasm.set_fuel(self.fuel_per_entry)?;
        let instance = self.linker.instantiate(&mut *wasm, &self.module)?;
        instance
            .get_typed_func::<(), ()>(&mut *wasm, "init")?
            .call(&mut *wasm, ())?;

        // write entry
        let alloc_fn = instance.get_typed_func::<i32, i32>(&mut *wasm, "malloc")?;
        let mem = instance
            .get_memory(&mut *wasm, "memory")
            .ok_or_else(|| anyhow::anyhow!("wasm doesn't export 'memory'"))?;
        let entry_bytes = rmp_serde::to_vec(entry)?;
        let write_ptr = alloc_fn.call(&mut *wasm, entry_bytes.len() as i32)? as u32 as usize;
        mem.write(&mut *wasm, write_ptr, &entry_bytes)?;

        instance
            .get_typed_func::<(i32, i32), ()>(&mut *wasm, "process")?
            .call(&mut *wasm, (write_ptr as i32, entry_bytes.len() as i32))?;

        wasm.data().result.clone().map_err(anyhow::Error::msg)
    }
}

impl<S: Store + 'static> IdSource for RunEnvironment<S> {
    fn ids(&self, lower: &str, upper: &str) -> anyhow::Result<Vec<EntryId>> {
        let rows = self.db().read()?.range(
            Table::Entries,
            lower.as_bytes(),
            upper.as_bytes(),
            usize::MAX,
        )?;
        rows.into_iter()
            .map(|(key, _)| {
                EntryId::from_bytes(&key).ok_or_else(|| anyhow::anyhow!("invalid entry key"))
            })
            .collect()
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
    #[error("store: {0:#}")]
    Store(#[from] anyhow::Error),

    #[error("wasm: {0:#}")]
    Wasm(wasmtime::Error),

    #[error("error linking: {0}")]
    Linking(wasmtime::Error),

    #[error("bad wasm code: {0}")]
    BadWasm(String),

    #[error("program rejected the entry: {0}")]
    Rejected(String),

    #[error("entry {0} not found")]
    NotFound(EntryId),

    #[error("failed to encode to messagepack: {0}")]
    MessagePackEncode(#[from] rmp_serde::encode::Error),

    #[error("failed to decode from messagepack: {0}")]
    MessagePackDecode(#[from] rmp_serde::decode::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemStore, testing::COUNTER_WAT};

    fn path(parts: &[KeyPart]) -> Vec<u8> {
        key::to_bytes(parts)
    }

    fn env(author: u8) -> RunEnvironment<MemStore> {
        let mut env = RunEnvironment::new(
            std::path::Path::new(""),
            COUNTER_WAT.as_bytes(),
            Author([author; 32]),
        )
        .unwrap();
        env.snapshot_interval = 3;
        env.max_snapshots = 3;
        env
    }

    /// an entry created elsewhere, as it would arrive from a peer
    fn remote(when: u64, kind: &str) -> Entry {
        Entry {
            id: EntryId::generate(),
            when,
            author: Author([2; 32]),
            kind: kind.into(),
            values: vec![],
            deletes: None,
        }
    }

    fn remote_tombstone(target: &Entry) -> Entry {
        Entry {
            deletes: Some(target.id.clone()),
            ..remote(0, "")
        }
    }

    /// the counter program counts entries and records the encoded size of the last one
    fn size_of(entry: &Entry) -> u64 {
        rmp_serde::to_vec(entry).unwrap().len() as u64
    }

    fn state<S: Store>(env: &RunEnvironment<S>) -> serde_json::Value {
        env.state_json("").unwrap()
    }

    #[test]
    fn applies_in_event_order_whatever_the_arrival_order() {
        // created in this order, but they happened in a different one
        let entries: Vec<Entry> = (0..20)
            .map(|i| remote(1000 + (i * 7) % 20, &"k".repeat(1 + (i as usize % 5))))
            .collect();
        let last_event = entries
            .iter()
            .max_by_key(|e| (e.when, e.id.clone()))
            .unwrap();
        let expected = serde_json::json!({ "c": 20, "last": size_of(last_event) });

        let mut in_order = env(9);
        in_order.store_entries(entries.clone()).unwrap();
        assert_eq!(in_order.catch_up().unwrap().applied, 20);
        assert_eq!(state(&in_order), expected);

        // one at a time, newest first, catching up after each: every arrival rolls back
        let mut reversed = env(9);
        for e in entries.iter().rev() {
            reversed.store_entries(vec![e.clone()]).unwrap();
            reversed.catch_up().unwrap();
        }
        assert_eq!(state(&reversed), expected);

        // shuffled batches
        let mut shuffled = env(9);
        for chunk in [
            &entries[10..15],
            &entries[0..3],
            &entries[15..20],
            &entries[3..10],
        ] {
            shuffled.store_entries(chunk.to_vec()).unwrap();
            shuffled.catch_up().unwrap();
        }
        assert_eq!(state(&shuffled), expected);

        shuffled.replay().unwrap();
        assert_eq!(state(&shuffled), expected);

        let whens: Vec<u64> = shuffled
            .entries(Query::ALL)
            .unwrap()
            .iter()
            .map(|e| e.when)
            .collect();
        assert!(whens.is_sorted());
    }

    /// the same out-of-order scenario on the on-disk backends, reopening the store halfway
    fn disk_backend_scenario<S: Store + 'static>() {
        let dir = tempfile::tempdir().unwrap();
        let open = || {
            let mut env =
                RunEnvironment::<S>::new(dir.path(), COUNTER_WAT.as_bytes(), Author([1; 32]))
                    .unwrap();
            env.snapshot_interval = 2;
            env
        };
        let entries: Vec<Entry> = (0..9)
            .map(|i| remote(10 + i, &"k".repeat(1 + i as usize)))
            .collect();
        {
            let mut env = open();
            env.store_entries(entries[4..].to_vec()).unwrap();
            env.catch_up().unwrap();
        }
        let mut env = open();
        env.store_entries(entries[..4].to_vec()).unwrap();
        env.catch_up().unwrap();
        assert_eq!(
            state(&env),
            serde_json::json!({ "c": 9, "last": size_of(&entries[8]) })
        );
        env.delete(&entries[8].id).unwrap();
        assert_eq!(
            state(&env),
            serde_json::json!({ "c": 8, "last": size_of(&entries[7]) })
        );
    }

    #[cfg(feature = "backend-redb")]
    #[test]
    fn redb_backend() {
        disk_backend_scenario::<crate::RedbStore>();
    }

    #[cfg(feature = "backend-libmdbx")]
    #[test]
    fn mdbx_backend() {
        disk_backend_scenario::<crate::MdbxStore>();
    }

    #[test]
    fn rejected_local_entries_are_not_stored() {
        let mut env = env(1);
        env.add("short".into(), vec![], Some(10)).unwrap();
        let err = env.add("x".repeat(80), vec![], Some(20)).unwrap_err();
        assert!(matches!(err, RunError::Rejected(_)), "{err}");
        // also when it would have to be inserted before the tip
        assert!(env.add("x".repeat(80), vec![], Some(5)).is_err());
        assert_eq!(env.entries(Query::ALL).unwrap().len(), 1);
        assert_eq!(state(&env)["c"], 1);
    }

    #[test]
    fn rejected_remote_entries_are_no_ops() {
        let mut env = env(1);
        let bad = remote(5, &"x".repeat(80));
        env.store_entries(vec![remote(1, "a"), bad.clone(), remote(9, "b")])
            .unwrap();
        let report = env.catch_up().unwrap();
        assert_eq!(report.applied, 3);
        assert_eq!(report.failed, vec![(bad.id, "too long".to_string())]);
        assert_eq!(state(&env)["c"], 2);
    }

    #[test]
    fn backdated_entries_report_what_they_break() {
        let mut env = env(1);
        env.add("a".into(), vec![], Some(100)).unwrap();
        let (_, report) = env.add_with_report("b".into(), vec![], Some(50)).unwrap();
        assert_eq!(report.applied, 2);
        assert!(report.failed.is_empty());
    }

    #[test]
    fn edits_and_deletes() {
        let mut env = env(1);
        let a = env.add("a".into(), vec![], Some(100)).unwrap();
        let b = env.add("bb".into(), vec![], Some(200)).unwrap();
        assert_eq!(state(&env)["last"], size_of(&b));

        // editing `a` to have happened after `b` makes it the last one
        let (edited, _) = env.edit(&a.id, Some(300), "aaa".into(), vec![]).unwrap();
        let entries = env.entries(Query::ALL).unwrap();
        assert_eq!(entries, vec![b.clone(), edited.clone()]);
        assert_eq!(
            state(&env),
            serde_json::json!({ "c": 2, "last": size_of(&edited) })
        );
        assert!(env.is_deleted(&a.id).unwrap());
        assert!(matches!(env.delete(&a.id), Err(RunError::NotFound(_))));

        // editing keeps the original time by default
        let (b2, _) = env.edit(&b.id, None, "b2".into(), vec![]).unwrap();
        assert_eq!(b2.when, 200);

        // a rejected replacement leaves the original in place
        assert!(env.edit(&b2.id, None, "x".repeat(80), vec![]).is_err());
        assert!(!env.is_deleted(&b2.id).unwrap());

        env.delete(&edited.id).unwrap();
        assert_eq!(
            state(&env),
            serde_json::json!({ "c": 1, "last": size_of(&b2) })
        );
    }

    #[test]
    fn tombstones_can_arrive_before_their_target() {
        let mut env = env(1);
        let a = remote(10, "a");
        let b = remote(20, "bb");
        env.store_entries(vec![remote_tombstone(&b), a.clone()])
            .unwrap();
        env.catch_up().unwrap();
        env.store_entries(vec![b]).unwrap();
        env.catch_up().unwrap();
        assert_eq!(
            state(&env),
            serde_json::json!({ "c": 1, "last": size_of(&a) })
        );
    }

    #[test]
    fn snapshots_are_bounded_and_invalidated() {
        let mut env = env(1);
        for i in 0..30 {
            env.store_entries(vec![remote(100 + i, "a")]).unwrap();
        }
        env.catch_up().unwrap();
        let snapshots = snapshot_keys(&env.db().read().unwrap()).unwrap();
        assert_eq!(snapshots.len(), 3);

        // an entry in the middle of the snapshot range drops the snapshots after it, and
        // processing resumes from the one before it
        env.store_entries(vec![remote(125, "b")]).unwrap();
        let report = env.catch_up().unwrap();
        assert!(report.applied <= 8, "replayed {} entries", report.applied);
        assert_eq!(state(&env)["c"], 31);
    }

    #[test]
    fn overlay_masks_cleared_rows() {
        let db = MemStore::default();
        {
            let txn = db.write().unwrap();
            txn.put(
                Table::State,
                &path(&[KeyPart::Field("a"), KeyPart::Field("x")]),
                b"1",
            )
            .unwrap();
            txn.put(
                Table::State,
                &path(&[KeyPart::Field("a"), KeyPart::Field("y")]),
                b"2",
            )
            .unwrap();
            txn.put(Table::State, &path(&[KeyPart::Field("b")]), b"3")
                .unwrap();
            txn.commit().unwrap();
        }

        let mut overlay = Overlay::default();
        overlay.clear(&path(&[KeyPart::Field("a")]));
        overlay.put(
            path(&[KeyPart::Field("a"), KeyPart::Field("z")]),
            b"4".to_vec(),
        );

        let read = db.read().unwrap();
        let rows = overlay.scan(&[], || read.scan(Table::State, &[])).unwrap();
        assert_eq!(
            rows,
            vec![
                (
                    path(&[KeyPart::Field("a"), KeyPart::Field("z")]),
                    b"4".to_vec()
                ),
                (path(&[KeyPart::Field("b")]), b"3".to_vec()),
            ]
        );

        let txn = db.write().unwrap();
        overlay.apply(&txn).unwrap();
        txn.commit().unwrap();
        assert_eq!(db.read().unwrap().scan(Table::State, &[]).unwrap(), rows);
    }

    #[test]
    fn overlay_merge_keeps_order() {
        let mut base = Overlay::default();
        base.put(
            path(&[KeyPart::Field("a"), KeyPart::Field("x")]),
            b"1".to_vec(),
        );
        base.put(path(&[KeyPart::Field("b")]), b"2".to_vec());

        let mut newer = Overlay::default();
        newer.clear(&path(&[KeyPart::Field("a")]));
        newer.put(
            path(&[KeyPart::Field("a"), KeyPart::Field("y")]),
            b"3".to_vec(),
        );
        base.merge(newer);

        let rows = base.scan(&[], || Ok(vec![])).unwrap();
        assert_eq!(
            rows,
            vec![
                (
                    path(&[KeyPart::Field("a"), KeyPart::Field("y")]),
                    b"3".to_vec()
                ),
                (path(&[KeyPart::Field("b")]), b"2".to_vec()),
            ]
        );
    }
}
