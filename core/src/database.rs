/// Transaction trait that provides basic operations for reading state data
pub trait ReadTransaction {
    /// Get state data for a given key prefix
    fn get(&self, prefix: &[u8]) -> anyhow::Result<(usize, Vec<u8>)>;
}

/// Transaction trait that provides operations for reading and writing state data
pub trait WriteTransaction: ReadTransaction {
    /// Set state data for a given key base
    fn set(&self, key_base: &[u8], data: &[u8]) -> anyhow::Result<()>;

    /// Commit the transaction
    fn commit(self) -> anyhow::Result<()>;
}
