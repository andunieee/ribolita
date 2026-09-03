use libmdbx::{Database, NoWriteMap, RO, RW, Transaction, TransactionKind, WriteFlags};
use std::collections::VecDeque;

use crate::{
    StateDatabase,
    database::{ReadTransaction, WriteTransaction},
    state_db::{deserialize_rows, serialize_rows},
};

pub struct StateLMDBDatabase {
    db: Database<NoWriteMap>,
}

unsafe impl Send for StateLMDBDatabase {}
unsafe impl Sync for StateLMDBDatabase {}

pub struct MdbxReadTransaction<'db> {
    txn: Transaction<'db, RO, NoWriteMap>,
}

pub struct MdbxWriteTransaction<'db> {
    txn: Transaction<'db, RW, NoWriteMap>,
}

fn get<K: TransactionKind>(
    txn: &Transaction<'_, K, NoWriteMap>,
    prefix: &[u8],
) -> anyhow::Result<(usize, Vec<u8>)> {
    let state_table = txn.open_table(Some("state"))?;
    let cursor = txn.cursor(&state_table)?;

    let mut rows = Vec::new();

    let iter = if prefix.is_empty() {
        cursor.into_iter_start::<Vec<u8>, Vec<u8>>()
    } else {
        cursor.into_iter_from::<Vec<u8>, Vec<u8>>(prefix)
    };

    for result in iter {
        let (key, value): (Vec<u8>, Vec<u8>) = result?;
        if !key.starts_with(prefix) {
            break;
        }

        let rel_path = &key[prefix.len()..];
        rows.push((rel_path.to_vec(), value));
    }

    let serialized_data = serialize_rows(&rows);
    Ok((rows.len(), serialized_data))
}

impl<'db> ReadTransaction for MdbxReadTransaction<'db> {
    fn get(&self, prefix: &[u8]) -> anyhow::Result<(usize, Vec<u8>)> {
        get(&self.txn, prefix)
    }
}

impl<'db> ReadTransaction for MdbxWriteTransaction<'db> {
    fn get(&self, prefix: &[u8]) -> anyhow::Result<(usize, Vec<u8>)> {
        get(&self.txn, prefix)
    }
}

impl<'db> WriteTransaction for MdbxWriteTransaction<'db> {
    fn set(&self, key_base: &[u8], data: &[u8]) -> anyhow::Result<()> {
        let state_table = self.txn.open_table(Some("state"))?;
        let cursor = self.txn.cursor(&state_table)?;

        let new_rows = deserialize_rows(data);
        let mut new_rows = VecDeque::from(new_rows.into_iter().collect::<Vec<_>>());

        // First, collect all existing keys and values that match the prefix
        let mut existing_entries = Vec::new();
        {
            let iter = if key_base.is_empty() {
                cursor.into_iter_start::<Vec<u8>, Vec<u8>>()
            } else {
                cursor.into_iter_from::<Vec<u8>, Vec<u8>>(key_base)
            };

            for result in iter {
                let (key, existing_value): (Vec<u8>, Vec<u8>) = result?;
                if !key.starts_with(key_base) {
                    break;
                }
                existing_entries.push((key, existing_value));
            }
        }

        // Now process the collected entries
        let state_table = self.txn.open_table(Some("state"))?;

        for (key, existing_value) in existing_entries {
            let rel_path = &key[key_base.len()..];

            if let Some((i, new_value)) = new_rows.iter().enumerate().find_map(|(i, (rk, val))| {
                if *rk == rel_path {
                    Some((i, val))
                } else {
                    None
                }
            }) {
                // row exists, update if value is different
                if existing_value != *new_value {
                    self.txn
                        .put(&state_table, &key, new_value, WriteFlags::empty())?;
                }
                // remove from new_rows as it's handled
                new_rows.swap_remove_front(i);
            } else {
                // row doesn't exist in new data => delete it
                self.txn.del(&state_table, &key, None)?;
            }
        }

        // all remaining rows in new_rows are new => insert them
        for (rel_path, new_value) in new_rows {
            let full_key = [key_base, rel_path].concat();
            self.txn
                .put(&state_table, &full_key, new_value, WriteFlags::empty())?;
        }

        Ok(())
    }

    fn commit(self) -> anyhow::Result<()> {
        self.txn.commit().map(|_| ()).map_err(Into::into)
    }
}

impl StateDatabase for StateLMDBDatabase {
    type ReadTxn<'db>
        = MdbxReadTransaction<'db>
    where
        Self: 'db;
    type WriteTxn<'db>
        = MdbxWriteTransaction<'db>
    where
        Self: 'db;

    fn init(path: &std::path::Path) -> Result<Self, String> {
        let db = Database::open_with_options(
            path,
            libmdbx::DatabaseOptions {
                max_tables: Some(32),
                ..Default::default()
            },
        )
        .map_err(|e| e.to_string())?;

        let txn = db.begin_rw_txn().map_err(|e| e.to_string())?;

        let _state_table = txn
            .create_table(Some("state"), Default::default())
            .map_err(|e| e.to_string())?;

        txn.commit().map_err(|e| e.to_string())?;

        Ok(Self { db })
    }

    fn start_read_txn<'db>(&'db self) -> anyhow::Result<Self::ReadTxn<'db>> {
        let txn = self.db.begin_ro_txn()?;
        Ok(MdbxReadTransaction { txn })
    }

    fn start_write_txn<'db>(&'db self) -> anyhow::Result<Self::WriteTxn<'db>> {
        let txn = self.db.begin_rw_txn()?;
        Ok(MdbxWriteTransaction { txn })
    }
}
