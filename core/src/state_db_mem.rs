use crate::StateDatabase;
use crate::database::{ReadTransaction, Row, WriteTransaction};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

type Data = Rc<RefCell<BTreeMap<Vec<u8>, Vec<u8>>>>;

pub struct MemoryReadTransaction {
    data: Data,
}

pub struct MemoryWriteTransaction {
    data: Data,
}

fn scan(data: &BTreeMap<Vec<u8>, Vec<u8>>, prefix: &[u8]) -> Vec<Row> {
    data.range(prefix.to_vec()..)
        .take_while(|(key, _)| key.starts_with(prefix))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

impl ReadTransaction for MemoryReadTransaction {
    fn scan(&self, prefix: &[u8]) -> anyhow::Result<Vec<Row>> {
        Ok(scan(&self.data.borrow(), prefix))
    }
}

impl ReadTransaction for MemoryWriteTransaction {
    fn scan(&self, prefix: &[u8]) -> anyhow::Result<Vec<Row>> {
        Ok(scan(&self.data.borrow(), prefix))
    }
}

impl WriteTransaction for MemoryWriteTransaction {
    fn put(&self, key: &[u8], value: &[u8]) -> anyhow::Result<()> {
        self.data.borrow_mut().insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn delete_prefix(&self, prefix: &[u8]) -> anyhow::Result<()> {
        self.data
            .borrow_mut()
            .retain(|key, _| !key.starts_with(prefix));
        Ok(())
    }

    fn commit(self) -> anyhow::Result<()> {
        // For in-memory, commit is a no-op since changes are applied immediately
        Ok(())
    }
}

/// In-memory state database implementation using BTreeMap
pub struct StateMemDatabase {
    data: Data,
}

impl StateDatabase for StateMemDatabase {
    type ReadTxn<'db>
        = MemoryReadTransaction
    where
        Self: 'db;
    type WriteTxn<'db>
        = MemoryWriteTransaction
    where
        Self: 'db;

    fn init(_path: &std::path::Path) -> Result<Self, String> {
        Ok(Self {
            data: Rc::new(RefCell::new(BTreeMap::new())),
        })
    }

    fn start_read_txn<'db>(&'db self) -> anyhow::Result<Self::ReadTxn<'db>> {
        Ok(MemoryReadTransaction {
            data: self.data.clone(),
        })
    }

    fn start_write_txn<'db>(&'db self) -> anyhow::Result<Self::WriteTxn<'db>> {
        Ok(MemoryWriteTransaction {
            data: self.data.clone(),
        })
    }
}
