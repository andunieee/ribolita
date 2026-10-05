use ribolita_common::entry::Entry;

#[derive(Debug)]
pub enum Query {
    ByIds(Vec<u64>),
    ByTimestamps {
        since: Option<u64>,
        until: Option<u64>,
        limit: Option<usize>,
    },
}

/// The append-only log of entries, keyed by sequential id.
pub trait EntriesStore {
    fn init(path: &std::path::Path) -> Result<Self, String>
    where
        Self: Sized;

    /// Id of the newest entry, 0 if there are none.
    fn last_id(&self) -> anyhow::Result<u64>;

    /// Inserts or overwrites entries (by their `id`) in a single transaction.
    fn save(&self, entries: &[&Entry]) -> anyhow::Result<()>;

    fn query(&self, query: Query) -> anyhow::Result<Vec<Entry>>;
}
