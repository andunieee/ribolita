//! Runs a sync session (see [`ribolita::sync`]) over any byte stream.
//!
//! Frames are a little-endian `u32` length followed by messagepack. A stream starts with the
//! initiator's [`Hello`], naming the program to sync, and the responder's [`HelloReply`]. Then
//! peers alternate [`Turn`]s, the initiator first. A peer that receives an empty turn and has
//! nothing left to send ends the session by closing its side; the other peer then reads
//! end-of-stream.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use ribolita::sync::{self, Turn};
use ribolita::{RunEnvironment, Store};
use ribolita_common::entry::EntryId;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub type SharedEnv<S> = Arc<Mutex<RunEnvironment<S>>>;

const MAX_FRAME: usize = 32 * 1024 * 1024;
/// Entries per turn; the rest are sent in later turns.
const MAX_ENTRIES_PER_TURN: usize = 256;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SyncStats {
    /// entries received that were new and got stored
    pub received: usize,
    pub sent: usize,
}

/// Opens a stream: which program to sync, and the credentials to do so.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub program: String,
    pub token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloReply {
    /// why the responder refuses the session, if it does
    pub error: Option<String>,
}

pub async fn write_frame<T: Serialize>(
    w: &mut (impl AsyncWrite + Unpin),
    frame: &T,
) -> anyhow::Result<()> {
    let bytes = rmp_serde::to_vec(frame)?;
    anyhow::ensure!(bytes.len() <= MAX_FRAME, "sync frame too large");
    w.write_all(&(bytes.len() as u32).to_le_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

/// Reads the next frame, or `None` at end of stream.
pub async fn read_frame<T: DeserializeOwned>(
    r: &mut (impl AsyncRead + Unpin),
) -> anyhow::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    anyhow::ensure!(len <= MAX_FRAME, "sync frame too large");
    let mut bytes = vec![0u8; len];
    r.read_exact(&mut bytes).await?;
    Ok(Some(rmp_serde::from_slice(&bytes)?))
}

fn lock<S: Store>(env: &SharedEnv<S>) -> std::sync::MutexGuard<'_, RunEnvironment<S>> {
    env.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Reconciles the local log with the peer's over `r`/`w`. Received entries are stored but not
/// applied: call [`RunEnvironment::catch_up`] afterwards.
pub async fn sync_session<S: Store + 'static>(
    env: &SharedEnv<S>,
    mut r: impl AsyncRead + Unpin,
    mut w: impl AsyncWrite + Unpin,
    initiator: bool,
) -> anyhow::Result<SyncStats> {
    let mut stats = SyncStats::default();
    let mut outbox: VecDeque<EntryId> = VecDeque::new();

    if initiator {
        let ranges = sync::initial_ranges(&*lock(env))?;
        write_frame(
            &mut w,
            &Turn {
                ranges,
                ..Turn::default()
            },
        )
        .await?;
    }

    let mut sent_empty = false;
    loop {
        let Some(turn) = read_frame::<Turn>(&mut r).await? else {
            anyhow::ensure!(sent_empty, "peer closed the sync session early");
            break;
        };
        if turn.is_empty() && outbox.is_empty() {
            break;
        }

        let reply = {
            let mut env = lock(env);
            if !turn.entries.is_empty() {
                stats.received += env.store_entries(turn.entries)?.len();
            }
            let outcome = sync::reconcile(&*env, &turn.ranges)?;
            outbox.extend(turn.want);
            outbox.extend(outcome.send);
            let ids: Vec<EntryId> = outbox
                .drain(..outbox.len().min(MAX_ENTRIES_PER_TURN))
                .collect();
            Turn {
                ranges: outcome.ranges,
                want: outcome.want,
                entries: env.entries_for_sync(&ids)?,
            }
        };
        stats.sent += reply.entries.len();
        sent_empty = reply.is_empty();
        write_frame(&mut w, &reply).await?;
    }

    w.shutdown().await.ok();
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ribolita::{MemStore, Query, testing::COUNTER_WAT};
    use ribolita_common::entry::{Author, Entry};

    fn env(author: u8) -> SharedEnv<MemStore> {
        let mut env = RunEnvironment::new(
            std::path::Path::new(""),
            COUNTER_WAT.as_bytes(),
            Author([author; 32]),
        )
        .unwrap();
        env.snapshot_interval = 50;
        Arc::new(Mutex::new(env))
    }

    async fn sync(a: &SharedEnv<MemStore>, b: &SharedEnv<MemStore>) -> (SyncStats, SyncStats) {
        let (a_io, b_io) = tokio::io::duplex(64 * 1024);
        let (a_r, a_w) = tokio::io::split(a_io);
        let (b_r, b_w) = tokio::io::split(b_io);
        let (sa, sb) = tokio::join!(
            sync_session(a, a_r, a_w, true),
            sync_session(b, b_r, b_w, false)
        );
        lock(a).catch_up().unwrap();
        lock(b).catch_up().unwrap();
        (sa.unwrap(), sb.unwrap())
    }

    fn all(env: &SharedEnv<MemStore>) -> Vec<Entry> {
        lock(env).entries(Query::ALL).unwrap()
    }

    fn state(env: &SharedEnv<MemStore>) -> String {
        lock(env).state_json("").unwrap().to_string()
    }

    #[tokio::test]
    async fn devices_converge() {
        let (a, b, c) = (env(1), env(2), env(3));
        // created on different devices at interleaved times, so every merge rolls back
        for i in 0..700u64 {
            let (target, kind) = match i % 3 {
                0 => (&a, "a"),
                1 => (&b, "bb"),
                _ => (&c, "ccc"),
            };
            lock(target)
                .add(kind.into(), vec![], Some(1000 + i))
                .unwrap();
        }

        let (sa, sb) = sync(&a, &b).await;
        assert_eq!(sa.received, sb.sent);
        assert_eq!(sa.received + sb.received, 700 / 3 * 2 + 1);
        sync(&b, &c).await;
        sync(&c, &a).await;

        assert_eq!(all(&a).len(), 700);
        assert_eq!(all(&a), all(&b));
        assert_eq!(all(&b), all(&c));
        assert_eq!(state(&a), state(&b));
        assert_eq!(state(&b), state(&c));
        assert_eq!(lock(&a).state_json("c").unwrap(), 700);

        // nothing left to exchange
        let (sa, sb) = sync(&a, &b).await;
        assert_eq!((sa.received, sb.received), (0, 0));
    }

    #[tokio::test]
    async fn edits_and_deletes_converge() {
        let (a, b) = (env(1), env(2));
        let x = lock(&a).add("x".into(), vec![], Some(10)).unwrap();
        let y = lock(&a).add("y".into(), vec![], Some(20)).unwrap();
        sync(&a, &b).await;

        // both edit `x` offline: each edit deletes it and adds a replacement, so both
        // replacements survive. b also deletes `y`.
        lock(&a).edit(&x.id, None, "from-a".into(), vec![]).unwrap();
        lock(&b).edit(&x.id, None, "from-b".into(), vec![]).unwrap();
        lock(&b).delete(&y.id).unwrap();
        sync(&b, &a).await;

        assert_eq!(all(&a), all(&b));
        assert_eq!(state(&a), state(&b));
        let kinds: Vec<String> = all(&a).into_iter().map(|e| e.kind).collect();
        assert_eq!(kinds.len(), 2);
        assert!(kinds.contains(&"from-a".to_string()) && kinds.contains(&"from-b".to_string()));
        assert!(lock(&a).is_deleted(&y.id).unwrap());
    }
}
