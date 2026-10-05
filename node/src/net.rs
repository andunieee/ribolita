//! Keeps programs' logs in sync with peers over iroh.
//!
//! One endpoint serves any number of programs: every sync session runs on its own bidirectional
//! stream and starts with a [`Hello`] naming the program, which the [`Host`] may refuse. Once a
//! peer and this node have synced a program, it's synced again whenever it changes locally
//! ([`Cmd::Sync`]) and every minute. Peers this node dialed are redialed if they disconnect.

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

use iroh::{
    Endpoint, EndpointAddr, PublicKey, SecretKey,
    endpoint::{Connection, RecvStream, SendStream, presets},
};
use ribolita::{ApplyReport, Store};
use ribolita_common::entry::Author;
use tokio::sync::{
    Mutex,
    mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
};

use crate::session::{
    Hello, HelloReply, SharedEnv, SyncStats, read_frame, sync_session, write_frame,
};

pub const ALPN: &[u8] = b"ribolita/sync/2";
const RESYNC_INTERVAL: Duration = Duration::from_secs(60);

/// Decides which programs a node offers, and to whom.
pub trait Host: Send + Sync + 'static {
    type Store: Store + Send + Sync + 'static;

    /// The environment of a program hosted here.
    fn env(&self, program: &str) -> Result<SharedEnv<Self::Store>, String>;

    /// Whether `peer`, presenting `token`, may sync `program`.
    fn authorize(&self, program: &str, peer: &PublicKey, token: Option<&str>) -> Result<(), String> {
        let _ = (program, peer, token);
        Ok(())
    }
}

/// Hosts a single program that any peer may sync, as devices of the same user do.
pub struct SingleProgram<S: Store + 'static> {
    pub program: String,
    pub env: SharedEnv<S>,
}

impl<S: Store + Send + Sync + 'static> Host for SingleProgram<S> {
    type Store = S;

    fn env(&self, program: &str) -> Result<SharedEnv<S>, String> {
        if program == self.program {
            Ok(self.env.clone())
        } else {
            Err(format!("program '{program}' isn't hosted here"))
        }
    }
}

pub enum Cmd {
    /// connect to a peer (a node id) and keep `program` in sync with it
    Connect {
        peer: String,
        program: String,
        token: Option<String>,
    },
    /// reconcile a program (or all of them) with every peer it's synced with, e.g. after a
    /// local change
    Sync { program: Option<String> },
}

pub enum Event {
    PeerConnected(String),
    PeerDisconnected(String),
    /// a session finished; the entries it brought in have been applied
    Synced {
        peer: String,
        program: String,
        received: usize,
        sent: usize,
        report: ApplyReport,
    },
    Error(String),
}

pub struct Handle {
    pub cmd_tx: UnboundedSender<Cmd>,
    pub node_id: String,
}

/// Loads the node's secret key from `dir`, creating it on first use.
pub fn load_or_create_secret(dir: &Path) -> anyhow::Result<SecretKey> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("iroh.key");
    if path.exists() {
        let bytes: [u8; 32] = std::fs::read(&path)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("{} must hold 32 bytes", path.display()))?;
        return Ok(SecretKey::from_bytes(&bytes));
    }
    let key = SecretKey::generate();
    std::fs::write(&path, key.to_bytes())?;
    Ok(key)
}

/// Entries created on this node are attributed to its public key.
pub fn author_of(secret: &SecretKey) -> Author {
    Author(*secret.public().as_bytes())
}

/// Starts the node on its own thread and runtime. Events are delivered on the returned channel.
pub fn spawn<H: Host>(host: H, secret: SecretKey) -> (Handle, mpsc::Receiver<Event>) {
    let (cmd_tx, cmd_rx) = unbounded_channel();
    let (evt_tx, evt_rx) = mpsc::channel();
    let node_id = secret.public().to_string();

    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("iroh tokio runtime");
        rt.block_on(run(host, secret, cmd_rx, evt_tx));
    });

    (Handle { cmd_tx, node_id }, evt_rx)
}

/// A program kept in sync with a peer.
#[derive(Clone)]
struct Link {
    token: Option<String>,
    /// we dialed the peer for it, so we redial when disconnected
    dial: bool,
}

struct Node<H: Host> {
    host: Arc<H>,
    endpoint: Endpoint,
    connections: Arc<Mutex<HashMap<PublicKey, Connection>>>,
    links: Arc<Mutex<HashMap<PublicKey, HashMap<String, Link>>>>,
    events: mpsc::Sender<Event>,
}

impl<H: Host> Clone for Node<H> {
    fn clone(&self) -> Self {
        Node {
            host: self.host.clone(),
            endpoint: self.endpoint.clone(),
            connections: self.connections.clone(),
            links: self.links.clone(),
            events: self.events.clone(),
        }
    }
}

impl<H: Host> Node<H> {
    fn error(&self, msg: String) {
        self.events.send(Event::Error(msg)).ok();
    }

    async fn link(&self, peer: PublicKey, program: &str, link: Link) {
        let mut links = self.links.lock().await;
        let programs = links.entry(peer).or_default();
        // a dialed link (with its token) is worth more than one the peer opened
        if link.dial || programs.get(program).is_none_or(|l| !l.dial) {
            programs.insert(program.to_string(), link);
        }
    }

    /// Registers a connection and answers the sessions the peer opens on it.
    async fn serve(self, conn: Connection) {
        let peer = conn.remote_id();
        self.connections.lock().await.insert(peer, conn.clone());
        self.events.send(Event::PeerConnected(peer.to_string())).ok();

        // sync what we have in common with this peer right away
        let links = self
            .links
            .lock()
            .await
            .get(&peer)
            .cloned()
            .unwrap_or_default();
        for (program, link) in links {
            tokio::spawn(self.clone().sync_with(conn.clone(), program, link.token));
        }

        while let Ok((send, recv)) = conn.accept_bi().await {
            tokio::spawn(self.clone().respond(peer, send, recv));
        }

        let mut connections = self.connections.lock().await;
        // only forget the connection if the peer didn't reconnect in the meantime
        if connections
            .get(&peer)
            .is_some_and(|c| c.stable_id() == conn.stable_id())
        {
            connections.remove(&peer);
        }
        drop(connections);
        self.events
            .send(Event::PeerDisconnected(peer.to_string()))
            .ok();
    }

    /// Looks up a program, authorizing `peer` if given.
    async fn open(
        &self,
        program: &str,
        peer: Option<(PublicKey, Option<String>)>,
    ) -> anyhow::Result<Result<SharedEnv<H::Store>, String>> {
        // opening may compile a module or open a store
        let host = self.host.clone();
        let program = program.to_string();
        Ok(tokio::task::spawn_blocking(move || {
            if let Some((peer, token)) = peer {
                host.authorize(&program, &peer, token.as_deref())?;
            }
            host.env(&program)
        })
        .await?)
    }

    /// Answers a session the peer opened.
    async fn respond(self, peer: PublicKey, mut send: SendStream, mut recv: RecvStream) {
        let result = async {
            let Some(hello) = read_frame::<Hello>(&mut recv).await? else {
                return Ok(None);
            };
            let env = match self.open(&hello.program, Some((peer, hello.token))).await? {
                Ok(env) => env,
                Err(error) => {
                    let reply = HelloReply {
                        error: Some(error.clone()),
                    };
                    write_frame(&mut send, &reply).await?;
                    send.finish().ok();
                    anyhow::bail!("refused to sync '{}': {error}", hello.program);
                }
            };
            write_frame(&mut send, &HelloReply { error: None }).await?;
            let link = Link {
                token: None,
                dial: false,
            };
            self.link(peer, &hello.program, link).await;
            let stats = sync_session(&env, recv, send, false).await?;
            anyhow::Ok(Some((hello.program, stats, env)))
        }
        .await;
        match result {
            Ok(Some((program, stats, env))) => self.finish(peer, program, stats, env).await,
            Ok(None) => {}
            Err(err) => self.error(format!("sync with {peer}: {err:#}")),
        }
    }

    /// Opens a sync session for `program` with a connected peer.
    async fn sync_with(self, conn: Connection, program: String, token: Option<String>) {
        let peer = conn.remote_id();
        let result = async {
            let env = self
                .open(&program, None)
                .await?
                .map_err(anyhow::Error::msg)?;
            let (mut send, mut recv) = conn.open_bi().await?;
            let hello = Hello {
                program: program.clone(),
                token,
            };
            write_frame(&mut send, &hello).await?;
            let reply = read_frame::<HelloReply>(&mut recv)
                .await?
                .ok_or_else(|| anyhow::anyhow!("peer closed the stream"))?;
            if let Some(error) = reply.error {
                anyhow::bail!("peer refused: {error}");
            }
            anyhow::Ok((sync_session(&env, recv, send, true).await?, env))
        }
        .await;
        match result {
            Ok((stats, env)) => self.finish(peer, program, stats, env).await,
            Err(err) => self.error(format!("sync '{program}' with {peer}: {err:#}")),
        }
    }

    /// Applies what a session brought in.
    async fn finish(
        &self,
        peer: PublicKey,
        program: String,
        stats: SyncStats,
        env: SharedEnv<H::Store>,
    ) {
        let report = if stats.received == 0 {
            Ok(Ok(ApplyReport::default()))
        } else {
            tokio::task::spawn_blocking(move || {
                env.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .catch_up()
            })
            .await
        };
        match report {
            Ok(Ok(report)) => {
                self.events
                    .send(Event::Synced {
                        peer: peer.to_string(),
                        program,
                        received: stats.received,
                        sent: stats.sent,
                        report,
                    })
                    .ok();
            }
            Ok(Err(err)) => self.error(format!("applying '{program}' entries from {peer}: {err}")),
            Err(err) => self.error(format!("applying '{program}' entries from {peer}: {err}")),
        }
    }

    /// Syncs `program` (or everything) with the peers it's linked with, redialing peers we
    /// dialed before.
    async fn sync(&self, program: Option<&str>) {
        let links = self.links.lock().await.clone();
        let connections = self.connections.lock().await.clone();
        for (peer, programs) in links {
            let wanted: Vec<(String, Link)> = programs
                .into_iter()
                .filter(|(p, _)| program.is_none_or(|wanted| wanted == p))
                .collect();
            match connections.get(&peer) {
                Some(conn) => {
                    for (program, link) in wanted {
                        tokio::spawn(self.clone().sync_with(conn.clone(), program, link.token));
                    }
                }
                // serving the new connection syncs every linked program
                None if wanted.iter().any(|(_, l)| l.dial) => {
                    tokio::spawn(self.clone().dial(peer));
                }
                None => {}
            }
        }
    }

    async fn dial(self, peer: PublicKey) {
        match self.endpoint.connect(EndpointAddr::from(peer), ALPN).await {
            Ok(conn) => self.serve(conn).await,
            Err(err) => self.error(format!("connect {peer}: {err}")),
        }
    }

    async fn connect(self, peer: String, program: String, token: Option<String>) {
        let peer = match peer.parse::<PublicKey>() {
            Ok(peer) => peer,
            Err(err) => return self.error(format!("bad peer id {peer}: {err}")),
        };
        let link = Link {
            token: token.clone(),
            dial: true,
        };
        self.link(peer, &program, link).await;
        let existing = self.connections.lock().await.get(&peer).cloned();
        match existing {
            Some(conn) => self.sync_with(conn, program, token).await,
            None => self.dial(peer).await,
        }
    }
}

async fn run<H: Host>(
    host: H,
    secret: SecretKey,
    mut cmd_rx: UnboundedReceiver<Cmd>,
    events: mpsc::Sender<Event>,
) {
    let endpoint = match Endpoint::builder(presets::N0)
        .secret_key(secret)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
    {
        Ok(endpoint) => endpoint,
        Err(err) => {
            events.send(Event::Error(format!("iroh bind: {err}"))).ok();
            return;
        }
    };
    let node = Node {
        host: Arc::new(host),
        endpoint,
        connections: Arc::default(),
        links: Arc::default(),
        events,
    };
    serve_incoming(&node);

    // the first tick would fire right away, racing the commands sent on startup
    let first = tokio::time::Instant::now() + RESYNC_INTERVAL;
    let mut resync = tokio::time::interval_at(first, RESYNC_INTERVAL);
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(Cmd::Connect { peer, program, token }) => {
                    tokio::spawn(node.clone().connect(peer, program, token));
                }
                Some(Cmd::Sync { program }) => node.sync(program.as_deref()).await,
                None => break,
            },
            _ = resync.tick() => node.sync(None).await,
        }
    }
}

fn serve_incoming<H: Host>(node: &Node<H>) {
    let node = node.clone();
    tokio::spawn(async move {
        while let Some(incoming) = node.endpoint.accept().await {
            // handshake in its own task so one slow peer doesn't block the others
            let node = node.clone();
            tokio::spawn(async move {
                match incoming.await {
                    Ok(conn) => node.serve(conn).await,
                    Err(err) => node.error(format!("accept: {err}")),
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ribolita::{MemStore, Query, RunEnvironment, testing::COUNTER_WAT};
    use std::net::{Ipv4Addr, SocketAddr};

    fn env(author: u8, entries: u64) -> SharedEnv<MemStore> {
        let mut env = RunEnvironment::new(
            std::path::Path::new(""),
            COUNTER_WAT.as_bytes(),
            Author([author; 32]),
        )
        .unwrap();
        for i in 0..entries {
            env.add("k".into(), vec![], Some(100 + i * 2 + author as u64))
                .unwrap();
        }
        Arc::new(std::sync::Mutex::new(env))
    }

    /// two programs; peers need a token for one of them if `require_token`
    struct TwoPrograms {
        open: SharedEnv<MemStore>,
        guarded: SharedEnv<MemStore>,
        require_token: bool,
    }

    impl Host for TwoPrograms {
        type Store = MemStore;

        fn env(&self, program: &str) -> Result<SharedEnv<MemStore>, String> {
            match program {
                "open" => Ok(self.open.clone()),
                "guarded" => Ok(self.guarded.clone()),
                _ => Err("unknown program".into()),
            }
        }

        fn authorize(&self, program: &str, _: &PublicKey, token: Option<&str>) -> Result<(), String> {
            match (program, token) {
                ("guarded", token) if self.require_token && token != Some("secret") => {
                    Err("bad token".into())
                }
                _ => Ok(()),
            }
        }
    }

    async fn node<H: Host>(host: H) -> (Node<H>, mpsc::Receiver<Event>) {
        let endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::generate())
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        let (events, rx) = mpsc::channel();
        let node = Node {
            host: Arc::new(host),
            endpoint,
            connections: Arc::default(),
            links: Arc::default(),
            events,
        };
        serve_incoming(&node);
        (node, rx)
    }

    fn count(env: &SharedEnv<MemStore>) -> usize {
        env.lock().unwrap().entries(Query::ALL).unwrap().len()
    }

    async fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn syncs_programs_over_iroh() {
        let (server_open, server_guarded) = (env(1, 30), env(1, 5));
        let (server, _server_events) = node(TwoPrograms {
            open: server_open.clone(),
            guarded: server_guarded.clone(),
            require_token: true,
        })
        .await;
        let (device_open, device_guarded) = (env(2, 40), env(2, 3));
        let (device, device_events) = node(TwoPrograms {
            open: device_open.clone(),
            guarded: device_guarded.clone(),
            require_token: false,
        })
        .await;

        // the device dials the server (by address, as there's no discovery in tests)
        let port = server.endpoint.bound_sockets()[0].port();
        let addr = EndpointAddr::from(server.endpoint.id())
            .with_ip_addr(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port));
        let conn = device.endpoint.connect(addr, ALPN).await.unwrap();
        let server_id = conn.remote_id();
        let open = Link {
            token: None,
            dial: true,
        };
        let wrong = Link {
            token: Some("wrong".into()),
            dial: true,
        };
        device.link(server_id, "open", open).await;
        device.link(server_id, "guarded", wrong).await;
        tokio::spawn(device.clone().serve(conn));

        wait_for("the open program to sync", || {
            count(&server_open) == 70 && count(&device_open) == 70
        })
        .await;
        wait_for("both to apply", || {
            server_open.lock().unwrap().state_json("c").unwrap() == 70
                && device_open.lock().unwrap().state_json("c").unwrap() == 70
        })
        .await;
        assert_eq!(
            server_open.lock().unwrap().state_json("").unwrap(),
            device_open.lock().unwrap().state_json("").unwrap()
        );

        // the wrong token was refused
        let device_events = tokio::task::spawn_blocking(move || {
            loop {
                match device_events.recv_timeout(Duration::from_secs(20)).unwrap() {
                    Event::Error(msg) if msg.contains("bad token") => break device_events,
                    _ => continue,
                }
            }
        })
        .await
        .unwrap();
        assert_eq!((count(&server_guarded), count(&device_guarded)), (5, 3));

        // with the right one it syncs; and changes made on the server are pushed back
        let connect = device
            .clone()
            .connect(server_id.to_string(), "guarded".into(), Some("secret".into()));
        tokio::spawn(connect);
        wait_for("the guarded program to sync", || {
            count(&server_guarded) == 8 && count(&device_guarded) == 8
        })
        .await;
        server_guarded
            .lock()
            .unwrap()
            .add("k".into(), vec![], Some(1))
            .unwrap();
        server.sync(Some("guarded")).await;
        wait_for("the server's change to reach the device", || {
            count(&device_guarded) == 9
        })
        .await;
        drop(device_events);
    }
}
