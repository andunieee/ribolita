use crate::database::{ReadTransaction, WriteTransaction};

pub trait StateDatabase {
    type ReadTxn<'db>: ReadTransaction
    where
        Self: 'db;
    type WriteTxn<'db>: WriteTransaction
    where
        Self: 'db;

    fn init(path: &std::path::Path) -> Result<Self, String>
    where
        Self: Sized;

    fn start_read_txn<'db>(&'db self) -> anyhow::Result<Self::ReadTxn<'db>>;
    fn start_write_txn<'db>(&'db self) -> anyhow::Result<Self::WriteTxn<'db>>;
}
