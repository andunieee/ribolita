use redb::{Database, ReadableTable, TableDefinition};
use std::marker::PhantomData;

use crate::{
    StateDatabase,
    database::{ReadTransaction, Row, WriteTransaction},
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

fn scan(table: &impl ReadableTable<&'static [u8], &'static [u8]>, prefix: &[u8]) -> anyhow::Result<Vec<Row>> {
    let mut rows = Vec::new();
    for result in table.range(prefix..)? {
        let (key, value) = result?;
        let key = key.value();
        if !key.starts_with(prefix) {
            break;
        }
        rows.push((key.to_vec(), value.value().to_vec()));
    }
    Ok(rows)
}

impl<'db> ReadTransaction for RedbReadTransaction<'db> {
    fn scan(&self, prefix: &[u8]) -> anyhow::Result<Vec<Row>> {
        let table = self.txn.open_table(STATE_TABLE)?;
        scan(&table, prefix)
    }
}

impl<'db> ReadTransaction for RedbWriteTransaction<'db> {
    fn scan(&self, prefix: &[u8]) -> anyhow::Result<Vec<Row>> {
        let table = self.txn.open_table(STATE_TABLE)?;
        scan(&table, prefix)
    }
}

impl<'db> WriteTransaction for RedbWriteTransaction<'db> {
    fn put(&self, key: &[u8], value: &[u8]) -> anyhow::Result<()> {
        let mut table = self.txn.open_table(STATE_TABLE)?;
        table.insert(key, value)?;
        Ok(())
    }

    fn delete_prefix(&self, prefix: &[u8]) -> anyhow::Result<()> {
        let mut table = self.txn.open_table(STATE_TABLE)?;
        for (key, _) in scan(&table, prefix)? {
            table.remove(key.as_slice())?;
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
