use redb::{Database, ReadableTable, TableDefinition};
use std::collections::VecDeque;
use std::marker::PhantomData;

use crate::{
    StateDatabase,
    database::{ReadTransaction, WriteTransaction},
    state_db::{deserialize_rows, serialize_rows},
};

const STATE_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("state");

pub struct StateRedbDatabase {
    db: Database,
}

unsafe impl Send for StateRedbDatabase {}
unsafe impl Sync for StateRedbDatabase {}

pub struct RedbReadTransaction<'db> {
    txn: redb::ReadTransaction,
    _marker: PhantomData<&'db ()>,
}

pub struct RedbWriteTransaction<'db> {
    txn: redb::WriteTransaction,
    _marker: PhantomData<&'db ()>,
}

macro_rules! iter_table {
    ($table:expr, $prefix:expr) => {{
        let mut rows = Vec::new();
        let iter = if $prefix.is_empty() {
            $table.iter()?
        } else {
            $table.range($prefix..)?
        };
        for result in iter {
            let (key, value) = result?;
            let key = key.value();
            if !key.starts_with($prefix) {
                break;
            }
            let rel_path = &key[$prefix.len()..];
            rows.push((rel_path.to_vec(), value.value().to_vec()));
        }
        let serialized_data = serialize_rows(&rows);
        Ok((rows.len(), serialized_data))
    }};
}

impl<'db> ReadTransaction for RedbReadTransaction<'db> {
    fn get(&self, prefix: &[u8]) -> anyhow::Result<(usize, Vec<u8>)> {
        let table = self.txn.open_table(STATE_TABLE)?;
        iter_table!(table, prefix)
    }
}

impl<'db> ReadTransaction for RedbWriteTransaction<'db> {
    fn get(&self, prefix: &[u8]) -> anyhow::Result<(usize, Vec<u8>)> {
        let table = self.txn.open_table(STATE_TABLE)?;
        iter_table!(table, prefix)
    }
}

impl<'db> WriteTransaction for RedbWriteTransaction<'db> {
    fn set(&self, key_base: &[u8], data: &[u8]) -> anyhow::Result<()> {
        let mut table = self.txn.open_table(STATE_TABLE)?;

        let new_rows = deserialize_rows(data);
        let mut new_rows = VecDeque::from(new_rows.into_iter().collect::<Vec<_>>());

        // First, collect all existing keys and values that match the prefix
        let mut existing_entries = Vec::new();
        {
            let iter = if key_base.is_empty() {
                table.iter()?
            } else {
                table.range(key_base..)?
            };

            for result in iter {
                let (key, existing_value) = result?;
                let key = key.value();
                if !key.starts_with(key_base) {
                    break;
                }
                existing_entries.push((key.to_vec(), existing_value.value().to_vec()));
            }
        }

        // Now process the collected entries
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
                if existing_value != **new_value {
                    table.insert(key.as_slice(), *new_value)?;
                }
                // remove from new_rows as it's handled
                new_rows.swap_remove_front(i);
            } else {
                // row doesn't exist in new data => delete it
                table.remove(key.as_slice())?;
            }
        }

        // all remaining rows in new_rows are new => insert them
        for (rel_path, new_value) in new_rows {
            let full_key = [key_base, rel_path].concat();
            table.insert(full_key.as_slice(), new_value)?;
        }

        Ok(())
    }

    fn commit(self) -> anyhow::Result<()> {
        self.txn.commit()?;
        Ok(())
    }
}

impl StateDatabase for StateRedbDatabase {
    type ReadTxn<'db>
        = RedbReadTransaction<'db>
    where
        Self: 'db;
    type WriteTxn<'db>
        = RedbWriteTransaction<'db>
    where
        Self: 'db;

    fn init(path: &std::path::Path) -> Result<Self, String> {
        let db_path = path.join("redb");
        let db = Database::create(&db_path).map_err(|e| e.to_string())?;

        // Ensure the table exists
        let write_txn = db.begin_write().map_err(|e| e.to_string())?;
        {
            let _ = write_txn.open_table(STATE_TABLE).map_err(|e| e.to_string())?;
        }
        write_txn.commit().map_err(|e| e.to_string())?;

        Ok(Self { db })
    }

    fn start_read_txn<'db>(&'db self) -> anyhow::Result<Self::ReadTxn<'db>> {
        let txn = self.db.begin_read()?;
        Ok(RedbReadTransaction {
            txn,
            _marker: PhantomData,
        })
    }

    fn start_write_txn<'db>(&'db self) -> anyhow::Result<Self::WriteTxn<'db>> {
        let txn = self.db.begin_write()?;
        Ok(RedbWriteTransaction {
            txn,
            _marker: PhantomData,
        })
    }
}
