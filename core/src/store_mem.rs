use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::store::{ReadTxn, Store, Table, Visitor, WriteTxn};

type Tables = [BTreeMap<Vec<u8>, Vec<u8>>; Table::ALL.len()];

fn index(table: Table) -> usize {
    Table::ALL.iter().position(|t| *t == table).unwrap()
}

fn visit(tables: &Tables, table: Table, from: &[u8], f: &mut Visitor<'_>) -> anyhow::Result<()> {
    for (key, value) in tables[index(table)].range(from.to_vec()..) {
        if f(key, value).is_break() {
            break;
        }
    }
    Ok(())
}

/// In-memory store, for tests and throwaway environments.
#[derive(Default, Clone)]
pub struct MemStore {
    tables: Arc<Mutex<Tables>>,
}

pub struct MemRead {
    snapshot: Tables,
}

/// Works on a copy of the tables that replaces the original on commit.
pub struct MemWrite<'a> {
    store: &'a MemStore,
    tables: RefCell<Tables>,
}

impl ReadTxn for MemRead {
    fn visit(&self, table: Table, from: &[u8], f: &mut Visitor<'_>) -> anyhow::Result<()> {
        visit(&self.snapshot, table, from, f)
    }
}

impl ReadTxn for MemWrite<'_> {
    fn visit(&self, table: Table, from: &[u8], f: &mut Visitor<'_>) -> anyhow::Result<()> {
        visit(&self.tables.borrow(), table, from, f)
    }
}

impl WriteTxn for MemWrite<'_> {
    fn put(&self, table: Table, key: &[u8], value: &[u8]) -> anyhow::Result<()> {
        self.tables.borrow_mut()[index(table)].insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn delete(&self, table: Table, key: &[u8]) -> anyhow::Result<()> {
        self.tables.borrow_mut()[index(table)].remove(key);
        Ok(())
    }

    fn commit(self) -> anyhow::Result<()> {
        *self.store.tables.lock().unwrap() = self.tables.into_inner();
        Ok(())
    }
}

impl Store for MemStore {
    type Read<'a> = MemRead;
    type Write<'a> = MemWrite<'a>;

    fn open(_path: &std::path::Path) -> anyhow::Result<Self> {
        Ok(Self::default())
    }

    fn read(&self) -> anyhow::Result<MemRead> {
        Ok(MemRead {
            snapshot: self.tables.lock().unwrap().clone(),
        })
    }

    fn write(&self) -> anyhow::Result<MemWrite<'_>> {
        Ok(MemWrite {
            store: self,
            tables: RefCell::new(self.tables.lock().unwrap().clone()),
        })
    }
}
