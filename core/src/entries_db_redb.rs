use redb::{Database, ReadableTable, TableDefinition};
use ribolita_common::entry::Entry;
use std::path::Path;

use crate::entries_db::Query;

const ENTRIES_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("entries");
const BY_TIMESTAMP_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("by_timestamp");

pub struct EntriesDatabase {
    db: Database,
}

impl EntriesDatabase {
    pub fn init(path: &Path) -> Result<EntriesDatabase, String> {
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

        Ok(EntriesDatabase { db })
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

    pub fn get_last_id(&self, txn: &redb::ReadTransaction) -> anyhow::Result<u64> {
        let table = txn.open_table(ENTRIES_TABLE)?;
        match table.iter()?.rev().next() {
            Some(Ok((key, _))) => Ok(key.value()),
            Some(Err(e)) => Err(e.into()),
            None => Ok(0),
        }
    }

    pub fn save(
        &self,
        txn: &redb::WriteTransaction,
        id: u64,
        entry: &Entry,
    ) -> anyhow::Result<()> {
        let mut entries_table = txn.open_table(ENTRIES_TABLE)?;
        let mut by_ts_table = txn.open_table(BY_TIMESTAMP_TABLE)?;

        let entry_bytes = rmp_serde::to_vec(entry)?;
        entries_table.insert(id, entry_bytes.as_slice())?;

        // Composite key: (timestamp, entry_id) for ordering and uniqueness
        let mut ts_key = [0u8; 16];
        ts_key[..8].copy_from_slice(&entry.when.to_ne_bytes());
        ts_key[8..].copy_from_slice(&id.to_ne_bytes());
        by_ts_table.insert(ts_key.as_slice(), [].as_slice())?;

        Ok(())
    }

    pub fn query(&self, txn: &redb::ReadTransaction, query: Query) -> anyhow::Result<Vec<Entry>> {
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

                let start_key = {
                    let mut key = [0u8; 16];
                    if let Some(s) = since {
                        key[..8].copy_from_slice(&s.to_ne_bytes());
                    }
                    key
                };

                let iter = if let Some(u) = until {
                    let mut end_key = [0u8; 16];
                    end_key[..8].copy_from_slice(&u.to_ne_bytes());
                    table.range(start_key.as_slice()..end_key.as_slice())?
                } else {
                    table.range::<&[u8]>(start_key.as_slice()..)?
                };

                let mut ids = Vec::new();
                for result in iter {
                    let (key, _) = result?;
                    let key_bytes = key.value();
                    let id = u64::from_ne_bytes(key_bytes[8..].try_into().unwrap());
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

    pub fn start_write_txn(&self) -> anyhow::Result<redb::WriteTransaction> {
        Ok(self.db.begin_write()?)
    }

    pub fn start_read_txn(&self) -> anyhow::Result<redb::ReadTransaction> {
        Ok(self.db.begin_read()?)
    }
}

unsafe impl Send for EntriesDatabase {}
unsafe impl Sync for EntriesDatabase {}
