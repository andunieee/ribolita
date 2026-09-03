#[derive(Debug)]
pub enum Query {
    ByIds(Vec<u64>),
    ByTimestamps {
        since: Option<u64>,
        until: Option<u64>,
        limit: Option<usize>,
    },
}
