//! Range-based set reconciliation of entry logs.
//!
//! Two peers find out which entries the other is missing without exchanging their whole logs.
//! Each sends fingerprints of id ranges; ranges whose fingerprints match are in sync, ranges that
//! differ are split into smaller ranges, and small enough ranges are answered with their id
//! list. Entries never change once created (edits and deletes are new entries), so ids are all
//! that needs comparing.
//!
//! The protocol alternates [`Turn`]s and is transport agnostic: a session ends when a peer
//! receives an empty turn and has nothing left to send.

use ribolita_common::entry::{Entry, EntryId};
use serde::{Deserialize, Serialize};

pub type Fingerprint = [u8; 16];

/// Ranges with at most this many ids are answered with their ids instead of being split.
pub const IDS_THRESHOLD: usize = 32;
/// Number of sub-ranges a range with mismatching fingerprints is split into.
pub const SPLIT_BUCKETS: usize = 16;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Mode {
    Fingerprint(Fingerprint),
    Ids(Vec<EntryId>),
}

/// Ids in `lower..upper` (upper exclusive).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Range {
    pub lower: String,
    pub upper: String,
    pub mode: Mode,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    /// ranges still being reconciled
    pub ranges: Vec<Range>,
    /// entries the sender is missing, to be included in the next turn
    pub want: Vec<EntryId>,
    /// entries the receiver is missing, or that were asked for
    pub entries: Vec<Entry>,
}

impl Turn {
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty() && self.want.is_empty() && self.entries.is_empty()
    }
}

/// Order-independent fingerprint of a set of ids: the sum of their hashes, hashed with the count.
pub fn fingerprint(ids: &[EntryId]) -> Fingerprint {
    let mut sum = [0u64; 4];
    for id in ids {
        let hash = blake3::hash(id.as_bytes());
        for (lane, chunk) in sum.iter_mut().zip(hash.as_bytes().chunks_exact(8)) {
            *lane = lane.wrapping_add(u64::from_le_bytes(chunk.try_into().unwrap()));
        }
    }
    let mut hasher = blake3::Hasher::new();
    for lane in sum {
        hasher.update(&lane.to_le_bytes());
    }
    hasher.update(&(ids.len() as u64).to_le_bytes());
    hasher.finalize().as_bytes()[..16].try_into().unwrap()
}

/// Where the reconciler reads the local ids from.
pub trait IdSource {
    /// Ids in `lower..upper`, in order.
    fn ids(&self, lower: &str, upper: &str) -> anyhow::Result<Vec<EntryId>>;
}

/// What to do after processing a peer's ranges.
#[derive(Debug, Default)]
pub struct Outcome {
    /// ranges to send back
    pub ranges: Vec<Range>,
    /// entries we're missing
    pub want: Vec<EntryId>,
    /// entries the peer is missing
    pub send: Vec<EntryId>,
}

/// The ranges that open a session.
pub fn initial_ranges(source: &impl IdSource) -> anyhow::Result<Vec<Range>> {
    let ids = source.ids(EntryId::MIN, EntryId::MAX)?;
    Ok(vec![describe(EntryId::MIN, EntryId::MAX, ids)])
}

/// Processes the ranges received from the peer.
pub fn reconcile(source: &impl IdSource, incoming: &[Range]) -> anyhow::Result<Outcome> {
    let mut outcome = Outcome::default();
    for range in incoming {
        let mine = source.ids(&range.lower, &range.upper)?;
        match &range.mode {
            Mode::Fingerprint(theirs) => {
                if fingerprint(&mine) == *theirs {
                    continue;
                }
                if mine.len() <= IDS_THRESHOLD {
                    outcome
                        .ranges
                        .push(describe(&range.lower, &range.upper, mine));
                } else {
                    split(&range.lower, &range.upper, mine, &mut outcome.ranges);
                }
            }
            Mode::Ids(theirs) => {
                // the peer told us everything it has in this range: settle it here
                let mut theirs = theirs.iter().peekable();
                let mut mine = mine.into_iter().peekable();
                loop {
                    match (mine.peek(), theirs.peek()) {
                        (None, None) => break,
                        (Some(my_id), Some(their_id)) if my_id == *their_id => {
                            mine.next();
                            theirs.next();
                        }
                        (Some(my_id), Some(their_id)) if my_id > *their_id => {
                            outcome.want.push(theirs.next().unwrap().clone());
                        }
                        (Some(_), _) => outcome.send.push(mine.next().unwrap()),
                        (None, Some(_)) => outcome.want.push(theirs.next().unwrap().clone()),
                    }
                }
            }
        }
    }
    Ok(outcome)
}

fn describe(lower: &str, upper: &str, ids: Vec<EntryId>) -> Range {
    let mode = if ids.len() <= IDS_THRESHOLD {
        Mode::Ids(ids)
    } else {
        Mode::Fingerprint(fingerprint(&ids))
    };
    Range {
        lower: lower.to_string(),
        upper: upper.to_string(),
        mode,
    }
}

/// Splits `lower..upper` into buckets of roughly equal sizes, each with its fingerprint.
fn split(lower: &str, upper: &str, ids: Vec<EntryId>, out: &mut Vec<Range>) {
    let bucket_size = ids.len().div_ceil(SPLIT_BUCKETS);
    let mut bucket_lower = lower.to_string();
    let mut chunks = ids.chunks(bucket_size).peekable();
    while let Some(chunk) = chunks.next() {
        let bucket_upper = chunks
            .peek()
            .map_or(upper.to_string(), |next| next[0].0.clone());
        out.push(Range {
            lower: bucket_lower,
            upper: bucket_upper.clone(),
            mode: Mode::Fingerprint(fingerprint(chunk)),
        });
        bucket_lower = bucket_upper;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    struct Peer(BTreeSet<EntryId>);

    impl IdSource for Peer {
        fn ids(&self, lower: &str, upper: &str) -> anyhow::Result<Vec<EntryId>> {
            let range = EntryId(lower.to_string())..EntryId(upper.to_string());
            Ok(self.0.range(range).cloned().collect())
        }
    }

    fn id(n: u64) -> EntryId {
        EntryId(format!("c{n:010}"))
    }

    /// runs a session in memory. Returns the number of non-empty messages sent.
    fn sync(a: &mut Peer, b: &mut Peer) -> usize {
        let mut msg: (Vec<Range>, Vec<EntryId>, Vec<EntryId>) =
            (initial_ranges(a).unwrap(), vec![], vec![]);
        let mut to_b = true;
        let mut rounds = 0;
        loop {
            let (ranges, want, entries) = msg;
            if ranges.is_empty() && want.is_empty() && entries.is_empty() {
                return rounds;
            }
            rounds += 1;
            assert!(rounds < 100, "session doesn't terminate");

            let to: &mut Peer = if to_b { &mut *b } else { &mut *a };
            to.0.extend(entries);
            let outcome = reconcile(to, &ranges).unwrap();
            let send = want.into_iter().chain(outcome.send).collect();
            msg = (outcome.ranges, outcome.want, send);
            to_b = !to_b;
        }
    }

    #[test]
    fn converges() {
        let shared: BTreeSet<EntryId> = (0..5000).map(|i| id(i * 3)).collect();
        let mut a = Peer(shared.clone());
        let mut b = Peer(shared);
        for i in 0..20 {
            a.0.insert(id(i * 1000 + 1));
            b.0.insert(id(i * 777 + 2));
        }

        let rounds = sync(&mut a, &mut b);
        assert_eq!(a.0, b.0);
        assert!(rounds < 12, "took {rounds} rounds");

        // already in sync: one round trip
        assert_eq!(sync(&mut a, &mut b), 1);
    }

    #[test]
    fn empty_peers() {
        let mut a = Peer(BTreeSet::new());
        let mut b = Peer((0..100).map(id).collect());
        sync(&mut a, &mut b);
        assert_eq!(a.0, b.0);
        let mut c = Peer(BTreeSet::new());
        sync(&mut b, &mut c);
        assert_eq!(b.0, c.0);
    }
}
