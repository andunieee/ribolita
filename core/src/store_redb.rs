use redb::{Database, ReadableTable, TableDefinition};

use crate::store::{ReadTxn, Store, Table, Visitor, WriteTxn};

type Def = TableDefinition<'static, &'static [u8], &'static [u8]>;

fn def(table: Table) -> Def {
    TableDefinition::new(table.name())
}

pub struct RedbStore {
    db: Database,
}

pub struct RedbRead {
    txn: redb::ReadTransaction,
}

pub struct RedbWrite {
    txn: redb::WriteTransaction,
}

fn visit(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    from: &[u8],
    f: &mut Visitor<'_>,
) -> anyhow::Result<()> {
    for row in table.range(from..)? {
        let (key, value) = row?;
        if f(key.value(), value.value()).is_break() {
            break;
        }
    }
    Ok(())
}

impl ReadTxn for RedbRead {
    fn visit(&self, table: Table, from: &[u8], f: &mut Visitor<'_>) -> anyhow::Result<()> {
        visit(&self.txn.open_table(def(table))?, from, f)
    }
}

impl ReadTxn for RedbWrite {
    fn visit(&self, table: Table, from: &[u8], f: &mut Visitor<'_>) -> anyhow::Result<()> {
        visit(&self.txn.open_table(def(table))?, from, f)
    }
}

impl WriteTxn for RedbWrite {
    fn put(&self, table: Table, key: &[u8], value: &[u8]) -> anyhow::Result<()> {
        self.txn.open_table(def(table))?.insert(key, value)?;
        Ok(())
    }

    fn delete(&self, table: Table, key: &[u8]) -> anyhow::Result<()> {
        self.txn.open_table(def(table))?.remove(key)?;
        Ok(())
    }

    fn commit(self) -> anyhow::Result<()> {
        self.txn.commit()?;
        Ok(())
    }
}

impl Store for RedbStore {
    type Read<'a> = RedbRead;
    type Write<'a> = RedbWrite;

    fn open(path: &std::path::Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(path)?;
        let db = Database::create(path.join("store.redb"))?;
        let txn = db.begin_write()?;
        for table in Table::ALL {
            txn.open_table(def(table))?;
        }
        txn.commit()?;
        Ok(Self { db })
    }

    fn read(&self) -> anyhow::Result<RedbRead> {
        Ok(RedbRead {
            txn: self.db.begin_read()?,
        })
    }

    fn write(&self) -> anyhow::Result<RedbWrite> {
        Ok(RedbWrite {
            txn: self.db.begin_write()?,
        })
    }
}
