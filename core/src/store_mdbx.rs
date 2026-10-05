use libmdbx::{Database, NoWriteMap, RO, RW, Transaction, TransactionKind, WriteFlags};

use crate::store::{ReadTxn, Store, Table, Visitor, WriteTxn};

pub struct MdbxStore {
    db: Database<NoWriteMap>,
}

unsafe impl Send for MdbxStore {}
unsafe impl Sync for MdbxStore {}

pub struct MdbxRead<'db> {
    txn: Transaction<'db, RO, NoWriteMap>,
}

pub struct MdbxWrite<'db> {
    txn: Transaction<'db, RW, NoWriteMap>,
}

fn visit<K: TransactionKind>(
    txn: &Transaction<'_, K, NoWriteMap>,
    table: Table,
    from: &[u8],
    f: &mut Visitor<'_>,
) -> anyhow::Result<()> {
    let table = txn.open_table(Some(table.name()))?;
    let cursor = txn.cursor(&table)?;
    let iter = if from.is_empty() {
        cursor.into_iter_start::<Vec<u8>, Vec<u8>>()
    } else {
        cursor.into_iter_from::<Vec<u8>, Vec<u8>>(from)
    };
    for row in iter {
        let (key, value) = row?;
        if f(&key, &value).is_break() {
            break;
        }
    }
    Ok(())
}

impl ReadTxn for MdbxRead<'_> {
    fn visit(&self, table: Table, from: &[u8], f: &mut Visitor<'_>) -> anyhow::Result<()> {
        visit(&self.txn, table, from, f)
    }
}

impl ReadTxn for MdbxWrite<'_> {
    fn visit(&self, table: Table, from: &[u8], f: &mut Visitor<'_>) -> anyhow::Result<()> {
        visit(&self.txn, table, from, f)
    }
}

impl WriteTxn for MdbxWrite<'_> {
    fn put(&self, table: Table, key: &[u8], value: &[u8]) -> anyhow::Result<()> {
        let table = self.txn.open_table(Some(table.name()))?;
        self.txn.put(&table, key, value, WriteFlags::empty())?;
        Ok(())
    }

    fn delete(&self, table: Table, key: &[u8]) -> anyhow::Result<()> {
        let table = self.txn.open_table(Some(table.name()))?;
        self.txn.del(&table, key, None)?;
        Ok(())
    }

    fn commit(self) -> anyhow::Result<()> {
        self.txn.commit()?;
        Ok(())
    }
}

impl Store for MdbxStore {
    type Read<'a> = MdbxRead<'a>;
    type Write<'a> = MdbxWrite<'a>;

    fn open(path: &std::path::Path) -> anyhow::Result<Self> {
        let path = path.join("store.mdbx");
        std::fs::create_dir_all(&path)?;
        let db = Database::open_with_options(
            &path,
            libmdbx::DatabaseOptions {
                max_tables: Some(8),
                ..Default::default()
            },
        )?;
        let txn = db.begin_rw_txn()?;
        for table in Table::ALL {
            txn.create_table(Some(table.name()), Default::default())?;
        }
        txn.commit()?;
        Ok(Self { db })
    }

    fn read(&self) -> anyhow::Result<MdbxRead<'_>> {
        Ok(MdbxRead {
            txn: self.db.begin_ro_txn()?,
        })
    }

    fn write(&self) -> anyhow::Result<MdbxWrite<'_>> {
        Ok(MdbxWrite {
            txn: self.db.begin_rw_txn()?,
        })
    }
}
