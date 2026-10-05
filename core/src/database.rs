use ribolita_common::value;

/// A row of the state database: full encoded key and messagepack-encoded value.
pub type Row = (Vec<u8>, Vec<u8>);

/// Transaction trait that provides basic operations for reading state data
pub trait ReadTransaction {
    /// All rows whose key starts with `prefix`, in key order, with their full keys.
    fn scan(&self, prefix: &[u8]) -> anyhow::Result<Vec<Row>>;

    /// State data under a key prefix, as a row blob (see [`value::split_rows`]) whose keys are
    /// relative to `prefix`. This is the format the WASM guest and [`value::from_bytes`] read.
    fn get(&self, prefix: &[u8]) -> anyhow::Result<Vec<u8>> {
        let rows = self.scan(prefix)?;
        let relative: Vec<(&[u8], &[u8])> = rows
            .iter()
            .map(|(k, v)| (&k[prefix.len()..], v.as_slice()))
            .collect();
        Ok(value::join_rows(&relative))
    }
}

/// Transaction trait that provides operations for reading and writing state data
pub trait WriteTransaction: ReadTransaction {
    /// Insert or overwrite a single row
    fn put(&self, key: &[u8], value: &[u8]) -> anyhow::Result<()>;

    /// Delete every row whose key starts with `prefix` (an empty prefix clears everything)
    fn delete_prefix(&self, prefix: &[u8]) -> anyhow::Result<()>;

    /// Commit the transaction
    fn commit(self) -> anyhow::Result<()>;
}
