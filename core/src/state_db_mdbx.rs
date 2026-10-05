use libmdbx::{Database, NoWriteMap, RO, RW, Transaction, TransactionKind, WriteFlags};

use crate::{
    StateDatabase,
    database::{ReadTransaction, Row, WriteTransaction},
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

fn scan<K: TransactionKind>(
    txn: &Transaction<'_, K, NoWriteMap>,
    prefix: &[u8],
) -> anyhow::Result<Vec<Row>> {
    let state_table = txn.open_table(Some("state"))?;
    let cursor = txn.cursor(&state_table)?;

    let iter = if prefix.is_empty() {
        cursor.into_iter_start::<Vec<u8>, Vec<u8>>()
    } else {
        cursor.into_iter_from::<Vec<u8>, Vec<u8>>(prefix)
    };

    let mut rows = Vec::new();
    for result in iter {
        let (key, value): (Vec<u8>, Vec<u8>) = result?;
        if !key.starts_with(prefix) {
            break;
        }
        rows.push((key, value));
    }

    Ok(rows)
}

impl<'db> ReadTransaction for MdbxReadTransaction<'db> {
    fn scan(&self, prefix: &[u8]) -> anyhow::Result<Vec<Row>> {
        scan(&self.txn, prefix)
    }
}

impl<'db> ReadTransaction for MdbxWriteTransaction<'db> {
    fn scan(&self, prefix: &[u8]) -> anyhow::Result<Vec<Row>> {
        scan(&self.txn, prefix)
    }
}

impl<'db> WriteTransaction for MdbxWriteTransaction<'db> {
    fn put(&self, key: &[u8], value: &[u8]) -> anyhow::Result<()> {
        let state_table = self.txn.open_table(Some("state"))?;
        self.txn
            .put(&state_table, key, value, WriteFlags::empty())?;
        Ok(())
    }

    fn delete_prefix(&self, prefix: &[u8]) -> anyhow::Result<()> {
        let state_table = self.txn.open_table(Some("state"))?;
        for (key, _) in scan(&self.txn, prefix)? {
            self.txn.del(&state_table, &key, None)?;
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
