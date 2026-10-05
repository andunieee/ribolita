use ribolita_common::entry::Entry;
use std::path::Path;

use crate::entries_db::{EntriesStore, Query};

pub struct EntriesMdbxDatabase {
    db: libmdbx::Database<libmdbx::NoWriteMap>,
}

impl EntriesMdbxDatabase {
    fn open(path: &Path) -> Result<EntriesMdbxDatabase, String> {
        let db = libmdbx::Database::open_with_options(
            path,
            libmdbx::DatabaseOptions {
                max_tables: Some(32),
                ..Default::default()
            },
        )
        .map_err(|e| e.to_string())?;

        let txn = db.begin_rw_txn().map_err(|e| e.to_string())?;

        let _ = txn
            .create_table(Some("entries"), libmdbx::TableFlags::INTEGER_KEY)
            .map_err(|e| e.to_string())?;

        let _ = txn
            .create_table(
                Some("by_timestamp"),
                libmdbx::TableFlags::INTEGER_KEY | libmdbx::TableFlags::DUP_SORT,
            )
            .map_err(|e| e.to_string())?;

        txn.commit().map_err(|e| e.to_string())?;

        Ok(EntriesMdbxDatabase { db })
    }

    fn get_last_id<'txn, K: libmdbx::TransactionKind>(
        &self,
        txn: &'txn libmdbx::Transaction<'txn, K, libmdbx::NoWriteMap>,
    ) -> anyhow::Result<u64> {
        let entries_table = txn.open_table(Some("entries"))?;
        let mut cursor = txn.cursor(&entries_table)?;

        match cursor.last::<Vec<u8>, Vec<u8>>() {
            Ok(Some((key, _))) => {
                let id =
                    u64::from_ne_bytes(key.as_slice().try_into().expect("invalid last id size"));
                Ok(id)
            }
            Ok(None) => Ok(0),
            Err(err) => Err(err.into()),
        }
    }

    fn save<'txn>(
        &self,
        txn: &'txn libmdbx::Transaction<'txn, libmdbx::RW, libmdbx::NoWriteMap>,
        id: u64,
        entry: &Entry,
    ) -> anyhow::Result<()> {
        let id_bytes = id.to_ne_bytes();
        let entry_bytes = rmp_serde::to_vec(&entry).expect("entry must be rmp-serializable");

        let entries_table = txn.open_table(Some("entries"))?;
        let by_timestamp_table = txn.open_table(Some("by_timestamp"))?;

        // drop the index row of the entry we're overwriting, its timestamp may have changed
        if let Some(previous) = self.get(txn, id)? {
            txn.del(
                &by_timestamp_table,
                previous.when.to_ne_bytes(),
                Some(id_bytes.as_slice()),
            )?;
        }

        txn.put(
            &entries_table,
            id_bytes,
            &entry_bytes,
            libmdbx::WriteFlags::empty(),
        )?;

        // save to by_timestamp
        let ts_bytes = entry.when.to_ne_bytes();
        txn.put(
            &by_timestamp_table,
            ts_bytes,
            id_bytes,
            libmdbx::WriteFlags::empty(),
        )?;

        Ok(())
    }

    fn get<K: libmdbx::TransactionKind>(
        &self,
        txn: &libmdbx::Transaction<'_, K, libmdbx::NoWriteMap>,
        id: u64,
    ) -> anyhow::Result<Option<Entry>> {
        let id_bytes = id.to_ne_bytes();

        let entries_table = txn.open_table(Some("entries"))?;
        match txn.get::<Vec<u8>>(&entries_table, &id_bytes) {
            Ok(Some(entry_bytes)) => {
                let entry: Entry = rmp_serde::from_slice(&entry_bytes)?;
                Ok(Some(entry))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn query<K: libmdbx::TransactionKind>(
        &self,
        txn: &libmdbx::Transaction<'_, K, libmdbx::NoWriteMap>,
        query: Query,
    ) -> anyhow::Result<Vec<Entry>> {
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
                let by_timestamp_table = txn.open_table(Some("by_timestamp"))?;
                let cursor = txn.cursor(&by_timestamp_table)?;

                let mut ids = Vec::new();
                let start_key = if let Some(s) = since {
                    s.to_ne_bytes()
                } else {
                    0u64.to_ne_bytes()
                };

                let iter = if since.is_some() {
                    cursor.into_iter_from::<Vec<u8>, Vec<u8>>(&start_key)
                } else {
                    cursor.into_iter_start::<Vec<u8>, Vec<u8>>()
                };

                for result in iter {
                    let (ts_bytes, id_bytes): (Vec<u8>, Vec<u8>) = result?;
                    let ts = u64::from_ne_bytes(ts_bytes.as_slice().try_into().unwrap());

                    if let Some(u) = until
                        && ts >= u
                    {
                        break;
                    }

                    let id = u64::from_ne_bytes(id_bytes.as_slice().try_into().unwrap());
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
                        // take first lim
                        entries.truncate(lim);
                    } else {
                        // take last lim
                        if entries.len() > lim {
                            entries = entries.split_off(entries.len() - lim);
                        }
                    }
                }

                Ok(entries)
            }
        }
    }

    fn start_write_txn(
        &self,
    ) -> anyhow::Result<libmdbx::Transaction<'_, libmdbx::RW, libmdbx::NoWriteMap>> {
        Ok(self.db.begin_rw_txn()?)
    }

    fn start_read_txn(
        &self,
    ) -> anyhow::Result<libmdbx::Transaction<'_, libmdbx::RO, libmdbx::NoWriteMap>> {
        Ok(self.db.begin_ro_txn()?)
    }
}

unsafe impl Send for EntriesMdbxDatabase {}
unsafe impl Sync for EntriesMdbxDatabase {}

impl EntriesStore for EntriesMdbxDatabase {
    fn init(path: &Path) -> Result<Self, String> {
        Self::open(path)
    }

    fn last_id(&self) -> anyhow::Result<u64> {
        self.get_last_id(&self.start_read_txn()?)
    }

    fn save(&self, entries: &[&Entry]) -> anyhow::Result<()> {
        let txn = self.start_write_txn()?;
        for entry in entries {
            EntriesMdbxDatabase::save(self, &txn, entry.id, entry)?;
        }
        txn.commit()?;
        Ok(())
    }

    fn query(&self, query: Query) -> anyhow::Result<Vec<Entry>> {
        EntriesMdbxDatabase::query(self, &self.start_read_txn()?, query)
    }
}
