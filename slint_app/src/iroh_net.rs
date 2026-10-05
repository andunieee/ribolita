use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{mpsc::Receiver, mpsc::Sender, mpsc, Arc},
    thread,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use iroh::{
    endpoint::{Connection, presets},
    Endpoint, EndpointAddr, PublicKey, SecretKey,
};
use ribolita_common::entry::Entry;
use tokio::sync::Mutex;

const ALPN: &[u8] = b"ribolita/1";
const MAX_MSG: usize = 10 * 1024 * 1024;

// ---- Public API ----

pub enum Cmd {
    ConnectPeer(String),
    PublishEntry(Entry),
}

pub enum Event {
    NodeId(String),
    PeerConnected(String),
    PeerDisconnected(String),
    EntryReceived(Entry),
    Error(String),
}

pub struct Handle {
    pub cmd_tx: UnboundedSender<Cmd>,
    pub node_id: String,
}

pub fn spawn(data_dir: PathBuf) -> (Handle, Receiver<Event>) {
    let (cmd_tx, cmd_rx) = unbounded_channel();
    let (evt_tx, evt_rx) = mpsc::channel::<Event>();

    let key_dir = data_dir.join("iroh");
    std::fs::create_dir_all(&key_dir).ok();
    let key_path = key_dir.join("key");

    let secret = if key_path.exists() {
        let bytes = std::fs::read(&key_path).expect("iroh key read");
        let arr: [u8; 32] = bytes.as_slice().try_into().expect("iroh key 32 bytes");
        SecretKey::from_bytes(&arr)
    } else {
        let key = SecretKey::generate();
        std::fs::write(&key_path, key.to_bytes()).expect("iroh key write");
        key
    };

    let node_id = secret.public().to_string();

    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("iroh tokio rt");
        rt.block_on(run(secret, cmd_rx, evt_tx));
    });

    (Handle { cmd_tx, node_id }, evt_rx)
}

// ---- Internal ----

type Peers = Arc<Mutex<HashMap<String, Connection>>>;

async fn run(secret: SecretKey, mut cmd_rx: UnboundedReceiver<Cmd>, evt_tx: Sender<Event>) {
    let ep = match Endpoint::builder(presets::N0)
        .secret_key(secret)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
    {
        Ok(ep) => ep,
        Err(e) => {
            evt_tx.send(Event::Error(format!("iroh bind: {e}"))).ok();
            return;
        }
    };

    evt_tx.send(Event::NodeId(ep.id().to_string())).ok();

    let peers: Peers = Arc::new(Mutex::new(HashMap::new()));

    // Accept incoming connections
    eprintln!("iroh: starting accept loop");
    {
        let ep = ep.clone();
        let p = peers.clone();
        let evt = evt_tx.clone();
        tokio::spawn(async move {
            while let Some(incoming) = ep.accept().await {
                // handshake in its own task so one slow peer doesn't block the others
                let p = p.clone();
                let evt = evt.clone();
                tokio::spawn(async move {
                    match incoming.await {
                        Ok(conn) => {
                            let peer_id = conn.remote_id().to_string();
                            eprintln!("iroh accept: accepted connection from {peer_id}");
                            p.lock().await.insert(peer_id.clone(), conn.clone());
                            evt.send(Event::PeerConnected(peer_id.clone())).ok();
                            recv_loop(conn, peer_id, p, evt);
                        }
                        Err(e) => {
                            eprintln!("iroh accept: failed: {e}");
                            evt.send(Event::Error(format!("accept: {e}"))).ok();
                        }
                    }
                });
            }
        });
    }

    // Command loop
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            Cmd::ConnectPeer(peer_id) => {
                eprintln!("iroh cmd: connect to {peer_id}");
                let evt = evt_tx.clone();
                let ep = ep.clone();
                let p = peers.clone();
                tokio::spawn(async move {
                    connect_to(ep, &peer_id, evt, p).await;
                });
            }
            Cmd::PublishEntry(entry) => {
                eprintln!("iroh cmd: publish entry id={}", entry.id);
                broadcast_entry(&peers, &entry, &evt_tx).await;
            }
        }
    }
}

async fn connect_to(ep: Endpoint, peer_id: &str, evt_tx: Sender<Event>, peers: Peers) {
    eprintln!("iroh connect_to: parsing node id {peer_id}");
    let node_id = match peer_id.parse::<PublicKey>() {
        Ok(id) => id,
        Err(e) => {
            eprintln!("iroh connect_to: bad peer id {peer_id}: {e}");
            evt_tx.send(Event::Error(format!("bad peer id: {e}"))).ok();
            return;
        }
    };

    eprintln!("iroh connect_to: connecting to {node_id}");
    let addr = EndpointAddr::from(node_id);
    let conn = match ep.connect(addr, ALPN).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("iroh connect_to: connect failed: {e}");
            evt_tx.send(Event::Error(format!("connect {peer_id}: {e}"))).ok();
            return;
        }
    };

    let pid = conn.remote_id().to_string();
    eprintln!("iroh connect_to: connected to {pid}");
    {
        let mut map = peers.lock().await;
        map.insert(pid.clone(), conn.clone());
    }
    evt_tx.send(Event::PeerConnected(pid.clone())).ok();
    recv_loop(conn, pid, peers, evt_tx);
}

fn recv_loop(conn: Connection, peer_id: String, peers: Peers, evt_tx: Sender<Event>) {
    tokio::spawn(async move {
        loop {
            let mut recv = match conn.accept_uni().await {
                Ok(r) => r,
                Err(_) => {
                    peers.lock().await.remove(&peer_id);
                    evt_tx.send(Event::PeerDisconnected(peer_id)).ok();
                    return;
                }
            };
            let data = match recv.read_to_end(MAX_MSG).await {
                Ok(d) => d,
                Err(_) => {
                    peers.lock().await.remove(&peer_id);
                    evt_tx.send(Event::PeerDisconnected(peer_id)).ok();
                    return;
                }
            };
            if let Ok(entry) = rmp_serde::from_slice::<Entry>(&data) {
                evt_tx.send(Event::EntryReceived(entry)).ok();
            } else {
                evt_tx
                    .send(Event::Error(format!("bad entry from {peer_id}")))
                    .ok();
            }
        }
    });
}

async fn broadcast_entry(peers: &Peers, entry: &Entry, evt_tx: &Sender<Event>) {
    let data = match rmp_serde::to_vec(entry) {
        Ok(d) => d,
        Err(e) => {
            evt_tx.send(Event::Error(format!("serialize entry: {e}"))).ok();
            return;
        }
    };

    let conns = {
        let map = peers.lock().await;
        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>()
    };

    let mut dead = Vec::new();
    for (peer_id, conn) in &conns {
        match conn.open_uni().await {
            Ok(mut send) => {
                let sent = match send.write_all(&data).await {
                    Ok(()) => send.finish().map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                };
                if let Err(e) = sent {
                    eprintln!("send to {peer_id}: {e}");
                    dead.push(peer_id.clone());
                }
            }
            Err(e) => {
                eprintln!("open_uni to {peer_id}: {e}");
                dead.push(peer_id.clone());
            }
        }
    }
    if !dead.is_empty() {
        let mut map = peers.lock().await;
        for pid in dead {
            map.remove(&pid);
            evt_tx.send(Event::PeerDisconnected(pid)).ok();
        }
    }
}
