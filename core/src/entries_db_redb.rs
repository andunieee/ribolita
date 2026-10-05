use redb::{Database, ReadableTable, TableDefinition};
use ribolita_common::entry::Entry;
use std::path::Path;

use crate::entries_db::{EntriesStore, Query};

const ENTRIES_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("entries");
const BY_TIMESTAMP_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("by_timestamp");

/// Composite key: (timestamp, entry_id) for ordering and uniqueness. Big-endian so byte order
/// matches numeric order.
fn ts_key(when: u64, id: u64) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[..8].copy_from_slice(&when.to_be_bytes());
    key[8..].copy_from_slice(&id.to_be_bytes());
    key
}

pub struct EntriesRedbDatabase {
    db: Database,
}

impl EntriesRedbDatabase {
    fn open(path: &Path) -> Result<EntriesRedbDatabase, String> {
        let db_path = path.join("entries.redb");
        let db = Database::create(db_path).map_err(|e| e.to_string())?;

        let write_txn = db.begin_write().map_err(|e| e.to_string())?;
        {
            let _ = write_txn.open_table(ENTRIES_TABLE).map_err(|e| e.to_string())?;
            let _ = write_txn
                .open_table(BY_TIMESTAMP_TABLE)
                .map_err(|e| e.to_string())?;
        }
        write_txn.commit().map_err(|e| e.to_string())?;

        Ok(EntriesRedbDatabase { db })
    }

    fn get(&self, txn: &redb::ReadTransaction, id: u64) -> anyhow::Result<Option<Entry>> {
        let table = txn.open_table(ENTRIES_TABLE)?;
        match table.get(id)? {
            Some(access_guard) => {
                let entry: Entry = rmp_serde::from_slice(access_guard.value())?;
                Ok(Some(entry))
            }
            None => Ok(None),
        }
    }

    fn get_last_id(&self, txn: &redb::ReadTransaction) -> anyhow::Result<u64> {
        let table = txn.open_table(ENTRIES_TABLE)?;
        match table.iter()?.next_back() {
            Some(Ok((key, _))) => Ok(key.value()),
            Some(Err(e)) => Err(e.into()),
            None => Ok(0),
        }
    }

    fn save(
        &self,
        txn: &redb::WriteTransaction,
        id: u64,
        entry: &Entry,
    ) -> anyhow::Result<()> {
        let mut entries_table = txn.open_table(ENTRIES_TABLE)?;
        let mut by_ts_table = txn.open_table(BY_TIMESTAMP_TABLE)?;

        let entry_bytes = rmp_serde::to_vec(entry)?;
        let previous = entries_table.insert(id, entry_bytes.as_slice())?;

        // drop the index row of the entry we're overwriting, its timestamp may have changed
        if let Some(previous) = previous {
            let previous: Entry = rmp_serde::from_slice(previous.value())?;
            by_ts_table.remove(ts_key(previous.when, id).as_slice())?;
        }
        by_ts_table.insert(ts_key(entry.when, id).as_slice(), [].as_slice())?;

        Ok(())
    }

    fn query(&self, txn: &redb::ReadTransaction, query: Query) -> anyhow::Result<Vec<Entry>> {
        match query {
            Query::ByIds(ids) => {
                let mut entries = Vec::new();
                for id in ids {
                    if let Some(entry) = self.get(txn, id)? {
                        entries.push(entry);
                    }
                }
                Ok(entries)
            }
            Query::ByTimestamps {
                since,
                until,
                limit,
            } => {
                let table = txn.open_table(BY_TIMESTAMP_TABLE)?;

                let start_key = ts_key(since.unwrap_or(0), 0);

                let iter = if let Some(u) = until {
                    let end_key = ts_key(u, 0);
                    table.range(start_key.as_slice()..end_key.as_slice())?
                } else {
                    table.range::<&[u8]>(start_key.as_slice()..)?
                };

                let mut ids = Vec::new();
                for result in iter {
                    let (key, _) = result?;
                    let key_bytes = key.value();
                    let id = u64::from_be_bytes(key_bytes[8..].try_into().unwrap());
                    ids.push(id);
                }

                // now get the entries
                let mut entries = Vec::new();
                for id in ids {
                    if let Some(entry) = self.get(txn, id)? {
                        entries.push(entry);
                    }
                }

                // apply limit
                if let Some(lim) = limit {
                    if since.is_some() {
                        entries.truncate(lim);
                    } else if entries.len() > lim {
                        entries = entries.split_off(entries.len() - lim);
                    }
                }

                Ok(entries)
            }
        }
    }

    fn start_write_txn(&self) -> anyhow::Result<redb::WriteTransaction> {
        Ok(self.db.begin_write()?)
    }

    fn start_read_txn(&self) -> anyhow::Result<redb::ReadTransaction> {
        Ok(self.db.begin_read()?)
    }
}

unsafe impl Send for EntriesRedbDatabase {}
unsafe impl Sync for EntriesRedbDatabase {}

impl EntriesStore for EntriesRedbDatabase {
    fn init(path: &Path) -> Result<Self, String> {
        Self::open(path)
    }

    fn last_id(&self) -> anyhow::Result<u64> {
        self.get_last_id(&self.start_read_txn()?)
    }

    fn save(&self, entries: &[&Entry]) -> anyhow::Result<()> {
        let txn = self.start_write_txn()?;
        for entry in entries {
            EntriesRedbDatabase::save(self, &txn, entry.id, entry)?;
        }
        txn.commit()?;
        Ok(())
    }

    fn query(&self, query: Query) -> anyhow::Result<Vec<Entry>> {
        EntriesRedbDatabase::query(self, &self.start_read_txn()?, query)
    }
}
