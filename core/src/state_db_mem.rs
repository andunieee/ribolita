use crate::StateDatabase;
use crate::database::{ReadTransaction, WriteTransaction};
use crate::state_db::{deserialize_rows, serialize_rows};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

pub struct MemoryReadTransaction {
    data: Rc<RefCell<BTreeMap<Vec<u8>, Vec<u8>>>>,
}

pub struct MemoryWriteTransaction {
    data: Rc<RefCell<BTreeMap<Vec<u8>, Vec<u8>>>>,
}

fn get(
    data: std::cell::Ref<'_, BTreeMap<Vec<u8>, Vec<u8>>>,
    prefix: &[u8],
) -> anyhow::Result<(usize, Vec<u8>)> {
    let mut rows = Vec::new();

    // find all keys that start with the prefix
    for (key, value) in data.iter() {
        if key.starts_with(prefix) {
            let rel_key = &key[prefix.len()..];
            rows.push((rel_key.to_vec(), value.clone()));
        }
    }

    // serialize the rows
    let serialized_data = serialize_rows(&rows);
    Ok((rows.len(), serialized_data))
}

impl ReadTransaction for MemoryReadTransaction {
    fn get(&self, prefix: &[u8]) -> anyhow::Result<(usize, Vec<u8>)> {
        get(self.data.borrow(), prefix)
    }
}

impl ReadTransaction for MemoryWriteTransaction {
    fn get(&self, prefix: &[u8]) -> anyhow::Result<(usize, Vec<u8>)> {
        get(self.data.borrow(), prefix)
    }
}

impl WriteTransaction for MemoryWriteTransaction {
    fn set(&self, key_base: &[u8], data: &[u8]) -> anyhow::Result<()> {
        let new_rows = deserialize_rows(data);
        let mut data_map = self.data.borrow_mut();

        // Remove all existing keys with this prefix
        let keys_to_remove: Vec<Vec<u8>> = data_map
            .keys()
            .filter(|key| key.starts_with(key_base))
            .cloned()
            .collect();

        for key in keys_to_remove {
            data_map.remove(&key);
        }

        // Add new rows
        for (rel_key, value) in new_rows {
            let full_key = [key_base, rel_key].concat();
            data_map.insert(full_key, value.to_vec());
        }

        Ok(())
    }

    fn commit(self) -> anyhow::Result<()> {
        // For in-memory, commit is a no-op since changes are applied immediately
        Ok(())
    }
}

/// In-memory state database implementation using BTreeMap
pub struct StateMemDatabase {
    data: Rc<RefCell<BTreeMap<Vec<u8>, Vec<u8>>>>,
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
