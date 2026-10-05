use std::ops::ControlFlow;

/// A row: key and value bytes.
pub type Row = (Vec<u8>, Vec<u8>);

/// Called with each row's key and value until it breaks.
pub type Visitor<'a> = dyn FnMut(&[u8], &[u8]) -> ControlFlow<()> + 'a;

/// The tables of a program's store. Everything lives in one store so that appending entries,
/// changing the state and moving snapshots happen in a single transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Table {
    /// entry id (a cuid) => messagepack entry
    Entries,
    /// `when` (big-endian) ++ entry id => entry id: the order entries are applied in
    Order,
    /// encoded state path => messagepack value
    State,
    /// length-prefixed order key of the snapshot ++ encoded state path => messagepack value
    Snapshots,
    /// bookkeeping: applied tip, dirty marker, snapshot index
    Meta,
}

impl Table {
    pub const ALL: [Table; 5] = [
        Table::Entries,
        Table::Order,
        Table::State,
        Table::Snapshots,
        Table::Meta,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Table::Entries => "entries",
            Table::Order => "order",
            Table::State => "state",
            Table::Snapshots => "snapshots",
            Table::Meta => "meta",
        }
    }
}

pub trait ReadTxn {
    /// Calls `f` with every row of `table` whose key is `>= from`, in key order, until it breaks.
    fn visit(&self, table: Table, from: &[u8], f: &mut Visitor<'_>) -> anyhow::Result<()>;

    fn get(&self, table: Table, key: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
        let mut found = None;
        self.visit(table, key, &mut |k, v| {
            if k == key {
                found = Some(v.to_vec());
            }
            ControlFlow::Break(())
        })?;
        Ok(found)
    }

    /// All rows whose key starts with `prefix`, in key order.
    fn scan(&self, table: Table, prefix: &[u8]) -> anyhow::Result<Vec<Row>> {
        let mut rows = Vec::new();
        self.visit(table, prefix, &mut |k, v| {
            if !k.starts_with(prefix) {
                return ControlFlow::Break(());
            }
            rows.push((k.to_vec(), v.to_vec()));
            ControlFlow::Continue(())
        })?;
        Ok(rows)
    }

    /// Rows with `lower <= key < upper`, at most `limit` of them.
    fn range(
        &self,
        table: Table,
        lower: &[u8],
        upper: &[u8],
        limit: usize,
    ) -> anyhow::Result<Vec<Row>> {
        let mut rows = Vec::new();
        self.visit(table, lower, &mut |k, v| {
            if k >= upper || rows.len() >= limit {
                return ControlFlow::Break(());
            }
            rows.push((k.to_vec(), v.to_vec()));
            ControlFlow::Continue(())
        })?;
        Ok(rows)
    }
}

pub trait WriteTxn: ReadTxn {
    fn put(&self, table: Table, key: &[u8], value: &[u8]) -> anyhow::Result<()>;

    fn delete(&self, table: Table, key: &[u8]) -> anyhow::Result<()>;

    /// Deletes every row whose key starts with `prefix` (an empty prefix clears the table)
    fn delete_prefix(&self, table: Table, prefix: &[u8]) -> anyhow::Result<()> {
        for (key, _) in self.scan(table, prefix)? {
            self.delete(table, &key)?;
        }
        Ok(())
    }

    fn commit(self) -> anyhow::Result<()>;
}

pub trait Store: Sized {
    type Read<'a>: ReadTxn
    where
        Self: 'a;
    type Write<'a>: WriteTxn
    where
        Self: 'a;

    /// Opens (creating if needed) the store in the directory `path`.
    fn open(path: &std::path::Path) -> anyhow::Result<Self>;

    fn read(&self) -> anyhow::Result<Self::Read<'_>>;

    /// Starts a write transaction. Dropping it without committing discards its writes.
    fn write(&self) -> anyhow::Result<Self::Write<'_>>;
}
